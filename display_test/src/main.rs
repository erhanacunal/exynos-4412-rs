//! Display test. Reports what U-Boot left (power domain, clocks, pins, FIMD registers, panel
//! supply) before touching anything, then starts the panel with a framebuffer of its own and
//! measures that the FIMD really scans it. After that, keys on the UART choose test patterns;
//! what they look like can only be judged by eye.
//!
//! The PMIC is only read: the panel supply LDO23 is reported, never changed.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::display::{
    self, BACKLIGHT, Fimd, Framebuffer, LTN101AL03, LVDS_PDN, LVDS_SETTLE_MS, Timing, rgb,
};
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::PowerKeys;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

const PANEL: Timing = LTN101AL03;
const PIXELS: usize = PANEL.width * PANEL.height;

const BLACK: u32 = rgb(0, 0, 0);
const WHITE: u32 = rgb(255, 255, 255);
const RED: u32 = rgb(255, 0, 0);
const GREEN: u32 = rgb(0, 255, 0);
const BLUE: u32 = rgb(0, 0, 255);
const CYAN: u32 = rgb(0, 255, 255);
const MAGENTA: u32 = rgb(255, 0, 255);
const YELLOW: u32 = rgb(255, 255, 0);
const DARK_GRAY: u32 = rgb(32, 32, 32);

/// `exynos4412-mehmet.dts:1123`, `vdd_lcd`, the panel supply.
const LDO_VDD_LCD: u8 = 23;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const MEASURED_FRAMES: u32 = 120;
/// How far the measured refresh may be from the computed one, in millihertz.
const REFRESH_TOLERANCE_MHZ: u32 = 500;
const BAR_WIDTH: usize = 16;
const BAR_STEP: usize = 8;

/// The framebuffer, in `.bss`, so that it adds nothing to the image. `memory.x` makes room.
#[repr(align(4096))]
struct Pixels(UnsafeCell<[u32; PIXELS]>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Pixels {}

static PIXEL_STORE: Pixels = Pixels(UnsafeCell::new([0; PIXELS]));

fn elapsed_us(mct: Mct, start: u64) -> u64 {
    (mct.count() - start) / COUNTS_PER_US
}

fn wait_ms(mct: Mct, ms: u32) {
    let end = mct.count() + u64::from(ms) * 1000 * COUNTS_PER_US;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

fn level(high: bool) -> &'static str {
    if high { "high" } else { "low" }
}

/// Everything about the display that can be read safely, changing nothing. Reads the FIMD only
/// if its domain and clock are on, since touching it otherwise could hang the bus.
fn report_state(stage: &str) {
    println!("--- State {} ---", stage);
    println!("LCD0 power domain status {:#x}", display::power_domain_status());
    let clocks = Cmu::new().fimd_clocks();
    println!(
        "SRC_LCD0 {:#010x} SRC_MASK_LCD0 {:#010x} DIV_LCD0 {:#010x} GATE_IP_LCD0 {:#010x}",
        clocks.src, clocks.src_mask, clocks.div, clocks.gate,
    );
    match Cmu::new().sclk_fimd0_hz() {
        Some(hz) => println!("sclk_fimd0 {} Hz", hz),
        None => println!("sclk_fimd0 not from mout_mpll_user_t"),
    }
    println!("LCDBLK_CFG {:#010x}", display::lcdblk_cfg());
    println!(
        "LVDS_PDN gpm1-0 {:?} {}, backlight gpd0-1 {:?} {}",
        LVDS_PDN.function(),
        level(LVDS_PDN.read()),
        BACKLIGHT.function(),
        level(BACKLIGHT.read()),
    );
    match Fimd::new().registers() {
        Some(regs) => {
            println!("VIDCON0  {:#010x} VIDCON1  {:#010x}", regs.vidcon0, regs.vidcon1);
            println!(
                "VIDTCON0 {:#010x} VIDTCON1 {:#010x} VIDTCON2 {:#010x}",
                regs.vidtcon0, regs.vidtcon1, regs.vidtcon2,
            );
            println!("WINCON0  {:#010x} SHADOWCON {:#010x}", regs.wincon0, regs.shadowcon);
            println!("BUF_START {:#010x} in use {:#010x}", regs.buf_start, regs.buf_start_shadow);
        }
        None => println!("FIMD registers not readable: domain or clock off"),
    }
}

/// Reads the panel supply. Changes nothing, whatever it finds.
fn report_panel_supply() {
    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("PMIC bus ERROR {:?}", error);
        return;
    }
    match Max77686::new(i2c).ldo(LDO_VDD_LCD) {
        Ok(state) => println!(
            "LDO{} vdd_lcd raw {:#04x}, mode {}, {} mV",
            LDO_VDD_LCD,
            state.raw(),
            state.mode(),
            state.millivolts(),
        ),
        Err(error) => println!("LDO{} ERROR {:?}", LDO_VDD_LCD, error),
    }
}

