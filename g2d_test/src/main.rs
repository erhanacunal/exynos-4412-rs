//! G2D test. Fills, copies, scales and blends with the G2D (`drivers::g2d`), checks every pixel
//! of what it wrote against what it should be, and times each operation against the CPU doing
//! the same. Then a demo until reset: a scaled background and bouncing, half-transparent
//! sprites, drawn by the G2D alone into one of two framebuffers while the panel shows the other.
//!
//! The CPU sleeps while it waits for the G2D or a vsync (`exynos_4412_rs::sleep`); the demo's
//! lines give the share of time it slept.
//!
//! Keys: `+` and `-` the backlight's brightness, `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing, rgb};
use exynos_4412_rs::drivers::g2d::{self, Alpha, Filter, Format, G2d, Rect, Surface};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, print, println, sleep, soc};

exynos_4412_rs::entry!(main);

const PANEL: Timing = LTN101AL03;
const W: usize = PANEL.width;
const H: usize = PANEL.height;
const PIXELS: usize = W * H;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const TIMEOUT_US: u64 = 200_000;
/// The image scaled up: a quarter of the panel each way.
const SOURCE_W: usize = 320;
const SOURCE_H: usize = 200;
const SPRITE: usize = 128;
/// The device tree's `brightness-levels` (`exynos4412-mehmet.dts`, `backlight`).
const BRIGHTNESS: [u8; 10] = [0, 17, 32, 64, 96, 128, 160, 192, 224, 255];

const FILL_A: u32 = rgb(0x12, 0x34, 0x56);
const FILL_B: u32 = rgb(0xC0, 0x80, 0x40);
const FILL_C: u32 = rgb(0xFF, 0x00, 0xFF);
const BACKGROUND: u32 = rgb(20, 40, 200);

/// Images in `.bss`, inside the first 16 MiB of DRAM that the uncached view covers: `memory.x`
/// makes room. Each starts and ends on a whole cache line.
#[repr(align(4096))]
struct Store<T>(UnsafeCell<T>);

// SAFETY: `main` is the only user, and takes the one reference to each once.
unsafe impl<T> Sync for Store<T> {}

static SCREENS: Store<[[u32; PIXELS]; 2]> = Store(UnsafeCell::new([[0; PIXELS]; 2]));
static SOURCE: Store<[u32; SOURCE_W * SOURCE_H]> = Store(UnsafeCell::new([0; SOURCE_W * SOURCE_H]));
static SPRITE_STORE: Store<[u32; SPRITE * SPRITE]> = Store(UnsafeCell::new([0; SPRITE * SPRITE]));

fn elapsed_us(mct: Mct, start: u64) -> u64 {
    (mct.count() - start) / COUNTS_PER_US
}

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "MISMATCH" }
}

fn key(level: &mut usize) {
    handle_key(level, console::uart().try_read_byte());
}

fn handle_key(level: &mut usize, key: Option<u8>) {
    match key {
        Some(b'r') => power::restart(BootMode::Normal),
        Some(b'f') => power::restart(BootMode::Fastboot),
        Some(key @ (b'+' | b'=' | b'-')) => {
            *level = if key == b'-' { level.saturating_sub(1).max(1) } else { (*level + 1).min(BRIGHTNESS.len() - 1) };
            display::set_brightness(BRIGHTNESS[*level]);
            println!("Brightness {}", BRIGHTNESS[*level]);
        }
        _ => {}
    }
}

/// One line of what woke the CPU since the last one: interrupt ID x times.
fn print_wakes() {
    print!("Wakes:");
    sleep::take_wakes(|id, count| match id {
        Some(id) => print!(" {}x{}", id, count),
        None => print!(" none x{}", count),
    });
    println!();
}

fn halt(what: &str) -> ! {
    println!("{} (MISMATCH); keys: r restart, f fastboot", what);
    console::flush();
    let mut level = 5;
    loop {
        handle_key(&mut level, sleep::read_key(Mct::new(), None));
    }
}

