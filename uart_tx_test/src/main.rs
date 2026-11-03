//! Buffered console test: `print!` into a ring that the UART's transmit interrupt sends,
//! while a task ticks every 1 ms on CPU0.
//!
//! 1. One line of 100 characters, polled, before the console is buffered.
//! 2. The same line buffered: how long `println!` takes, how long the ring takes to go out,
//!    and the ticker meanwhile.
//! 3. Ten lines at once, then 200, more than the ring holds: the printing core then sends
//!    the oldest bytes itself, polled.
//! 4. CPU0 and CPU1 printing 20 lines each at the same time: every line must come out whole.
//!
//! Keys on the UART: `p` prints 30 lines and panics at once, `r` prints 30 lines and
//! restarts at once, `f` the same into fastboot, `s` the counts. The 30 lines are still in
//! the ring when the panic or the restart comes: all of them must come out before it.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::uart::{UartRx, buffer_console, flush_console};
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println, smp};

exynos_4412_rs::entry!(main);

/// 100 characters.
const LINE: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz..";
const _: () = assert!(LINE.len() == 100);
/// More lines of `LINE` than the ring holds: 102 bytes each with `\r\n`.
const BURST_LINES: u32 = 200;
const SHARED_LINES: u32 = 20;
const LAST_LINES: u32 = 30;

static TICKS: AtomicU32 = AtomicU32::new(0);
static MAX_LATE_US: AtomicU32 = AtomicU32::new(0);
static GO_CPU1: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static CPU1_DONE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if !embassy::on_interrupt(irq.id()) && irq.id() != smp::WAKE_SGI {
        println!("unexpected interrupt {} on CPU{}", irq.id(), cpu::id());
    }
    gic.end_of_interrupt(irq);
}

#[embassy_executor::task]
async fn ticker() {
    loop {
        let begin = Instant::now();
        Timer::after(Duration::from_millis(1)).await;
        let late = begin.elapsed().as_micros().saturating_sub(1000) as u32;
        MAX_LATE_US.fetch_max(late, Ordering::Relaxed);
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Starts a ticker window after one tick, so that a tick held back before counts in none.
async fn window() -> u32 {
    let before = TICKS.load(Ordering::Relaxed);
    while TICKS.load(Ordering::Relaxed) == before {
        yield_now().await;
    }
    MAX_LATE_US.store(0, Ordering::Relaxed);
    TICKS.load(Ordering::Relaxed)
}

/// Ticks since `start` and the latest one, in µs.
fn ticks_since(start: u32) -> (u32, u32) {
    (TICKS.load(Ordering::Relaxed) - start, MAX_LATE_US.load(Ordering::Relaxed))
}

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "MISMATCH" }
}

/// Prints `count` lines and returns how long the calls took, in µs.
fn lines(tag: &str, count: u32) -> u64 {
    let begin = Instant::now();
    for index in 0..count {
        println!("{} {:03} {}", tag, index, &LINE[..LINE.len() - tag.len() - 5]);
    }
    begin.elapsed().as_micros()
}

#[embassy_executor::task]
async fn supervisor(polled_us: u64) {
    println!("1. One line polled, before buffering: {} us in println", polled_us);

    let start = window().await;
    let begin = Instant::now();
    println!("{}", LINE);
    let printed = begin.elapsed().as_micros();
    flush_console().await;
    let sent = begin.elapsed().as_micros();
    let (ticks, late) = ticks_since(start);
    println!(
        "2. The same line buffered: {} us in println, {} us until the ring was out; ticker {} ticks, latest {} us late ({})",
        printed,
        sent,
        ticks,
        late,
        verdict(printed * 10 < polled_us && late < 200)
    );

    let start = window().await;
    let begin = Instant::now();
    let printed = lines("ten", 10);
    flush_console().await;
    let sent = begin.elapsed().as_micros();
    let (ticks, late) = ticks_since(start);
    let stats = console::tx_stats();
    println!(
        "3a. Ten lines: {} us in println, {} ms until out; ticker {} ticks, latest {} us late; {} bytes sent polled so far ({})",
        printed,
        sent / 1000,
        ticks,
        late,
        stats.polled,
        verdict(stats.polled == 0 && late < 200)
    );

    let before = console::tx_stats();
    let start = window().await;
    let begin = Instant::now();
    let printed = lines("burst", BURST_LINES);
    flush_console().await;
    let sent = begin.elapsed().as_micros();
    let (ticks, late) = ticks_since(start);
    let after = console::tx_stats();
    let bytes = BURST_LINES as usize * (LINE.len() + 2);
    println!(
        "3b. {} lines, {} bytes, ring {}: {} ms in println, {} ms until out; {} bytes sent polled by the printer; most held {}; ticker {} ticks, latest {} us late",
        BURST_LINES,
        bytes,
        console::TX_RING_BYTES,
        printed / 1000,
        sent / 1000,
        after.polled - before.polled,
        after.most_held,
        ticks,
        late
    );
    println!(
        "    expected: about {} bytes polled, the overflow ({})",
        bytes.saturating_sub(console::TX_RING_BYTES),
        verdict(after.polled > before.polled && after.most_held as usize == console::TX_RING_BYTES)
    );

    println!("4. CPU0 and CPU1, {} lines each at once: every line must be whole", SHARED_LINES);
    flush_console().await;
    GO_CPU1.signal(());
    lines("cpu0", SHARED_LINES);
    CPU1_DONE.wait().await;
    flush_console().await;
    println!("{:?}", console::tx_stats());
    println!("Done. Keys: p panic after {0} lines, r restart after {0} lines, f fastboot after {0} lines, s counts.", LAST_LINES);
}

#[embassy_executor::task]
async fn cpu1_printer() {
    GO_CPU1.wait().await;
    lines("cpu1", SHARED_LINES);
    CPU1_DONE.signal(());
}

#[embassy_executor::task]
async fn console_task() {
    let rx = UartRx::new(console::uart().id()).expect("the console UART has an interrupt");
    loop {
        match rx.read_byte().await {
            b'p' => {
                lines("before panic", LAST_LINES);
                panic!("on purpose, with {} bytes in the ring", console::held());
            }
            b'r' => {
                lines("before restart", LAST_LINES);
                println!("Restarting with {} bytes in the ring", console::held());
                power::restart(BootMode::Normal);
            }
            b'f' => {
                lines("before fastboot", LAST_LINES);
                println!("Restarting into fastboot with {} bytes in the ring", console::held());
                power::restart(BootMode::Fastboot);
            }
            b's' => println!("{:?}", console::tx_stats()),
            b'\r' | b'\n' => println!(),
            _ => println!("Keys: p panic, r restart, f fastboot, s counts"),
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

fn secondary(_core: usize) -> ! {
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    embassy::run(|spawner| spawner.spawn(cpu1_printer().unwrap()))
}

fn main(_args: BootArgs) -> ! {
    println!("\nBuffered console test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();

    // Out of the FIFO first, so that the line starts on an empty one.
    console::flush();
    let begin = embassy::now();
    println!("{}", LINE);
    let polled_us = embassy::now() - begin;
    buffer_console();
    if let Err(error) = smp::start_cpu(1, secondary) {
        println!("CPU1 ERROR {:?}", error);
    }
    embassy::run(|spawner| {
        spawner.spawn(ticker().unwrap());
        spawner.spawn(keys_task().unwrap());
        spawner.spawn(console_task().unwrap());
        spawner.spawn(supervisor(polled_us).unwrap());
    })
}