fn fail(what: &str, error: display::Error) -> ! {
    println!("{} ERROR {:?}", what, error);
    cpu::halt();
}

/// The FIMD scans the buffer, and at the rate the timing should give.
fn check_scanning(mct: Mct, fimd: Fimd, pixel_clock_hz: u32) {
    let expected_refresh = PANEL.refresh_millihertz(pixel_clock_hz);
    println!(
        "Pixel clock {} Hz (asked {}), refresh should be {}.{:03} Hz",
        pixel_clock_hz,
        PANEL.pixel_clock_hz,
        expected_refresh / 1000,
        expected_refresh % 1000,
    );

    if let Err(error) = fimd.wait_vsync() {
        fail("First vsync", error);
    }
    if let Err(error) = fimd.wait_vsync() {
        fail("Second vsync", error);
    }

    let in_use = fimd.scan_address();
    let programmed = fimd.registers().map_or(0, |regs| regs.buf_start);
    println!(
        "Buffer {:#010x}, in use {:#010x} ({})",
        programmed,
        in_use,
        if in_use == programmed { "OK" } else { "MISMATCH" },
    );

    let (mut lowest, mut highest) = (u32::MAX, 0);
    for _ in 0..10_000 {
        let line = fimd.line();
        lowest = lowest.min(line);
        highest = highest.max(line);
    }
    println!(
        "Line counter range {}..{} ({})",
        lowest,
        highest,
        if highest > lowest { "OK" } else { "MISMATCH" },
    );

    let start = mct.count();
    for _ in 0..MEASURED_FRAMES {
        if let Err(error) = fimd.wait_vsync() {
            fail("Refresh measurement", error);
        }
    }
    let span = mct.count() - start;
    let refresh = u64::from(MEASURED_FRAMES) * 1000 * u64::from(soc::MCT_CLOCK_HZ) / span.max(1);
    let refresh = refresh as u32;
    println!(
        "{} frames in {} us: {}.{:03} Hz ({})",
        MEASURED_FRAMES,
        span / COUNTS_PER_US,
        refresh / 1000,
        refresh % 1000,
        if refresh.abs_diff(expected_refresh) <= REFRESH_TOLERANCE_MHZ { "OK" } else { "MISMATCH" },
    );
}

fn draw_bars(buffer: &mut Framebuffer) {
    let colors = [WHITE, YELLOW, CYAN, GREEN, MAGENTA, RED, BLUE, BLACK];
    let width = buffer.width() / colors.len();
    let height = buffer.height();
    for (index, color) in colors.iter().enumerate() {
        buffer.fill_rect(index * width, 0, width, height, *color);
    }
}

fn draw_geometry(buffer: &mut Framebuffer) {
    const CORNER: usize = 64;
    let (width, height) = (buffer.width(), buffer.height());
    buffer.fill_rect(0, 0, width, height, BLACK);
    buffer.fill_rect(0, 0, width, 1, WHITE);
    buffer.fill_rect(0, height - 1, width, 1, WHITE);
    buffer.fill_rect(0, 0, 1, height, WHITE);
    buffer.fill_rect(width - 1, 0, 1, height, WHITE);
    buffer.fill_rect(width / 2, 0, 1, height, WHITE);
    buffer.fill_rect(0, height / 2, width, 1, WHITE);
    buffer.fill_rect(1, 1, CORNER, CORNER, RED);
    buffer.fill_rect(width - 1 - CORNER, 1, CORNER, CORNER, GREEN);
    buffer.fill_rect(1, height - 1 - CORNER, CORNER, CORNER, BLUE);
    buffer.fill_rect(width - 1 - CORNER, height - 1 - CORNER, CORNER, CORNER, YELLOW);
}

fn draw_gradient(buffer: &mut Framebuffer) {
    let (width, height) = (buffer.width(), buffer.height());
    let band = height / 4;
    for y in 0..height {
        for x in 0..width {
            let value = (x * 255 / (width - 1)) as u8;
            let color = match y / band {
                0 => rgb(value, 0, 0),
                1 => rgb(0, value, 0),
                2 => rgb(0, 0, value),
                _ => rgb(value, value, value),
            };
            buffer.set_pixel(x, y, color);
        }
    }
}

/// Draws and flushes, and reports how long each takes.
fn show(mct: Mct, buffer: &mut Framebuffer, draw: fn(&mut Framebuffer)) {
    let start = mct.count();
    draw(buffer);
    let drawn = elapsed_us(mct, start);
    let start = mct.count();
    buffer.flush();
    println!("Drawn in {} us, flushed in {} us", drawn, elapsed_us(mct, start));
}

