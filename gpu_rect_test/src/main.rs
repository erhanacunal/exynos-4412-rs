//! Rectangles filled by the Mali-400's pixel processors: a clear frame written back at a corner
//! of the screen, with the screen's stride. A PP writes whole tiles from an 8-byte boundary, so
//! it gets the part of the rectangle `Rect::split_for_gpu` leaves it, and the CPU the edges.
//!
//! 1. The GPU and the screen on, as in `gpu_clear_test`; the screen grey.
//! 2. Rectangles whose corner falls on 4, 8, 16, 32 and 64-byte boundaries, and whose sides are
//!    and are not multiples of a tile, one at the bottom right corner of the screen. For each,
//!    every pixel of the screen checked: inside the rectangle its colour, outside still grey;
//!    what changed outside is reported as a box.
//! 3. If all hold: 200 random rectangles split so, then the same from the CPU alone, timed; each
//!    way through the cached screen, then through its uncached view (`mmu::DRAM_UNCACHED`),
//!    which needs no cache maintenance; at 160 MHz, then at 266 MHz.
//!
//! Keys afterwards: `d` the random rectangles again, `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, rgb};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mali::{self, IdentityMap, Mali, PpFrame, Rect, TileEnd};
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const WIDTH: usize = 1280;
const HEIGHT: usize = 800;
const TIMEOUT_US: u64 = 100_000;
const STREAM_WORDS: usize = (WIDTH / mali::TILE) * (HEIGHT / mali::TILE) * 4 + 64;
const RANDOM_RECTS: usize = 200;

const GREY: u32 = rgb(64, 64, 64);
const FILL: u32 = 0xFF00_C0FF;

/// Corner and size of each case: the corner at byte boundaries of 4 to 64 and the sides on and
/// off tiles.
const CASES: [(usize, usize, usize, usize); 11] = [
    (0, 0, 64, 64),
    (64, 32, 128, 48),
    (16, 100, 64, 64),
    (8, 200, 64, 64),
    (4, 300, 64, 64),
    (2, 400, 64, 64),
    (1, 500, 64, 64),
    (300, 40, 50, 30),
    (513, 333, 101, 77),
    (1200, 700, 80, 100),
    (1203, 777, 77, 23),
];

#[repr(C, align(4096))]
struct Pixels(UnsafeCell<[u32; WIDTH * HEIGHT]>);

// SAFETY: `main` takes the one reference to it once, for the framebuffer.
unsafe impl Sync for Pixels {}

static PIXELS: Pixels = Pixels(UnsafeCell::new([0; WIDTH * HEIGHT]));

#[repr(C, align(4096))]
struct Shared {
    streams: [[u32; STREAM_WORDS]; mali::PP_COUNT],
    render_state: [u32; 16],
}

struct Store(UnsafeCell<Shared>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Store {}

static STORE: Store = Store(UnsafeCell::new(Shared { streams: [[0; STREAM_WORDS]; mali::PP_COUNT], render_state: [0; 16] }));

struct Tables(UnsafeCell<IdentityMap<5>>);

// SAFETY: as for `Store`.
unsafe impl Sync for Tables {}

static TABLES: Tables = Tables(UnsafeCell::new(IdentityMap::new()));

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

fn us_since(mct: Mct, begin: u64) -> u64 {
    (mct.count() - begin) / COUNTS_PER_US
}

fn wait_keys_only() -> ! {
    console::flush();
    let uart = console::uart();
    loop {
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => core::hint::spin_loop(),
        }
    }
}

struct Random(u32);