/// Starts an operation, waits for its end and returns how long it took, from the first
/// register written to the end.
fn timed(mct: Mct, g2d: G2d, op: impl FnOnce() -> Result<(), g2d::Error>) -> u64 {
    let start = mct.count();
    if let Err(error) = op().and_then(|()| g2d.wait(mct, TIMEOUT_US)) {
        let (pending, fifo) = g2d.status();
        println!("G2D ERROR {:?}, INTC_PEND {:#x} FIFO_STAT {:#x}", error, pending, fifo);
        halt("G2D failed");
    }
    elapsed_us(mct, start)
}

/// Megabytes per second for `bytes` in `us`.
fn rate(bytes: usize, us: u64) -> u64 {
    bytes as u64 / us.max(1)
}

fn channel(pixel: u32, shift: u32) -> i32 {
    ((pixel >> shift) & 0xFF) as i32
}

/// The largest difference between two pixels' colours.
fn distance(a: u32, b: u32) -> i32 {
    [16, 8, 0].iter().map(|&shift| (channel(a, shift) - channel(b, shift)).abs()).max().unwrap_or(0)
}

/// `source` over `target` by `alpha` (0 to 255), straight colours, rounded.
fn over(source: u32, target: u32, alpha: u32) -> u32 {
    let mix = |shift: u32| {
        let (s, d) = ((source >> shift) & 0xFF, (target >> shift) & 0xFF);
        ((s * alpha + d * (255 - alpha) + 127) / 255) << shift
    };
    mix(16) | mix(8) | mix(0)
}

fn source_pixel(x: usize, y: usize) -> u32 {
    rgb((x * 255 / (SOURCE_W - 1)) as u8, (y * 255 / (SOURCE_H - 1)) as u8, 128)
}

/// A soft-edged disc, straight alpha: opaque inside, fading over its last 20 pixels.
fn sprite_pixel(x: usize, y: usize) -> u32 {
    let (dx, dy) = (x as i32 * 2 - 127, y as i32 * 2 - 127);
    let d2 = (dx * dx + dy * dy) as u32; // (2 * distance)^2
    let alpha = match d2 {
        _ if d2 < 88 * 88 => 255,
        _ if d2 < 128 * 128 => (128 - isqrt(d2)) * 255 / 40,
        _ => 0,
    };
    alpha << 24 | rgb(255, (x * 2) as u8, (y * 2) as u8)
}

fn isqrt(value: u32) -> u32 {
    let mut root = 0u32;
    while (root + 1) * (root + 1) <= value {
        root += 1;
    }
    root
}

fn draw(buffer: &mut Framebuffer, pixel: fn(usize, usize) -> u32) {
    let mut line = [0u32; W];
    let width = buffer.width();
    for y in 0..buffer.height() {
        for (x, out) in line[..width].iter_mut().enumerate() {
            *out = pixel(x, y);
        }
        buffer.write_span(0, y, &line[..width]);
    }
}

fn screen_pattern(x: usize, y: usize) -> u32 {
    rgb(x as u8, y as u8, (x ^ y) as u8)
}

/// Pixels of `buffer` whose colour differs from `expected`. The top byte is left out: the G2D
/// writes it as 0xFF in XRGB, where the CPU's `rgb` has 0.
fn wrong(buffer: &Framebuffer, expected: impl Fn(usize, usize) -> u32) -> usize {
    let width = buffer.width();
    buffer.pixels().iter().enumerate().filter(|&(i, &p)| (p ^ expected(i % width, i / width)) & 0xFF_FFFF != 0).count()
}

fn inside(rect: Rect, x: usize, y: usize) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

struct Images {
    screens: [Framebuffer; 2],
    source: Framebuffer,
    sprite: Framebuffer,
}