/// A vertical bar crossing the screen, one step per frame, until a key arrives. Reports the
/// rate it keeps, which falls below the refresh if a frame takes too long.
fn animate(mct: Mct, fimd: Fimd, buffer: &mut Framebuffer, keys: &mut PowerKeys) -> u8 {
    let (width, height) = (buffer.width(), buffer.height());
    buffer.fill_rect(0, 0, width, height, DARK_GRAY);
    buffer.flush();

    let uart = console::uart();
    let mut x = 0;
    let mut frames = 0u32;
    let mut slowest = 0;
    let mut next_report = mct.count() + u64::from(soc::MCT_CLOCK_HZ);
    loop {
        if let Err(error) = fimd.wait_vsync() {
            fail("Animation", error);
        }
        let start = mct.count();
        buffer.fill_rect(x, 0, BAR_WIDTH, height, DARK_GRAY);
        buffer.flush_rect(x, 0, BAR_WIDTH, height);
        x = if x + BAR_STEP + BAR_WIDTH > width { 0 } else { x + BAR_STEP };
        buffer.fill_rect(x, 0, BAR_WIDTH, height, WHITE);
        buffer.flush_rect(x, 0, BAR_WIDTH, height);
        slowest = slowest.max(elapsed_us(mct, start));
        frames += 1;

        if mct.count() >= next_report {
            next_report += u64::from(soc::MCT_CLOCK_HZ);
            println!("{} frames/s, slowest frame {} us", frames, slowest);
            frames = 0;
            slowest = 0;
        }
        keys.service(mct);
        if let Some(key) = uart.try_read_byte() {
            return key;
        }
    }
}

/// Runs one pattern. Returns the key that ended it, if it is one that runs until a key.
fn run_pattern(key: u8, mct: Mct, fimd: Fimd, buffer: &mut Framebuffer, keys: &mut PowerKeys) -> Option<u8> {
    fimd.set_solid_color(None);
    match key {
        b'1' => {
            println!("1: solid red, from a register and not from DRAM");
            fimd.set_solid_color(Some(RED));
        }
        b'2' => {
            println!("2: bars, left to right: white yellow cyan green magenta red blue black");
            show(mct, buffer, draw_bars);
        }
        b'3' => {
            println!("3: 1-pixel white border and center cross, on black.");
            println!("   Corners: top left red, top right green, bottom left blue, bottom right yellow");
            show(mct, buffer, draw_geometry);
        }
        b'4' => {
            println!("4: gradients, top to bottom: red green blue gray, dark on the left");
            show(mct, buffer, draw_gradient);
        }
        b'5' => {
            println!("5: white bar moving to the right. Any key ends it");
            return Some(animate(mct, fimd, buffer, keys));
        }
        _ => println!("Keys: 1 solid, 2 bars, 3 geometry, 4 gradient, 5 animation, l LVDS, b backlight"),
    }
    None
}

fn main(_args: BootArgs) -> ! {
    println!("\nDisplay test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    report_state("left by U-Boot");
    report_panel_supply();

    let fimd = Fimd::new();
    if let Err(error) = fimd.power_on() {
        fail("Power on", error);
    }
    display::configure_pins();

    // SAFETY: the only reference to this buffer, taken once: see `Pixels`.
    let pixels: &'static mut [u32] = unsafe { &mut *PIXEL_STORE.0.get() };
    let Some(mut buffer) = Framebuffer::new(pixels, PANEL.width, PANEL.height) else {
        println!("Framebuffer too small");
        cpu::halt();
    };
    show(mct, &mut buffer, draw_bars);

    let pixel_clock_hz = match fimd.start(&PANEL, &buffer) {
        Ok(hz) => hz,
        Err(error) => fail("Start", error),
    };
    display::set_lvds(true);
    wait_ms(mct, LVDS_SETTLE_MS);
    display::set_backlight(true);

    report_state("after start");
    check_scanning(mct, fimd, pixel_clock_hz);
    println!("Now showing bars, left to right: white yellow cyan green magenta red blue black");
    println!("Keys: 1 solid, 2 bars, 3 geometry, 4 gradient, 5 animation, l LVDS, b backlight");

    let uart = console::uart();
    let mut lvds = true;
    let mut backlight = true;
    let mut pending = None;
    let mut keys = PowerKeys::new();
    loop {
        keys.service(mct);
        let Some(key) = pending.take().or_else(|| uart.try_read_byte()) else {
            continue;
        };
        match key {
            b'l' => {
                lvds = !lvds;
                display::set_lvds(lvds);
                println!("LVDS {}", if lvds { "on" } else { "off" });
            }
            b'b' => {
                backlight = !backlight;
                display::set_backlight(backlight);
                println!("Backlight {}", if backlight { "on" } else { "off" });
            }
            b'\r' | b'\n' => print!("\r\n"),
            key => pending = run_pattern(key, mct, fimd, &mut buffer, &mut keys),
        }
    }
}
