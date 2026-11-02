//! MCT local timer test. Gives each of the four cores a timer of its own, at a different
//! rate, and measures from the handlers: the tick rate, the period against the global
//! counter, and its spread. Then stops one core's timer alone, fires another once, and
//! keeps all of them running, with CPU0 echoing the UART. CPU1-3 wait in `wfi`.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::timer::{LocalTimer, Mct};
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, smp, soc};

exynos_4412_rs::entry!(main);

const CPUS: usize = soc::CPU_COUNT;
const COUNTS_PER_SECOND: u64 = soc::MCT_CLOCK_HZ as u64;

/// 100 Hz, 1 kHz, 500 Hz and 250 Hz, all different so that a tick on the wrong core shows.
const PERIODS: [u32; CPUS] = [240_000, 24_000, 48_000, 96_000];
const MEASURE_SECONDS: u32 = 5;
const STOPPED_CPU: usize = 2;
const ONESHOT_CPU: usize = 3;
const ONESHOT_PERIOD: u32 = 240_000;

/// What the handler measures on one core. In its own cache line, written by that core only.
#[repr(align(32))]
struct Stats {
    ticks: AtomicU32,
    first: AtomicU32,
    last: AtomicU32,
    min: AtomicU32,
    max: AtomicU32,
}

static STATS: [Stats; CPUS] = [const {
    Stats {
        ticks: AtomicU32::new(0),
        first: AtomicU32::new(0),
        last: AtomicU32::new(0),
        min: AtomicU32::new(u32::MAX),
        max: AtomicU32::new(0),
    }
}; CPUS];
static ONESHOT: [AtomicBool; CPUS] = [const { AtomicBool::new(false) }; CPUS];
static WRONG_IRQS: AtomicU32 = AtomicU32::new(0);
static READY: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    match irq.id() {
        soc::MCT_LOCAL_IRQ => {
            let me = cpu::id();
            let timer = LocalTimer::new(me);
            if ONESHOT[me].load(Ordering::Relaxed) {
                timer.stop();
            }
            timer.clear_interrupt();
            record_tick(&STATS[me]);
        }
        // Each core takes one on starting: see `Gic::init_this_cpu`.
        smp::WAKE_SGI => {}
        _ => {
            WRONG_IRQS.fetch_add(1, Ordering::Relaxed);
        }
    }
    gic.end_of_interrupt(irq);
}

fn record_tick(stats: &Stats) {
    let now = Mct::new().count() as u32;
    let ticks = stats.ticks.load(Ordering::Relaxed);
    if ticks == 0 {
        stats.first.store(now, Ordering::Relaxed);
    } else {
        let interval = now.wrapping_sub(stats.last.load(Ordering::Relaxed));
        if interval < stats.min.load(Ordering::Relaxed) {
            stats.min.store(interval, Ordering::Relaxed);
        }
        if interval > stats.max.load(Ordering::Relaxed) {
            stats.max.store(interval, Ordering::Relaxed);
        }
    }
    stats.last.store(now, Ordering::Relaxed);
    stats.ticks.store(ticks + 1, Ordering::Release);
}