fn images() -> Images {
    // SAFETY: the only references to these stores, taken once: see `Store`.
    let (screens, source, sprite) =
        unsafe { (&mut *SCREENS.0.get(), &mut *SOURCE.0.get(), &mut *SPRITE_STORE.0.get()) };
    let wrap = |pixels: &'static mut [u32], width, height| {
        let buffer = Framebuffer::new(pixels, width, height).unwrap_or_else(|| halt("Image too small"));
        if !buffer.is_uncached() {
            halt("Image not uncached");
        }
        buffer
    };
    let [first, second] = screens;
    Images {
        screens: [wrap(first, W, H), wrap(second, W, H)],
        source: wrap(source, SOURCE_W, SOURCE_H),
        sprite: wrap(sprite, SPRITE, SPRITE),
    }
}

fn start_panel(mct: Mct, first: &mut Framebuffer) {
    let fimd = Fimd::new();
    if let Err(error) = fimd.power_on() {
        println!("Panel ERROR {:?}", error);
        halt("No panel");
    }
    display::configure_pins();
    first.fill_rect(0, 0, W, H, 0);
    first.flush();
    if let Err(error) = fimd.start(&PANEL, first) {
        println!("Panel ERROR {:?}", error);
        halt("No panel");
    }
    display::set_lvds(true);
    let end = mct.count() + u64::from(LVDS_SETTLE_MS) * 1000 * COUNTS_PER_US;
    sleep::until_count(mct, end);
    display::set_brightness(BRIGHTNESS[5]);
}

fn test_fill(mct: Mct, g2d: G2d, screen: &mut Framebuffer) {
    let surface = Surface::of(screen, Format::Xrgb8888);
    let bytes = PIXELS * 4;
    let us = timed(mct, g2d, || g2d.fill(&surface, Rect::all(&surface), FILL_A));
    let bad = wrong(screen, |_, _| FILL_A);
    println!("Pixel written {:#010x}, asked {:#010x}", screen.pixels()[0], FILL_A);
    let start = mct.count();
    screen.fill_rect(0, 0, W, H, FILL_B);
    screen.flush();
    let cpu = elapsed_us(mct, start);
    println!(
        "Fill {}x{}: G2D {} us ({} MB/s), CPU {} us ({} MB/s); {} wrong ({})",
        W,
        H,
        us,
        rate(bytes, us),
        cpu,
        rate(bytes, cpu),
        bad,
        verdict(bad == 0)
    );

    // Its edges: right and bottom exclusive.
    let rect = Rect::new(100, 50, 200, 100);
    let us = timed(mct, g2d, || g2d.fill(&surface, rect, FILL_C));
    let bad = wrong(screen, |x, y| if inside(rect, x, y) { FILL_C } else { FILL_B });
    println!("Fill 200x100 at 100,50: {} us; {} wrong ({})", us, bad, verdict(bad == 0));
}

fn test_copy(mct: Mct, g2d: G2d, images: &mut Images) {
    let [first, second] = &mut images.screens;
    draw(second, screen_pattern);
    let (to, from) = (Surface::of(first, Format::Xrgb8888), Surface::of(second, Format::Xrgb8888));
    let us = timed(mct, g2d, || g2d.copy(&from, Rect::all(&from), &to, Rect::all(&to), Filter::Nearest));
    let bad = wrong(first, screen_pattern);
    let start = mct.count();
    for y in 0..H {
        first.write_span(0, y, &second.pixels()[y * W..(y + 1) * W]);
    }
    first.flush();
    let cpu = elapsed_us(mct, start);
    println!(
        "Copy {}x{}: G2D {} us ({} MB/s), CPU {} us ({} MB/s); {} wrong ({})",
        W,
        H,
        us,
        rate(PIXELS * 4, us),
        cpu,
        rate(PIXELS * 4, cpu),
        bad,
        verdict(bad == 0)
    );

    // A rectangle elsewhere, onto a fill.
    let (rect, at) = (Rect::new(10, 20, 300, 200), Rect::new(500, 300, 300, 200));
    timed(mct, g2d, || g2d.fill(&to, Rect::all(&to), FILL_A));
    let us = timed(mct, g2d, || g2d.copy(&from, rect, &to, at, Filter::Nearest));
    let bad = wrong(first, |x, y| if inside(at, x, y) { screen_pattern(x - 490, y - 280) } else { FILL_A });
    println!("Copy 300x200 from 10,20 to 500,300: {} us; {} wrong ({})", us, bad, verdict(bad == 0));
}

