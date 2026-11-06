//! Light standby test: the power key turns the screen off and slows the CPU, and wakes it again.
//!
//! The panel shows colored bars and the CPU runs at 1.4 GHz. A short press of the power key
//! enters `standby::enter`: backlight, LVDS and FIMD off, the CPU at 200 MHz and 0.9 V, the
//! core asleep in `wfi`. The next short press brings the screen and 1.4 GHz back, and the
//! standby is reported: how long, how much of it asleep, the clocks. Each standby moves the
//! bars, so the screen shows that the program went on.
//!
//! Keys: power short = standby / wake, power held 2 s = power off. UART: `r` restart, `f`
//! restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing, rgb};
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::standby::{self, PowerButton, Press};
use exynos_4412_rs::{BootArgs, console, cpu, cpufreq, mmu, println, sleep};

exynos_4412_rs::entry!(main);

const PANEL: Timing = LTN101AL03;
const PIXELS: usize = PANEL.width * PANEL.height;
const RUN_MHZ: u32 = 1400;
/// How often the main loop looks at the key and the UART.
const POLL_US: u64 = 5_000;
const BARS: [u32; 8] = [
    rgb(255, 255, 255),
    rgb(255, 255, 0),
    rgb(0, 255, 255),
    rgb(0, 255, 0),
    rgb(255, 0, 255),
    rgb(255, 0, 0),
    rgb(0, 0, 255),
    rgb(0, 0, 0),
];

/// The framebuffer, in `.bss`. `memory.x` makes room.
#[repr(align(4096))]
struct Pixels(UnsafeCell<[u32; PIXELS]>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Pixels {}

static PIXEL_STORE: Pixels = Pixels(UnsafeCell::new([0; PIXELS]));

fn halt(why: &str) -> ! {
    println!("{}: stopped", why);
    console::flush();
    cpu::halt();
}

/// Eight vertical bars, turned by `shift` places.
fn draw_bars(buffer: &mut Framebuffer, shift: usize) {
    let width = PANEL.width / BARS.len();
    for (i, _) in BARS.iter().enumerate() {
        let color = BARS[(i + shift) % BARS.len()];
        buffer.fill_rect(i * width, 0, width, PANEL.height, color);
    }
    buffer.flush();
}

fn start_panel(mct: Mct, buffer: &mut Framebuffer) {
    let fimd = Fimd::new();
    if let Err(error) = fimd.power_on() {
        println!("Panel ERROR {:?}", error);
        halt("No panel");
    }
    display::configure_pins();
    draw_bars(buffer, 0);
    if let Err(error) = fimd.start(&PANEL, buffer) {
        println!("Panel ERROR {:?}", error);
        halt("No panel");
    }
    display::set_lvds(true);
    sleep::for_us(mct, u64::from(LVDS_SETTLE_MS) * 1000);
    display::set_backlight(true);
}

fn report(count: u32, report: &standby::Report) {
    let percent = report.slept_us * 100 / report.standby_us.max(1);
    println!(
        "Standby {}: {} ms, asleep {}%, {} wakes; armclk {} -> {} -> {} Hz; ended by {:?}",
        count,
        report.standby_us / 1000,
        percent,
        report.wakes,
        report.before_hz,
        report.standby_hz,
        report.after_hz,
        report.press
    );
}

fn main(_args: BootArgs) -> ! {
    println!("\nStandby test: power key");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("I2C ERROR {:?}", error);
        halt("No PMIC");
    }
    let pmic = Max77686::new(i2c);
    match cpufreq::set_arm_mhz(pmic, mct, RUN_MHZ) {
        Ok(change) => println!("CPU: {} -> {} Hz, {} uV", change.from_hz, change.to_hz, change.to_microvolts),
        Err(error) => println!("CPU ERROR {:?}, stays at {} Hz", error, cpufreq::arm_hz()),
    }

    // SAFETY: the only reference to this buffer, taken once: see `Pixels`.
    let pixels: &'static mut [u32] = unsafe { &mut *PIXEL_STORE.0.get() };
    let Some(mut buffer) = Framebuffer::new(pixels, PANEL.width, PANEL.height) else {
        halt("Framebuffer too small");
    };
    start_panel(mct, &mut buffer);
    println!("Panel on, bars. Power key: short = standby/wake, held 2 s = power off");
    println!("UART: r restart, f fastboot");
    console::flush();

    let uart = console::uart();
    let mut button = PowerButton::new();
    let mut count = 0;
    loop {
        match button.poll(mct) {
            Some(Press::Short) => {
                count += 1;
                println!("Standby {}: press the power key to wake", count);
                console::flush();
                match standby::enter(mct, Some(pmic), &mut button) {
                    Ok(done) => {
                        report(count, &done);
                        if done.press == Press::Long {
                            println!("Long press: power off");
                            power::power_off();
                        }
                    }
                    Err(error) => println!("Standby {}: ERROR {:?}", count, error),
                }
                draw_bars(&mut buffer, count as usize);
                console::flush();
            }
            Some(Press::Long) => {
                println!("Long press: power off");
                power::power_off();
            }
            None => {}
        }
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
        sleep::for_us(mct, POLL_US);
    }
}