impl Random {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

/// Writes `rect` of the screen through its uncached view: what the CPU writes there is in DRAM
/// when the next access starts, for the GPU and the FIMD alike.
fn fill_uncached(buffer: &Framebuffer, rect: Rect, color: u32) {
    let base = mmu::uncached(buffer.address()).expect("the screen is in the uncached view");
    for row in rect.y..rect.y + rect.height {
        let start = (base + (row * WIDTH + rect.x) * 4) as *mut u32;
        // SAFETY: inside the screen, `rect` being inside it; the cached view of these pixels is
        // clean and the CPU does not use it while this view is in use: see `race`.
        unsafe { core::slice::from_raw_parts_mut(start, rect.width).fill(color) };
    }
}

/// Where the time of [`fill`] goes, in MCT counts.
#[derive(Clone, Copy, Default)]
struct Parts {
    cpu_edges: u64,
    streams: u64,
    gpu: u64,
    invalidate: u64,
}

/// Fills `rect` with `color`: the edges from the CPU, the rest from the GPU on as many PPs as it
/// has tiles, up to four. Returns the microseconds of the GPU's frame, 0 without one.
///
/// The screen is kept clean in the caches: the CPU writes it only through the framebuffer, and
/// flushes what it wrote. So nothing dirty can land on what the GPU writes, and afterwards only
/// the lines the GPU wrote are dropped, so that the CPU reads what it wrote. The lines at the
/// GPU part's sides that it shares with CPU pixels are clean too, so dropping them loses nothing.
///
/// With `uncached`, the CPU writes the edges through the uncached view instead, and nothing is
/// dropped after the GPU.
#[allow(clippy::too_many_arguments)]
fn fill(gpu: Mali, mct: Mct, shared: &mut Shared, buffer: &mut Framebuffer, rect: Rect, color: u32, parts: &mut Parts, uncached: bool) -> Result<u64, mali::Error> {
    let split = rect.split_for_gpu();
    let begin = mct.count();
    for part in split.cpu.iter().flatten() {
        if uncached {
            fill_uncached(buffer, *part, color);
        } else {
            buffer.fill_rect(part.x, part.y, part.width, part.height, color);
            buffer.flush_rect(part.x, part.y, part.width, part.height);
        }
    }
    parts.cpu_edges += mct.count() - begin;
    let Some(part) = split.gpu else {
        return Ok(0);
    };
    let us = gpu_fill(gpu, mct, shared, buffer, part, color, parts)?;
    if uncached {
        return Ok(us);
    }
    let begin = mct.count();
    let stride = buffer.stride();
    for row in part.y..part.y + part.height {
        let first = buffer.address() + row * stride + part.x * 4;
        let start = first & !(mmu::DCACHE_LINE - 1);
        let end = (first + part.width * 4).next_multiple_of(mmu::DCACHE_LINE);
        mmu::invalidate_dcache_range(start, end - start);
    }
    parts.invalidate += mct.count() - begin;
    Ok(us)
}

/// Fills `rect`, as [`Rect::split_for_gpu`] leaves it to the GPU, with `color`, on as many PPs as
/// there are tiles, up to four. Returns the microseconds of the frame.
fn gpu_fill(gpu: Mali, mct: Mct, shared: &mut Shared, buffer: &Framebuffer, rect: Rect, color: u32, parts: &mut Parts) -> Result<u64, mali::Error> {
    let Rect { x, y, width, height } = rect;
    let stride = buffer.stride();
    let corner = buffer.address() + y * stride + x * 4;
    let tiles = width.div_ceil(mali::TILE) * height.div_ceil(mali::TILE);
    let pps = tiles.min(mali::PP_COUNT);
    let streams_begin = mct.count();
    for pp in 0..pps {
        let words = mali::clear_stream(&mut shared.streams[pp], width, height, pp, pps, TileEnd::Bare);
        // Only what the PP reads.
        mmu::clean_dcache_range(shared.streams[pp].as_ptr() as usize, words * 4);
    }
    parts.streams += mct.count() - streams_begin;
    let frame = PpFrame::clear(width, height, color, corner, stride, shared.render_state.as_ptr() as usize);
    let begin = mct.count();
    for pp in 0..pps {
        gpu.start_pp(pp, &frame, shared.streams[pp].as_ptr() as u32)?;
    }
    for pp in 0..pps {
        gpu.wait_pp(pp, mct, TIMEOUT_US)?;
    }
    parts.gpu += mct.count() - begin;
    Ok(us_since(mct, begin))
}

/// Inside: pixels not `color`. Outside: pixels not grey, and the box around them.
struct Check {
    inside_wrong: usize,
    outside_changed: usize,
    spill: Option<(usize, usize, usize, usize)>,
}

fn check(buffer: &Framebuffer, rect: Rect, color: u32) -> Check {
    let Rect { x, y, width, height } = rect;
    let mut result = Check { inside_wrong: 0, outside_changed: 0, spill: None };
    for (index, &pixel) in buffer.pixels().iter().enumerate() {
        let (px, py) = (index % WIDTH, index / WIDTH);
        let inside = px >= x && px < x + width && py >= y && py < y + height;
        if inside {
            result.inside_wrong += usize::from(pixel != color);
        } else if pixel != GREY {
            result.outside_changed += 1;
            let (left, top, right, bottom) = result.spill.unwrap_or((px, py, px, py));
            result.spill = Some((left.min(px), top.min(py), right.max(px), bottom.max(py)));
        }
    }
    result
}

fn recover(gpu: Mali, mct: Mct, error: mali::Error) {
    println!("  ERROR {:?}", error);
    for pp in 0..mali::PP_COUNT {
        if let Err(error) = gpu.hard_reset_pp(pp, mct) {
            println!("  PP{} hard reset ERROR {:?}", pp, error);
        }
    }
}

/// Step 2: every case, the screen grey again after each.
fn cases(gpu: Mali, mct: Mct, shared: &mut Shared, buffer: &mut Framebuffer) -> bool {
    let mut good = true;
    for (x, y, width, height) in CASES {
        let rect = Rect { x, y, width, height };
        let corner = buffer.address() + y * buffer.stride() + x * 4;
        let gpu_part = rect.split_for_gpu().gpu;
        match fill(gpu, mct, shared, buffer, rect, FILL, &mut Parts::default(), false) {
            Ok(us) => {
                let result = check(buffer, rect, FILL);
                let fine = result.inside_wrong == 0 && result.outside_changed == 0;
                let (gpu_w, gpu_h) = gpu_part.map_or((0, 0), |part| (part.width, part.height));
                println!(
                    "{}x{} at {},{} (corner on {} B), GPU {}x{} in {} us; inside wrong {}, outside changed {} in {:?}: {}",
                    width,
                    height,
                    x,
                    y,
                    1 << corner.trailing_zeros().min(12),
                    gpu_w,
                    gpu_h,
                    us,
                    result.inside_wrong,
                    result.outside_changed,
                    result.spill,
                    verdict(fine)
                );
                good &= fine;
            }
            Err(error) => {
                println!("{}x{} at {},{}", width, height, x, y);
                recover(gpu, mct, error);
                good = false;
            }
        }
        console::flush();
        buffer.fill_rect(0, 0, WIDTH, HEIGHT, GREY);
        buffer.flush();
    }
    good
}

fn random_rect(random: &mut Random) -> Rect {
    let width = 16 + random.next() as usize % 400;
    let height = 16 + random.next() as usize % 300;
    let x = random.next() as usize % (WIDTH - width);
    let y = random.next() as usize % (HEIGHT - height);
    Rect { x, y, width, height }
}

/// One way of step 3: the GPU with CPU edges, then the CPU alone, the same rectangles.
fn race_one(gpu: Mali, mct: Mct, shared: &mut Shared, buffer: &mut Framebuffer, seed: u32, uncached: bool) -> bool {
    let mut random = Random(seed);
    let mut pixels = 0;
    let mut parts = Parts::default();
    let begin = mct.count();
    for _ in 0..RANDOM_RECTS {
        let rect = random_rect(&mut random);
        pixels += rect.width * rect.height;
        let color = random.next() | 0xFF00_0000;
        if let Err(error) = fill(gpu, mct, shared, buffer, rect, color, &mut parts, uncached) {
            recover(gpu, mct, error);
            return false;
        }
    }
    let gpu_us = us_since(mct, begin);

    let mut random = Random(seed);
    let begin = mct.count();
    for _ in 0..RANDOM_RECTS {
        let rect = random_rect(&mut random);
        let color = random.next() | 0xFF00_0000;
        if uncached {
            fill_uncached(buffer, rect, color);
        } else {
            buffer.fill_rect(rect.x, rect.y, rect.width, rect.height, color);
            buffer.flush_rect(rect.x, rect.y, rect.width, rect.height);
        }
    }
    let cpu_us = us_since(mct, begin);
    println!(
        "{}: {} rects, {} px: GPU+CPU edges {} us, CPU alone {} us",
        if uncached { "Uncached" } else { "Cached  " },
        RANDOM_RECTS,
        pixels,
        gpu_us,
        cpu_us
    );
    println!(
        "  GPU way: edges {} us, streams {} us, GPU frames {} us, invalidating {} us",
        parts.cpu_edges / COUNTS_PER_US,
        parts.streams / COUNTS_PER_US,
        parts.gpu / COUNTS_PER_US,
        parts.invalidate / COUNTS_PER_US
    );
    true
}

/// Step 3: cached, then uncached. Afterwards the cached view is dropped whole, so that no line
/// of it from before the uncached writes is written back or read.
fn race(gpu: Mali, mct: Mct, shared: &mut Shared, buffer: &mut Framebuffer, seed: u32) {
    if race_one(gpu, mct, shared, buffer, seed, false) {
        race_one(gpu, mct, shared, buffer, seed, true);
    }
    mmu::invalidate_dcache_range(buffer.address(), WIDTH * HEIGHT * 4);
}

/// A rate within the supply's reach: 266 MHz needs 900 mV (`exynos4412.dtsi:760-763`), and BUCK4
/// gives 1.1 V, or 1.0 V from `DVS2` (`gpu_test`).
const FAST_HZ: u32 = 267_000_000;

fn main(_args: BootArgs) -> ! {
    println!("\nGPU rectangles: Mali-400 PPs");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();

    // SAFETY: the only references to these, taken once, here.
    let (shared, tables) = unsafe { (&mut *STORE.0.get(), &mut *TABLES.0.get()) };

    let gpu = Mali::new();
    match gpu.power_on(mali::SAFE_HZ, mct) {
        Ok(hz) => println!("GPU on: sclk_g3d {} Hz", hz),
        Err(error) => {
            println!("Power on ERROR {:?}", error);
            wait_keys_only();
        }
    }
    if let Err(error) = gpu.pmu_power_up(mali::PMU_ALL, mct) {
        println!("GPU PMU ERROR {:?}", error);
    }
    mmu::clean_dcache_range(shared.render_state.as_ptr() as usize, 64);
    if tables.map(soc::DRAM_BASE, 20 << 20).is_err() {
        println!("Page tables too few");
        wait_keys_only();
    }
    for pp in 0..mali::PP_COUNT {
        let reset = gpu.reset_pp(pp, mct, false);
        let enabled = gpu.enable_mmu(Some(pp), tables.directory(), mct);
        if reset.is_err() || enabled.is_err() {
            println!("PP{} reset {:?}, MMU {:?}", pp, reset, enabled);
            wait_keys_only();
        }
    }

    let fimd = Fimd::new();
    if let Err(error) = fimd.power_on() {
        println!("Display power on ERROR {:?}", error);
        wait_keys_only();
    }
    display::configure_pins();
    // SAFETY: the only reference to these, taken once: see `Pixels`.
    let pixels: &'static mut [u32] = unsafe { &mut *PIXELS.0.get() };
    let mut buffer = Framebuffer::new_cached(pixels, WIDTH, HEIGHT).expect("the buffer holds the screen");
    buffer.fill_rect(0, 0, WIDTH, HEIGHT, GREY);
    buffer.flush();
    if let Err(error) = fimd.start(&LTN101AL03, &buffer) {
        println!("Display start ERROR {:?}", error);
        wait_keys_only();
    }
    display::set_lvds(true);
    let settle = mct.count();
    while us_since(mct, settle) < u64::from(LVDS_SETTLE_MS) * 1000 {}
    display::set_backlight(true);
    println!("GPU and screen on; PP0-3 paging");
    console::flush();

    let good = cases(gpu, mct, shared, &mut buffer);
    println!("Rectangle cases: {}", verdict(good));
    let mut seed = 0x1234_5678;
    if good {
        race(gpu, mct, shared, &mut buffer, seed);
        match gpu.power_on(FAST_HZ, mct) {
            Ok(hz) => {
                println!("GPU at {} Hz", hz);
                race(gpu, mct, shared, &mut buffer, seed);
            }
            Err(error) => println!("Clock ERROR {:?}", error),
        }
    }
    println!("Keys: d random rectangles again, r restart, f fastboot");
    console::flush();

    let uart = console::uart();
    loop {
        match uart.try_read_byte() {
            Some(b'd') => {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) | 1;
                race(gpu, mct, shared, &mut buffer, seed);
            }
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => core::hint::spin_loop(),
        }
    }
}