fn test_scale(mct: Mct, g2d: G2d, images: &mut Images) {
    draw(&mut images.source, source_pixel);
    let screen = &mut images.screens[0];
    let (to, from) = (Surface::of(screen, Format::Xrgb8888), Surface::of(&images.source, Format::Xrgb8888));
    let (sx, sy) = (W / SOURCE_W, H / SOURCE_H);
    let us = timed(mct, g2d, || g2d.copy(&from, Rect::all(&from), &to, Rect::all(&to), Filter::Nearest));
    let bad = wrong(screen, |x, y| source_pixel(x / sx, y / sy));
    let start = mct.count();
    let mut line = [0u32; W];
    let source = images.source.pixels();
    for y in 0..H {
        let row = &source[(y / sy) * SOURCE_W..][..SOURCE_W];
        for (x, out) in line.iter_mut().enumerate() {
            *out = row[x / sx];
        }
        screen.write_span(0, y, &line);
    }
    screen.flush();
    let cpu = elapsed_us(mct, start);
    println!(
        "Scale {}x{} to {}x{}, nearest: G2D {} us, CPU {} us; {} wrong ({})",
        SOURCE_W,
        SOURCE_H,
        W,
        H,
        us,
        cpu,
        bad,
        verdict(bad == 0)
    );

    // Bilinear: the source is a ramp each way, so every result lies within the neighbouring
    // source pixels' colours, whatever the sampling phase.
    let us = timed(mct, g2d, || g2d.copy(&from, Rect::all(&from), &to, Rect::all(&to), Filter::Bilinear));
    let mut bad = 0;
    for (i, &pixel) in screen.pixels().iter().enumerate() {
        let (cx, cy) = ((i % W) / sx, (i / W) / sy);
        let (lo, hi) = (source_pixel(cx.saturating_sub(1), cy.saturating_sub(1)), source_pixel((cx + 1).min(SOURCE_W - 1), (cy + 1).min(SOURCE_H - 1)));
        let outside = [16, 8, 0].iter().any(|&shift| {
            let c = channel(pixel, shift);
            c < channel(lo, shift).min(channel(hi, shift)) - 1 || c > channel(lo, shift).max(channel(hi, shift)) + 1
        });
        bad += usize::from(outside);
    }
    println!("Scale to {}x{}, bilinear: G2D {} us; {} out of range ({})", W, H, us, bad, verdict(bad == 0));
}

/// Blends the sprite at `at` with `global`, then checks the square and a 2-pixel ring around it.
fn check_blend(mct: Mct, g2d: G2d, images: &Images, at: (usize, usize), global: u8) -> (u64, i32, usize) {
    let screen = &images.screens[0];
    let (to, from) = (Surface::of(screen, Format::Xrgb8888), Surface::of(&images.sprite, Format::Argb8888));
    let rect = Rect::new(at.0, at.1, SPRITE, SPRITE);
    let us = timed(mct, g2d, || g2d.blend(&from, Rect::all(&from), Alpha::Straight, &to, rect, Filter::Nearest, global));
    let (mut worst, mut bad) = (0, 0);
    let pixels = screen.pixels();
    for y in at.1 - 2..at.1 + SPRITE + 2 {
        for x in at.0 - 2..at.0 + SPRITE + 2 {
            let expected = if inside(rect, x, y) {
                let sprite = sprite_pixel(x - at.0, y - at.1);
                over(sprite, BACKGROUND, (sprite >> 24) * u32::from(global) / 255)
            } else {
                BACKGROUND
            };
            let error = distance(pixels[y * W + x], expected);
            worst = worst.max(error);
            bad += usize::from(error > 3);
        }
    }
    (us, worst, bad)
}

