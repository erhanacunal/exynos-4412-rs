//! u-blox G6010 GPS test: turns the receiver's supplies on, sets up UART1 and prints what it
//! sends.
//!
//! 1. The console UART's clock and divider, read back: the baud rate they give must be 115200,
//!    which checks `Cmu::sclk_uart_hz` and the divider formula of `Uart::configure`.
//! 2. LDO9 and LDO18 before and after `g6010::power_on`.
//! 3. UART1 at 9600. Each sentence is checked and fed to `nmea::Fix`; once a second, a line
//!    with the counts and the fix. Without a good sentence for 3 s, the next baud rate of
//!    `BAUDS` is tried, until one gives sentences.
//!
//! Keys on the console: `r` raw sentences on or off, `b` the next baud rate, `p` supplies off or
//! on, `s` the counts, `f` restart into fastboot, `x` restart.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::g6010;
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::uart::Uart;
use exynos_4412_rs::embassy::uart::{UartRx, buffer_console};
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::nmea::{Fix, LineBuffer};
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println};

exynos_4412_rs::entry!(main);

/// Tried in this order while no sentence comes.
const BAUDS: [u32; 6] = [9600, 4800, 19200, 38400, 57600, 115200];
/// Without a good sentence for this long, the next baud rate.
const SEARCH_MS: u64 = 3000;

static RAW: AtomicBool = AtomicBool::new(true);
static NEXT_BAUD: AtomicBool = AtomicBool::new(false);
static TOGGLE_POWER: AtomicBool = AtomicBool::new(false);
static SHOW_STATS: AtomicBool = AtomicBool::new(false);
static BAUD: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if !embassy::on_interrupt(irq.id()) {
        println!("unexpected interrupt {} on CPU{}", irq.id(), cpu::id());
    }
    gic.end_of_interrupt(irq);
}

#[derive(Default)]
struct Counts {
    bytes: u32,
    good: u32,
    bad: u32,
}

/// `dd.ddddddd` from degrees times 10^7.
struct Degrees(i32);

impl core::fmt::Display for Degrees {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let magnitude = self.0.unsigned_abs();
        write!(f, "{}{}.{:07}", sign, magnitude / 10_000_000, magnitude % 10_000_000)
    }
}

fn print_fix(fix: &Fix, counts: &Counts, rx: &UartRx) {
    let stats = rx.stats();
    print_counts_prefix(counts, stats.dropped, stats.errors);
    match fix.time {
        Some(time) => println!(
            "  time {:02}:{:02}:{:02}.{:03}",
            time.hour, time.minute, time.second, time.millisecond
        ),
        None => println!("  time -"),
    }
    if let Some(date) = fix.date {
        println!("  date {}-{:02}-{:02}", date.year, date.month, date.day);
    }
    println!(
        "  valid {}, quality {}, satellites {} used / {} in view, hdop {:?}",
        fix.valid, fix.quality, fix.satellites_used, fix.satellites_in_view, fix.hdop_centi
    );
    if let (Some(latitude), Some(longitude)) = (fix.latitude, fix.longitude) {
        println!(
            "  position {}, {}, altitude {:?} dm, speed {:?} milliknots, course {:?}",
            Degrees(latitude),
            Degrees(longitude),
            fix.altitude_dm,
            fix.speed_milliknots,
            fix.course_centidegrees
        );
    }
}

fn print_counts_prefix(counts: &Counts, dropped: u32, errors: u32) {
    println!(
        "[{} baud] {} bytes, {} good sentences, {} bad, ring dropped {}, line errors {:#x}",
        BAUD.load(Ordering::Relaxed),
        counts.bytes,
        counts.good,
        counts.bad,
        dropped,
        errors
    );
}

fn set_baud(baud: u32) {
    let actual = g6010::set_baud(baud);
    // `configure` masked the UART's interrupts.
    Uart::new(g6010::UART).enable_rx_interrupt();
    BAUD.store(baud, Ordering::Relaxed);
    println!("UART1 at {} baud (divider gives {})", baud, actual);
}

