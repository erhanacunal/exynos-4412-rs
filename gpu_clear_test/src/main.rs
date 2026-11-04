//! First frames on the Mali-400: the pixel processors clear, nothing is drawn yet.
//!
//! 1. The GPU powered at 160 MHz, the PPs reset, each PP's MMU on with page tables that give it
//!    the CPU's addresses.
//! 2. PP0 clears a 64 x 64 buffer the CPU filled with another value; the CPU checks every pixel.
//!    Each tile first as its position alone; if that fails, with an empty list of polygons.
//! 3. The screen on, grey from the CPU. PP0 alone clears it red, then all four PPs blue; the CPU
//!    checks every pixel each time, and fills it once itself for comparison.
//!
//! Keys afterwards: `c` the next colour from four PPs, `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, rgb};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mali::{self, EMPTY_LIST, IdentityMap, Mali, PpFrame, TileEnd};
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{console, mmu, println, soc, BootArgs};

exynos_4412_rs::entry!(main);

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const WIDTH: usize = 1280;
const HEIGHT: usize = 800;
const SMALL: usize = 64;
const TIMEOUT_US: u64 = 100_000;
/// Words of stream for all the screen's 80 x 50 tiles, which PP0 renders alone: 4 a tile at most,
/// 4 for the end.
const STREAM_WORDS: usize = (WIDTH / mali::TILE) * (HEIGHT / mali::TILE) * 4 + 64;

const BEFORE: u32 = 0xDEAD_BEEF;
const SMALL_COLOR: u32 = 0xFF33_6699;
const GREY: u32 = rgb(64, 64, 64);
const COLORS: [u32; 6] = [
    rgb(255, 0, 0) | 0xFF00_0000,
    rgb(0, 0, 255) | 0xFF00_0000,
    rgb(0, 255, 0) | 0xFF00_0000,
    rgb(255, 255, 255) | 0xFF00_0000,
    rgb(255, 128, 0) | 0xFF00_0000,
    rgb(0, 0, 0) | 0xFF00_0000,
];

/// The screen's pixels, in `.bss`: the FIMD and the GPU use them by DMA.
#[repr(C, align(4096))]
struct Pixels(UnsafeCell<[u32; WIDTH * HEIGHT]>);

// SAFETY: `main` takes the one reference to it once, for the framebuffer.
unsafe impl Sync for Pixels {}

static PIXELS: Pixels = Pixels(UnsafeCell::new([0; WIDTH * HEIGHT]));

/// What else the GPU reads and writes, in `.bss`, on cache lines of their own.
#[repr(C, align(4096))]
struct Shared {
    small: [u32; SMALL * SMALL],
    streams: [[u32; STREAM_WORDS]; mali::PP_COUNT],
    render_state: [u32; 16],
    empty_list: [u32; 16],
}