fn test_blend(mct: Mct, g2d: G2d, images: &mut Images) {
    draw(&mut images.sprite, sprite_pixel);
    let to = Surface::of(&images.screens[0], Format::Xrgb8888);
    timed(mct, g2d, || g2d.fill(&to, Rect::all(&to), BACKGROUND));
    for (at, global) in [((200, 100), 255), ((400, 100), 128)] {
        let (us, worst, bad) = check_blend(mct, g2d, images, at, global);
        println!(
            "Blend {}x{} ARGB, global alpha {}: G2D {} us; worst error {}, {} off by more than 3 ({})",
            SPRITE,
            SPRITE,
            global,
            us,
            worst,
            bad,
            verdict(bad == 0)
        );
    }

    // The CPU, the same blend, elsewhere.
    let screen = &mut images.screens[0];
    let start = mct.count();
    let mut line = [0u32; SPRITE];
    let sprite = images.sprite.pixels();
    for y in 0..SPRITE {
        let row = (300 + y) * W + 200;
        line.copy_from_slice(&screen.pixels()[row..row + SPRITE]);
        for (x, out) in line.iter_mut().enumerate() {
            let pixel = sprite[y * SPRITE + x];
            *out = over(pixel, *out, pixel >> 24);
        }
        screen.write_span(200, 300 + y, &line);
    }
    screen.flush();
    println!("Blend {}x{} ARGB: CPU {} us", SPRITE, SPRITE, elapsed_us(mct, start));

    // Scaled up 3 times, bilinear: judged by eye, right of the others.
    let from = Surface::of(&images.sprite, Format::Argb8888);
    let rect = Rect::new(640, 100, 3 * SPRITE, 3 * SPRITE);
    let us = timed(mct, g2d, || g2d.blend(&from, Rect::all(&from), Alpha::Straight, &to, rect, Filter::Bilinear, 255));
    println!("Blend {}x{} scaled to {}x{}, bilinear: G2D {} us", SPRITE, SPRITE, rect.width, rect.height, us);

    // A whole screen over another at half, the colour-only case.
    let [first, second] = &mut images.screens;
    draw(second, screen_pattern);
    timed(mct, g2d, || g2d.fill(&to, Rect::all(&to), FILL_A));
    let from = Surface::of(second, Format::Xrgb8888);
    let us = timed(mct, g2d, || g2d.blend(&from, Rect::all(&from), Alpha::Straight, &to, Rect::all(&to), Filter::Nearest, 128));
    let (mut worst, mut bad) = (0, 0);
    for (i, &pixel) in first.pixels().iter().enumerate() {
        let error = distance(pixel, over(screen_pattern(i % W, i / W), FILL_A, 128));
        worst = worst.max(error);
        bad += usize::from(error > 3);
    }
    let start = mct.count();
    for y in 0..H {
        let mut row = [0u32; W];
        row.copy_from_slice(&second.pixels()[y * W..(y + 1) * W]);
        for out in row.iter_mut() {
            *out = (*out >> 1 & 0x7F_7F7F) + (FILL_A >> 1 & 0x7F_7F7F);
        }
        first.write_span(0, y, &row);
    }
    first.flush();
    let cpu = elapsed_us(mct, start);
    println!(
        "Blend {}x{} at half: G2D {} us, CPU {} us; worst error {}, {} off by more than 3 ({})",
        W,
        H,
        us,
        cpu,
        worst,
        bad,
        verdict(bad == 0)
    );
}

#[derive(Clone, Copy)]
struct Ball {
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    size: usize,
    alpha: u8,
}

impl Ball {
    fn step(&mut self) {
        let (right, bottom) = ((W - self.size) as i32, (H - self.size) as i32);
        self.x += self.dx;
        self.y += self.dy;
        if self.x < 0 || self.x > right {
            self.dx = -self.dx;
            self.x = self.x.clamp(0, right);
        }
        if self.y < 0 || self.y > bottom {
            self.dy = -self.dy;
            self.y = self.y.clamp(0, bottom);
        }
    }
}