fn secondary(me: usize) -> ! {
    let gic = Gic::new();
    gic.init_this_cpu();
    gic.enable(soc::MCT_LOCAL_IRQ);
    println!("CPU{} up", me);
    cpu::enable_irq();
    READY.fetch_add(1, Ordering::Release);
    loop {
        // SAFETY: `wfi` only suspends the core until the next interrupt.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

fn wait_seconds(mct: Mct, seconds: u32) {
    let end = mct.count() + u64::from(seconds) * COUNTS_PER_SECOND;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

fn ticks(cpu: usize) -> u32 {
    STATS[cpu].ticks.load(Ordering::Acquire)
}

fn all_ticks() -> [u32; CPUS] {
    core::array::from_fn(ticks)
}

fn start_all() {
    for (cpu, period) in PERIODS.iter().enumerate() {
        LocalTimer::new(cpu).start_periodic(*period);
    }
}

fn stop_all() {
    for cpu in 0..CPUS {
        LocalTimer::new(cpu).stop();
    }
}

/// Rate and period of each core. Takes the same time on every core, then stops them all.
fn measure(mct: Mct) {
    start_all();
    wait_seconds(mct, MEASURE_SECONDS);
    stop_all();

    for (cpu, period) in PERIODS.iter().enumerate() {
        let stats = &STATS[cpu];
        let count = ticks(cpu);
        let expected = (u64::from(MEASURE_SECONDS) * COUNTS_PER_SECOND / u64::from(*period)) as u32;
        let span = stats.last.load(Ordering::Relaxed).wrapping_sub(stats.first.load(Ordering::Relaxed));
        // In thousandths of a count, to see one count of systematic error over the run.
        let mean = u64::from(span) * 1000 / u64::from(count.max(2) - 1);
        println!(
            "CPU{}: ticks {} expected {} ({}), mean {}.{:03} expected {}, min {} max {}",
            cpu,
            count,
            expected,
            if count.abs_diff(expected) <= 1 { "OK" } else { "MISMATCH" },
            mean / 1000,
            mean % 1000,
            period,
            stats.min.load(Ordering::Relaxed),
            stats.max.load(Ordering::Relaxed),
        );
    }
}

/// One core's timer stops, the others carry on.
fn stop_one(mct: Mct) {
    start_all();
    wait_seconds(mct, 1);
    LocalTimer::new(STOPPED_CPU).stop();
    let before = all_ticks();
    wait_seconds(mct, 2);
    let after = all_ticks();
    print!("CPU{} stopped, ticks in 2 s:", STOPPED_CPU);
    for cpu in 0..CPUS {
        print!(" {}", after[cpu] - before[cpu]);
    }
    println!(" ({})", if after[STOPPED_CPU] == before[STOPPED_CPU] { "OK" } else { "MISMATCH" });
    stop_all();
}

/// The handler stops the timer at its first tick, so it fires once per start.
fn oneshot(mct: Mct) {
    ONESHOT[ONESHOT_CPU].store(true, Ordering::Relaxed);
    for round in 1..=2 {
        let before = ticks(ONESHOT_CPU);
        LocalTimer::new(ONESHOT_CPU).start_periodic(ONESHOT_PERIOD);
        wait_seconds(mct, 1);
        let fired = ticks(ONESHOT_CPU) - before;
        println!(
            "One-shot {}, CPU{}: {} ticks ({})",
            round,
            ONESHOT_CPU,
            fired,
            if fired == 1 { "OK" } else { "MISMATCH" },
        );
    }
    ONESHOT[ONESHOT_CPU].store(false, Ordering::Relaxed);
}

fn main(_args: BootArgs) -> ! {
    println!("\nMCT local timer test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let gic = Gic::new();
    gic.init();
    gic.enable(soc::MCT_LOCAL_IRQ);

    for target in 1..CPUS {
        if let Err(error) = smp::start_cpu(target, secondary) {
            println!("CPU{} ERROR {:?}", target, error);
            cpu::halt();
        }
    }
    while READY.load(Ordering::Acquire) != CPUS as u32 - 1 {
        core::hint::spin_loop();
    }
    cpu::enable_irq();

    println!("{} s measurement, CPUs: 100 Hz, 1 kHz, 500 Hz, 250 Hz. Times in counter units (24 MHz).", MEASURE_SECONDS);
    measure(mct);
    stop_one(mct);
    oneshot(mct);
    println!("Unexpected interrupts: {}", WRONG_IRQS.load(Ordering::Relaxed));

    start_all();
    println!("Free running. Keys echo as <x>.");
    let uart = console::uart();
    let mut next = mct.count() + COUNTS_PER_SECOND;
    let mut last = all_ticks();
    loop {
        if mct.count() >= next {
            next += COUNTS_PER_SECOND;
            let now = all_ticks();
            println!(
                "ticks/s: {} {} {} {}",
                now[0] - last[0],
                now[1] - last[1],
                now[2] - last[2],
                now[3] - last[3],
            );
            last = now;
        }
        match uart.try_read_byte() {
            Some(b'\r') => println!(),
            Some(byte) => print!("<{}>", byte as char),
            None => {}
        }
    }
}
