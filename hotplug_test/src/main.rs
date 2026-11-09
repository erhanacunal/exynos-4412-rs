//! CPU hotplug test. In each round CPU1-3 are started, each fills a region of its own with a
//! pattern for the round, which stays dirty in its L1 cache, and stops itself with
//! `smp::stop_this_cpu`. CPU0 waits for each core to lose power, then checks the regions: a
//! word lost means the cache was not cleaned before the power went. Prints every tenth round
//! and every failure, then a summary.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::smp::{self, StartError};
use exynos_4412_rs::{BootArgs, cpu, mmu, println, soc};

exynos_4412_rs::entry!(main);

const CPUS: usize = soc::CPU_COUNT;
const ROUNDS: u32 = 100;
/// 16 KiB per core: half of its L1 data cache.
const REGION_WORDS: usize = 16 * 1024 / 4;
const OFFLINE_TIMEOUT_MS: u64 = 100;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

static ROUND: AtomicU32 = AtomicU32::new(0);
static DATA: [AtomicU32; CPUS * REGION_WORDS] = [const { AtomicU32::new(0) }; CPUS * REGION_WORDS];
static RUNS: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];
static WAKE_SGIS: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];
static OTHER_IRQS: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if irq.id() == smp::WAKE_SGI {
        WAKE_SGIS[cpu::id()].fetch_add(1, Ordering::Relaxed);
    } else {
        OTHER_IRQS.fetch_add(1, Ordering::Relaxed);
    }
    gic.end_of_interrupt(irq);
}

fn pattern(cpu: usize, round: u32, index: usize) -> u32 {
    (index as u32).wrapping_mul(0x9E37_79B1) ^ (round << 8) ^ cpu as u32
}

fn worker(cpu: usize) -> ! {
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    let round = ROUND.load(Ordering::Acquire);
    for index in 0..REGION_WORDS {
        DATA[cpu * REGION_WORDS + index].store(pattern(cpu, round, index), Ordering::Relaxed);
    }
    RUNS[cpu].fetch_add(1, Ordering::Release);
    smp::stop_this_cpu()
}

/// Words of `cpu`'s region that do not hold the round's pattern.
fn check(cpu: usize, round: u32) -> usize {
    (0..REGION_WORDS).filter(|&index| DATA[cpu * REGION_WORDS + index].load(Ordering::Relaxed) != pattern(cpu, round, index)).count()
}

#[derive(Clone, Copy)]
struct Range {
    min: u64,
    max: u64,
    sum: u64,
    count: u64,
}

impl Range {
    const fn new() -> Self {
        Self { min: u64::MAX, max: 0, sum: 0, count: 0 }
    }

    fn add(&mut self, value: u64) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += value;
        self.count += 1;
    }

    fn print(&self, label: &str) {
        if self.count == 0 {
            println!("{}: none", label);
        } else {
            println!("{}: min {} us, avg {} us, max {} us ({})", label, self.min, self.sum / self.count, self.max, self.count);
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nCPU hotplug test, {} rounds", ROUNDS);
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    cpu::enable_irq();

    let mut start_us = Range::new();
    let mut offline_us = Range::new();
    let (mut start_errors, mut offline_timeouts, mut lost_rounds, mut lost_words) = (0u32, 0u32, 0u32, 0usize);

    for round in 1..=ROUNDS {
        ROUND.store(round, Ordering::Release);
        let mut starts = [0u64; CPUS];
        let mut offs = [0u64; CPUS];
        let mut lost = [0usize; CPUS];
        let mut failed = false;
        for target in 1..CPUS {
            let begin = mct.count();
            let result = smp::start_cpu(target, worker);
            starts[target] = (mct.count() - begin) / COUNTS_PER_US;
            if let Err(error) = result {
                println!("Round {} CPU{} start ERROR {:?}, power conf {:#x} status {:#x}", round, target, error, smp::power_config(target), smp::power_status(target));
                start_errors += 1;
                failed = true;
                if let StartError::BootTimeout | StartError::PowerTimeout(_) = error {
                    continue;
                }
            }
            start_us.add(starts[target]);
        }
        for target in 1..CPUS {
            match smp::wait_offline(target, OFFLINE_TIMEOUT_MS) {
                Ok(us) => {
                    offs[target] = us;
                    offline_us.add(us);
                }
                Err(status) => {
                    println!("Round {} CPU{} still powered: conf {:#x} status {:#x}", round, target, smp::power_config(target), status);
                    offline_timeouts += 1;
                    failed = true;
                }
            }
            lost[target] = check(target, round);
            if lost[target] != 0 {
                lost_words += lost[target];
                failed = true;
            }
        }
        if lost.iter().any(|&words| words != 0) {
            lost_rounds += 1;
        }
        if failed || round % 10 == 0 || round == 1 {
            println!(
                "Round {:>3}: start {}/{}/{} us, off {}/{}/{} us, lost words {}/{}/{}",
                round, starts[1], starts[2], starts[3], offs[1], offs[2], offs[3], lost[1], lost[2], lost[3]
            );
        }
    }

    println!("Summary:");
    start_us.print("  start_cpu");
    offline_us.print("  power off after start_cpu returned");
    println!("  start errors {}, still powered {}, rounds with lost data {} ({} words)", start_errors, offline_timeouts, lost_rounds, lost_words);
    println!(
        "  runs {}/{}/{}, wake SGIs {}/{}/{}, other IRQs {}",
        RUNS[1].load(Ordering::Relaxed),
        RUNS[2].load(Ordering::Relaxed),
        RUNS[3].load(Ordering::Relaxed),
        WAKE_SGIS[1].load(Ordering::Relaxed),
        WAKE_SGIS[2].load(Ordering::Relaxed),
        WAKE_SGIS[3].load(Ordering::Relaxed),
        OTHER_IRQS.load(Ordering::Relaxed),
    );
    for target in 1..CPUS {
        println!("  CPU{}: power conf {:#x} status {:#x}", target, smp::power_config(target), smp::power_status(target));
    }
    let pass = start_errors == 0 && offline_timeouts == 0 && lost_rounds == 0;
    println!("Result: {}", if pass { "PASS" } else { "FAIL" });
    cpu::halt()
}