/// The demo: each frame, the scaled background and the sprites by the G2D into the buffer not
/// on the panel, which then takes it at the next vsync.
fn demo(mct: Mct, g2d: G2d, images: &mut Images) -> ! {
    let fimd = Fimd::new();
    let surfaces = images.screens.each_ref().map(|screen| Surface::of(screen, Format::Xrgb8888));
    let source = Surface::of(&images.source, Format::Xrgb8888);
    let sprite = Surface::of(&images.sprite, Format::Argb8888);
    let mut balls = [Ball { x: 0, y: 0, dx: 0, dy: 0, size: 0, alpha: 0 }; 12];
    for (i, ball) in balls.iter_mut().enumerate() {
        let i = i as i32;
        *ball = Ball {
            x: 90 * i,
            y: 50 * i,
            dx: 3 + i % 5,
            dy: 2 + i % 4,
            size: [64, 96, 128, 192, 256][i as usize % 5],
            alpha: [255, 192, 128][i as usize % 3],
        };
    }
    sleep::take_wakes(|_, _| {});
    println!("Demo: {} sprites; keys: + - brightness, r restart, f fastboot", balls.len());
    console::flush();
    let (mut front, mut frames, mut total, mut slowest) = (0, 0u32, 0u64, 0u64);
    let mut report = mct.count() + 2 * u64::from(soc::MCT_CLOCK_HZ);
    let mut slept = sleep::slept();
    let mut level = 5;
    loop {
        let back = 1 - front;
        let target = &surfaces[back];
        let start = mct.count();
        timed(mct, g2d, || g2d.copy(&source, Rect::all(&source), target, Rect::all(target), Filter::Bilinear));
        for ball in &mut balls {
            ball.step();
            let rect = Rect::new(ball.x as usize, ball.y as usize, ball.size, ball.size);
            let filter = if ball.size == SPRITE { Filter::Nearest } else { Filter::Bilinear };
            timed(mct, g2d, || g2d.blend(&sprite, Rect::all(&sprite), Alpha::Straight, target, rect, filter, ball.alpha));
        }
        let us = elapsed_us(mct, start);
        (total, slowest, frames) = (total + us, slowest.max(us), frames + 1);
        let shown = fimd.show(&images.screens[back]).and_then(|()| fimd.wait_shown(&images.screens[back]));
        if let Err(error) = shown {
            println!("Display ERROR {:?}", error);
            halt("Display failed");
        }
        front = back;
        if mct.count() >= report {
            let idle = (sleep::slept() - slept) * 100 / (2 * u64::from(soc::MCT_CLOCK_HZ));
            println!("{} frames/s; draw avg/max {}/{} us; idle {}%", frames / 2, total / u64::from(frames), slowest, idle);
            print_wakes();
            (frames, total, slowest, slept) = (0, 0, 0, sleep::slept());
            report += 2 * u64::from(soc::MCT_CLOCK_HZ);
        }
        key(&mut level);
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nG2D test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let mut images = images();
    // The CPU's fill once before anything sleeps, to compare with the one in `test_fill`.
    let start = mct.count();
    images.screens[1].fill_rect(0, 0, W, H, FILL_B);
    images.screens[1].flush();
    println!("CPU fill before any sleep: {} us", elapsed_us(mct, start));
    start_panel(mct, &mut images.screens[0]);
    let g2d = G2d::new();
    let hz = g2d.power_on(g2d::DEFAULT_HZ);
    let cmu = Cmu::new();
    let [src, div, gate] = cmu.g2d_clocks();
    println!(
        "sclk_fimg2d {} Hz (reads {:?}); SRC_DMC {:#010x} DIV_DMC1 {:#010x} GATE_IP_DMC {:#010x}",
        hz,
        cmu.sclk_fimg2d_hz(),
        src,
        div,
        gate
    );
    console::flush();

    test_fill(mct, g2d, &mut images.screens[0]);
    console::flush();
    test_copy(mct, g2d, &mut images);
    console::flush();
    test_scale(mct, g2d, &mut images);
    console::flush();
    test_blend(mct, g2d, &mut images);
    console::flush();
    demo(mct, g2d, &mut images);
}