struct Store(UnsafeCell<Shared>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Store {}

static STORE: Store = Store(UnsafeCell::new(Shared {
    small: [0; SMALL * SMALL],
    streams: [[0; STREAM_WORDS]; mali::PP_COUNT],
    render_state: [0; 16],
    empty_list: [0; 16],
}));

/// Five tables: 20 MiB, all of RAM from below the image.
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

/// Pixels of `pixels` that are not `color`, and the first of them.
fn count_wrong(pixels: &[u32], color: u32) -> (usize, Option<(usize, u32)>) {
    let mut wrong = 0;
    let mut first = None;
    for (index, &pixel) in pixels.iter().enumerate() {
        if pixel != color {
            wrong += 1;
            first.get_or_insert((index, pixel));
        }
    }
    (wrong, first)
}

/// What a frame clears: where, how wide, how high.
#[derive(Clone, Copy)]
struct Target {
    address: usize,
    width: usize,
    height: usize,
}

/// One frame that clears `target` to `color`, on the first `pps` PPs at once. Returns the
/// microseconds from the first start to the last end, or why it failed.
fn clear(gpu: Mali, mct: Mct, shared: &mut Shared, target: Target, color: u32, pps: usize, end: TileEnd) -> Result<u64, mali::Error> {
    let Target { address, width, height } = target;
    let render_state = shared.render_state.as_ptr() as usize;
    for pp in 0..pps {
        mali::clear_stream(&mut shared.streams[pp], width, height, pp, pps, end);
    }
    mmu::clean_dcache_range(shared.streams.as_ptr() as usize, core::mem::size_of_val(&shared.streams));
    let bytes = width * height * 4;
    mmu::discard_dcache_range(address, bytes);
    let frame = PpFrame::clear(width, height, color, address, width * 4, render_state);
    let begin = mct.count();
    for pp in 0..pps {
        gpu.start_pp(pp, &frame, shared.streams[pp].as_ptr() as u32)?;
    }
    for pp in 0..pps {
        gpu.wait_pp(pp, mct, TIMEOUT_US)?;
    }
    let us = us_since(mct, begin);
    mmu::invalidate_dcache_range(address, bytes);
    Ok(us)
}

/// After a failed frame: what the PPs show, then every PP stopped.
fn recover(gpu: Mali, mct: Mct, error: mali::Error) {
    println!("  ERROR {:?}", error);
    for pp in 0..mali::PP_COUNT {
        if let Ok((status, bus, mmu_raw)) = gpu.pp_state(pp) {
            println!("  PP{}: STATUS {:#x}, BUS_ERROR_STATUS {:#x}, MMU INT_RAWSTAT {:#x}", pp, status, bus, mmu_raw);
        }
        match gpu.hard_reset_pp(pp, mct) {
            Ok(_) => {}
            Err(error) => println!("  PP{} hard reset ERROR {:?}", pp, error),
        }
    }
}

/// Step 2. Returns the tile end that works, if one does.
fn small_buffer(gpu: Mali, mct: Mct, shared: &mut Shared) -> Option<TileEnd> {
    let list = shared.empty_list.as_ptr() as u32;
    for end in [TileEnd::Bare, TileEnd::EmptyList(list)] {
        shared.small.fill(BEFORE);
        mmu::clean_dcache_range(shared.small.as_ptr() as usize, core::mem::size_of_val(&shared.small));
        println!("PP0 clears 64 x 64 to {:#010x}, tiles {:?}", SMALL_COLOR, end);
        console::flush();
        let target = Target { address: shared.small.as_ptr() as usize, width: SMALL, height: SMALL };
        match clear(gpu, mct, shared, target, SMALL_COLOR, 1, end) {
            Ok(us) => {
                let (wrong, first) = count_wrong(&shared.small, SMALL_COLOR);
                println!("  done in {} us; {} of {} pixels wrong, first {:x?}: {}", us, wrong, SMALL * SMALL, first, verdict(wrong == 0));
                if wrong == 0 {
                    return Some(end);
                }
                let untouched = shared.small.iter().filter(|&&pixel| pixel == BEFORE).count();
                println!("  {} pixels untouched", untouched);
            }
            Err(error) => recover(gpu, mct, error),
        }
    }
    None
}

/// Step 3, one frame of the screen; `None` if it failed.
fn screen(gpu: Mali, mct: Mct, shared: &mut Shared, buffer: &Framebuffer, color: u32, pps: usize, end: TileEnd) -> Option<u64> {
    let target = Target { address: buffer.address(), width: WIDTH, height: HEIGHT };
    match clear(gpu, mct, shared, target, color, pps, end) {
        Ok(us) => {
            let (wrong, first) = count_wrong(buffer.pixels(), color);
            println!(
                "{} PP: 1280 x 800 to {:#010x} in {} us; {} wrong, first {:x?}: {}",
                pps,
                color,
                us,
                wrong,
                first,
                verdict(wrong == 0)
            );
            (wrong == 0).then_some(us)
        }
        Err(error) => {
            println!("{} PP: 1280 x 800 to {:#010x}", pps, color);
            recover(gpu, mct, error);
            None
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nGPU clears: Mali-400 PPs");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();

    // SAFETY: the only references to these, taken once, here.
    let (shared, tables) = unsafe { (&mut *STORE.0.get(), &mut *TABLES.0.get()) };

    // Step 1. BUCK4 is at 1.1 V as U-Boot leaves it (gpu_test), above what 160 MHz needs.
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
    shared.empty_list[..4].copy_from_slice(&EMPTY_LIST);
    mmu::clean_dcache_range(shared.empty_list.as_ptr() as usize, 64);
    mmu::clean_dcache_range(shared.render_state.as_ptr() as usize, 64);
    let start = soc::DRAM_BASE;
    if tables.map(start, 20 << 20).is_err() {
        println!("Page tables too few");
        wait_keys_only();
    }
    let shared_end = shared as *const Shared as usize + core::mem::size_of::<Shared>();
    let pixels_end = PIXELS.0.get() as usize + WIDTH * HEIGHT * 4;
    println!(
        "Mapped {:#010x}..{:#010x} for the GPU; data ends at {:#010x} and {:#010x}",
        start,
        start + (20 << 20),
        shared_end,
        pixels_end
    );
    for pp in 0..mali::PP_COUNT {
        let reset = gpu.reset_pp(pp, mct, false);
        let enabled = gpu.enable_mmu(Some(pp), tables.directory(), mct);
        if reset.is_err() || enabled.is_err() {
            println!("PP{} reset {:?}, MMU {:?}", pp, reset, enabled);
            wait_keys_only();
        }
    }
    println!("PP0-3 reset, their MMUs paging");

    // Step 2.
    let Some(end) = small_buffer(gpu, mct, shared) else {
        println!("GPU clear test: MISMATCH. Keys: r restart, f fastboot");
        wait_keys_only();
    };

    // Step 3.
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
    println!("Screen on, grey");

    let mut good = true;
    let one = screen(gpu, mct, shared, &buffer, COLORS[0], 1, end);
    let four = screen(gpu, mct, shared, &buffer, COLORS[1], mali::PP_COUNT, end);
    good &= one.is_some() && four.is_some();

    let begin = mct.count();
    buffer.fill_rect(0, 0, WIDTH, HEIGHT, COLORS[2]);
    buffer.flush();
    let cpu_us = us_since(mct, begin);
    println!("CPU: 1280 x 800 filled and flushed in {} us", cpu_us);
    if let (Some(one), Some(four)) = (one, four) {
        println!("  PP0 {:.1}x, four PPs {:.1}x faster than the CPU", cpu_us as f32 / one.max(1) as f32, cpu_us as f32 / four.max(1) as f32);
    }
    println!("GPU clear test: {}. Keys: c next colour, r restart, f fastboot", verdict(good));
    console::flush();

    let uart = console::uart();
    let mut next = 2;
    loop {
        match uart.try_read_byte() {
            Some(b'c') => {
                next = (next + 1) % COLORS.len();
                screen(gpu, mct, shared, &buffer, COLORS[next], mali::PP_COUNT, end);
            }
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => core::hint::spin_loop(),
        }
    }
}
