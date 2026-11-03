//! Embassy test: one executor per core, timers on the MCT, channels between cores.
//!
//! On CPU0: how late timers of 1 to 500 ms wake, three timers awaited together, then a job
//! run three times on CPU0 alone and once on each of CPU1-3 at the same time. CPU1-3 first
//! check their own timers, which wake through CPU0's MCT interrupt and `SEV`. Afterwards a
//! task keeps the UART and the keys served, at 10 ms.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println, smp, soc};

exynos_4412_rs::entry!(main);

const WORKERS: usize = 3;
/// Primes below this, by trial division: the job.
const JOB_LIMIT: u32 = 200_000;
/// Timer rounds each worker checks before taking jobs.
const WORKER_ROUNDS: u32 = 10;
const WORKER_PERIOD_MS: u64 = 50;

/// What CPU1-3 report to CPU0.
#[derive(Clone, Copy, Debug)]
enum Report {
    /// Timer rounds done: how late the latest and the average one woke, in µs.
    Timers {
        core: usize,
        max_late: u64,
        mean_late: u64,
    },
    /// A job done: its result and how long it took.
    Job { core: usize, primes: u32, us: u64 },
}

static JOBS: Channel<CriticalSectionRawMutex, u32, WORKERS> = Channel::new();
static REPORTS: Channel<CriticalSectionRawMutex, Report, 8> = Channel::new();

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    // Each secondary core gets one wake SGI from its start: see the README's SMP section.
    if !embassy::on_interrupt(irq.id()) && irq.id() != smp::WAKE_SGI {
        println!("unexpected interrupt {} on CPU{}", irq.id(), cpu::id());
    }
    gic.end_of_interrupt(irq);
}

fn primes_below(limit: u32) -> u32 {
    (2..limit)
        .filter(|n| (2..).take_while(|d| d * d <= *n).all(|d| n % d != 0))
        .count() as u32
}

/// MCT counts as µs.
fn us(counts: u64) -> u64 {
    counts / u64::from(soc::MCT_CLOCK_HZ / 1_000_000)
}

#[embassy_executor::task(pool_size = 3)]
async fn worker(core: usize) {
    let (mut max_late, mut total_late) = (0, 0);
    for _ in 0..WORKER_ROUNDS {
        let begin = Instant::now();
        Timer::after_millis(WORKER_PERIOD_MS).await;
        let late = begin.elapsed().as_micros() - WORKER_PERIOD_MS * 1000;
        max_late = max_late.max(late);
        total_late += late;
    }
    let mean_late = total_late / u64::from(WORKER_ROUNDS);
    REPORTS
        .send(Report::Timers {
            core,
            max_late,
            mean_late,
        })
        .await;
    loop {
        let limit = JOBS.receive().await;
        let begin = Mct::new().count();
        let primes = primes_below(limit);
        let us = us(Mct::new().count() - begin);
        REPORTS.send(Report::Job { core, primes, us }).await;
    }
}

/// How late a timer of `ms` wakes, by `embassy-time` and by the MCT directly, in µs.
async fn timer_lateness(ms: u64) -> (u64, u64) {
    let mct = Mct::new();
    let begin = Instant::now();
    let begin_count = mct.count();
    Timer::after_millis(ms).await;
    let by_instant = begin.elapsed().as_micros();
    let by_mct = us(mct.count() - begin_count);
    (by_instant - ms * 1000, by_mct.saturating_sub(ms * 1000))
}

#[embassy_executor::task]
async fn supervisor(spawner: Spawner) {
    println!("CPU0: timer lateness (embassy-time / MCT):");
    for ms in [1, 10, 100, 500] {
        let (instant, mct) = timer_lateness(ms).await;
        println!("  {:>3} ms: {} / {} us late", ms, instant, mct);
    }

    let begin = Instant::now();
    join3(
        Timer::after_millis(100),
        Timer::after_millis(200),
        Timer::after_millis(300),
    )
    .await;
    let together = begin.elapsed().as_millis();
    println!(
        "Timers of 100, 200 and 300 ms together: {} ms ({})",
        together,
        if (300..=301).contains(&together) {
            "OK"
        } else {
            "MISMATCH"
        }
    );

    for _ in 0..WORKERS {
        match REPORTS.receive().await {
            Report::Timers {
                core,
                max_late,
                mean_late,
            } => println!(
                "CPU{}: {} timers of {} ms, late {} us on average, {} us at most ({})",
                core,
                WORKER_ROUNDS,
                WORKER_PERIOD_MS,
                mean_late,
                max_late,
                if max_late < 1000 { "OK" } else { "MISMATCH" }
            ),
            other => println!("Unexpected report {:?}", other),
        }
    }

    // The job blocks this executor while it runs: nothing else is due now.
    let mct = Mct::new();
    let begin = mct.count();
    let mut expected = 0;
    for _ in 0..WORKERS {
        // Kept from being computed once for all three.
        expected = primes_below(core::hint::black_box(JOB_LIMIT));
    }
    let alone = us(mct.count() - begin);
    println!(
        "CPU0 alone: {} jobs (primes below {}: {}) in {} ms",
        WORKERS,
        JOB_LIMIT,
        expected,
        alone / 1000
    );

    let begin = mct.count();
    for _ in 0..WORKERS {
        JOBS.send(JOB_LIMIT).await;
    }
    let mut good = true;
    let mut cores = [false; soc::CPU_COUNT];
    for _ in 0..WORKERS {
        match REPORTS.receive().await {
            Report::Job { core, primes, us } => {
                println!("  CPU{}: {} primes in {} ms", core, primes, us / 1000);
                good &= primes == expected;
                cores[core] = true;
            }
            other => {
                println!("Unexpected report {:?}", other);
                good = false;
            }
        }
    }
    let parallel = us(mct.count() - begin);
    let used = cores.iter().filter(|used| **used).count();
    println!(
        "CPU1-3 together: {} ms on {} cores, {}.{:02}x faster ({})",
        parallel / 1000,
        used,
        alone / parallel.max(1),
        alone * 100 / parallel.max(1) % 100,
        if good && used == WORKERS {
            "OK"
        } else {
            "MISMATCH"
        }
    );

    spawner.spawn(console_task().unwrap());
}

/// Stays where the board can still be restarted from the UART or the keys.
#[embassy_executor::task]
async fn console_task() {
    println!("Done. UART: r restart, f restart into fastboot. Keys: as in power_test.");
    let uart = console::uart();
    let mut keys = PowerKeys::new();
    let mct = Mct::new();
    loop {
        keys.service(mct);
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
        Timer::after(Duration::from_millis(10)).await;
    }
}

fn secondary(core: usize) -> ! {
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    embassy::run(|spawner| spawner.spawn(worker(core).unwrap()))
}

fn main(_args: BootArgs) -> ! {
    println!("\nEmbassy test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();

    for core in 1..=WORKERS {
        if let Err(error) = smp::start_cpu(core, secondary) {
            println!("CPU{} ERROR {:?}", core, error);
        }
    }
    println!("CPU1-{} started", WORKERS);
    embassy::run(|spawner| spawner.spawn(supervisor(spawner).unwrap()))
}