#[embassy_executor::task]
async fn gps_task(pmic: Max77686, rx: UartRx) {
    let mct = Mct::new();
    let mut lines = LineBuffer::new();
    let mut fix = Fix::default();
    let mut counts = Counts::default();
    let mut baud_index = 0;
    let mut powered = true;
    let mut last_good = Instant::now();
    let mut last_report = Instant::now();
    let mut buffer = [0u8; 64];
    loop {
        if NEXT_BAUD.swap(false, Ordering::Relaxed)
            || (powered && last_good.elapsed() > Duration::from_millis(SEARCH_MS))
        {
            baud_index = (baud_index + 1) % BAUDS.len();
            set_baud(BAUDS[baud_index]);
            last_good = Instant::now();
        }
        if TOGGLE_POWER.swap(false, Ordering::Relaxed) {
            let result = if powered { g6010::power_off(&pmic, mct) } else { g6010::power_on(&pmic, mct) };
            powered = !powered;
            println!("Supplies {}: {:?}, powered {:?}", if powered { "on" } else { "off" }, result, g6010::is_powered(&pmic));
            last_good = Instant::now();
        }
        if SHOW_STATS.swap(false, Ordering::Relaxed) {
            let stats = rx.stats();
            println!("{:?}, line buffer dropped {}", stats, lines.dropped());
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            if !RAW.load(Ordering::Relaxed) {
                print_fix(&fix, &counts, &rx);
            } else {
                let stats = rx.stats();
                print_counts_prefix(&counts, stats.dropped, stats.errors);
            }
        }
        let count = match select(rx.read(&mut buffer), Timer::after(Duration::from_millis(100))).await {
            Either::First(count) => count,
            Either::Second(()) => continue,
        };
        counts.bytes += count as u32;
        for byte in &buffer[..count] {
            let Some(sentence) = lines.push(*byte) else {
                continue;
            };
            if RAW.load(Ordering::Relaxed) {
                // Printable ASCII only: `LineBuffer` drops anything else.
                println!("{}", core::str::from_utf8(sentence).unwrap_or("?"));
            }
            match fix.update(sentence) {
                Ok(_) => {
                    counts.good += 1;
                    last_good = Instant::now();
                }
                Err(error) => {
                    counts.bad += 1;
                    if RAW.load(Ordering::Relaxed) {
                        println!("  {:?}", error);
                    }
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn console_task() {
    let rx = UartRx::new(console::uart().id()).expect("the console UART has an interrupt");
    loop {
        match rx.read_byte().await {
            b'r' => {
                let raw = !RAW.load(Ordering::Relaxed);
                RAW.store(raw, Ordering::Relaxed);
                println!("Raw sentences {}", if raw { "on" } else { "off" });
            }
            b'b' => NEXT_BAUD.store(true, Ordering::Relaxed),
            b'p' => TOGGLE_POWER.store(true, Ordering::Relaxed),
            b's' => SHOW_STATS.store(true, Ordering::Relaxed),
            b'f' => power::restart(BootMode::Fastboot),
            b'x' => power::restart(BootMode::Normal),
            b'\r' | b'\n' => println!(),
            _ => println!("Keys: r raw on/off, b next baud, p supplies off/on, s counts, f fastboot, x restart"),
        }
    }
}

#[embassy_executor::task]
async fn keys_task() {
    let mut keys = PowerKeys::new();
    let mct = Mct::new();
    loop {
        keys.service(mct);
        Timer::after(Duration::from_millis(10)).await;
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nG6010 GPS test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();
    buffer_console();

    let cmu = Cmu::new();
    let console_uart = console::uart();
    let config = console_uart.config();
    match cmu.sclk_uart_hz(console_uart.id() as usize) {
        Some(hz) => {
            let divisor = (config.ubrdiv + 1) * 16 + config.ufracval;
            println!(
                "1. Console: sclk_uart {} Hz, UBRDIV {}, UFRACVAL {}: {} baud",
                hz,
                config.ubrdiv,
                config.ufracval,
                hz / divisor.max(1)
            );
        }
        None => println!("1. Console: sclk_uart from a parent this driver does not read, {:x?}", config),
    }

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("i2c_0: init ERROR {:?}", error);
        cpu::halt();
    }
    let pmic = Max77686::new(i2c);
    for (ldo, _) in [g6010::LDO_IO, g6010::LDO_LNA] {
        println!("2. LDO{} before: {:?}", ldo, pmic.ldo(ldo).map(|state| (state.mode(), state.millivolts())));
    }
    if let Err(error) = g6010::power_on(&pmic, mct) {
        println!("2. power_on ERROR {:?}", error);
    }
    for (ldo, _) in [g6010::LDO_IO, g6010::LDO_LNA] {
        println!("2. LDO{} after: {:?}", ldo, pmic.ldo(ldo).map(|state| (state.mode(), state.millivolts())));
    }

    let actual = g6010::setup_uart(BAUDS[0]);
    BAUD.store(BAUDS[0], Ordering::Relaxed);
    println!(
        "3. UART1: sclk_uart1 {:?} Hz, {} baud (divider gives {}), {:x?}",
        cmu.sclk_uart_hz(g6010::UART as usize),
        BAUDS[0],
        actual,
        Uart::new(g6010::UART).config()
    );
    let rx = UartRx::new(g6010::UART).expect("UART1 has an interrupt");

    embassy::run(|spawner| {
        spawner.spawn(keys_task().unwrap());
        spawner.spawn(console_task().unwrap());
        spawner.spawn(gps_task(pmic, rx).unwrap());
    })
}
