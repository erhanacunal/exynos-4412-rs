//! SMP test. Starts CPU1-3, then checks that they really share memory: an atomic counter, a
//! lock around a plain counter, data handed from core to core, and SGIs around a ring. It
//! also measures random-access latency with the L2 cache off and on, by pointer chasing.
//!
//! Then come three timed phases under a 1 Hz timer interrupt routed to CPU1, all with CPU0
//! echoing the UART and CPU3 reporting once a second without taking the console lock, so a
//! stuck core or lock still shows in the log:
//!
//! 1. the working cores only poll;
//! 2. they store in a tight loop, each to a cache line of its own;
//! 3. they store in a tight loop, all to one cache line.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering, fence};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::scu::Scu;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::smp::{self, SpinLock};
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

const CPUS: usize = soc::CPU_COUNT;

/// PL310 line: Linux 5.15.198 `arch/arm/mm/cache-l2x0.c:37`. Cortex-A9 L1 lines are as long.
const LINE_WORDS: usize = 32 / 4;
/// Working sets: inside L1 (32 KiB), inside L2 (1 MiB), and well beyond both.
const LATENCY_SIZES_KIB: [usize; 6] = [16, 128, 512, 768, 2048, 8192];
const CHASE_WORDS: usize = 8192 * 1024 / 4;
/// Every line of the largest set is visited about once, the smaller ones many times.
const CHASE_STEPS: u32 = 1 << 18;

const ATOMIC_ROUNDS: u32 = 1_000_000;
const LOCK_ROUNDS: u32 = 200_000;
const HANDOFF_WORDS: usize = 256 * 1024 / 4;
const HANDOFF_ROUNDS: u32 = 16;
const RING_ROUNDS: u32 = 1000;
/// The core that reports during the timed phases.
const REPORTER: usize = 3;

/// Passes a token around the ring CPU0, 1, 2, 3, 0.
const RING_SGI: u16 = 1;

static CHASE: [AtomicU32; CHASE_WORDS] = [const { AtomicU32::new(0) }; CHASE_WORDS];
static HANDOFF: [AtomicU32; HANDOFF_WORDS] = [const { AtomicU32::new(0) }; HANDOFF_WORDS];

/// Which test the secondary cores should run next, advanced by CPU0.
static STAGE: AtomicU32 = AtomicU32::new(0);
const STAGE_ATOMIC: u32 = 1;
const STAGE_LOCK: u32 = 2;
const STAGE_HANDOFF: u32 = 3;
const STAGE_PHASES: u32 = 4;
/// How many cores have finished the current stage.
static DONE: AtomicU32 = AtomicU32::new(0);
/// How many cores wait at the start of the current stage.
static READY: AtomicU32 = AtomicU32::new(0);

static SHARED_ATOMIC: AtomicU32 = AtomicU32::new(0);
/// Deliberately a plain read and write under the lock: lost updates would show.
static SHARED_LOCKED: SpinLock<u32> = SpinLock::new(0);

/// Whose turn it is in the handoff test, and the seed to verify.
static HANDOFF_TURN: AtomicU32 = AtomicU32::new(0);

static RING_ARRIVALS: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];
static TICKS: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];
static STRAY_SGI: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];

/// One counter per core in cache lines of their own.
#[repr(align(32))]
struct Padded(AtomicU32);
static WORK_PADDED: [Padded; CPUS] = [const { Padded(AtomicU32::new(0)) }; CPUS];
/// One counter per core, all in one cache line.
#[repr(align(32))]
struct SameLine([AtomicU32; CPUS]);
static WORK_SAME_LINE: SameLine = SameLine([const { AtomicU32::new(0) }; CPUS]);

const PHASE_POLL: u32 = 1;
const PHASE_OWN_LINE: u32 = 2;
const PHASE_SAME_LINE: u32 = 3;
const PHASE_LAST: u32 = PHASE_SAME_LINE;
const PHASE_SECONDS: u32 = 6;
/// Set by CPU3, the only core guaranteed not to depend on the others here.
static PHASE: AtomicU32 = AtomicU32::new(PHASE_POLL);
/// Stores between two looks at the phase. Bounded on purpose: see "Store loops" in `smp`.
const STORE_BURST: u32 = 1 << 20;

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    let me = cpu::id();
    match irq.id() {
        soc::MCT_G0_IRQ => {
            Mct::new().clear_interrupt();
            TICKS[me].fetch_add(1, Ordering::Relaxed);
        }
        RING_SGI => {
            RING_ARRIVALS[me].fetch_add(1, Ordering::Release);
            // CPU0 starts each lap and sees it end.
            if me != 0 {
                gic.send_sgi((me + 1) % CPUS, RING_SGI);
            }
        }
        smp::WAKE_SGI => {
            STRAY_SGI[me].fetch_add(1, Ordering::Relaxed);
        }
        other => println!("CPU{} unexpected interrupt: {}", me, other),
    }
    gic.end_of_interrupt(irq);
}

fn wait_for_stage(stage: u32) {
    READY.fetch_add(1, Ordering::AcqRel);
    while STAGE.load(Ordering::Acquire) != stage {
        core::hint::spin_loop();
    }
}

fn finish_stage() {
    DONE.fetch_add(1, Ordering::AcqRel);
}

fn work_atomic() {
    for _ in 0..ATOMIC_ROUNDS {
        SHARED_ATOMIC.fetch_add(1, Ordering::Relaxed);
    }
}

fn work_lock() {
    for _ in 0..LOCK_ROUNDS {
        let mut counter = SHARED_LOCKED.lock();
        *counter += 1;
    }
}

fn handoff_pattern(index: usize, round: u32) -> u32 {
    (index as u32).wrapping_mul(0x9E37_79B1) ^ round
}

/// Each core in turn verifies what the previous core wrote and writes the next round, so
/// the data lives dirty in another core's cache whenever it is read.
fn work_handoff(me: usize) {
    let mut failures = 0u32;
    for turn in 0..HANDOFF_ROUNDS * CPUS as u32 {
        if turn as usize % CPUS != me {
            continue;
        }
        while HANDOFF_TURN.load(Ordering::Acquire) != turn {
            core::hint::spin_loop();
        }
        if turn != 0 {
            for (index, word) in HANDOFF.iter().enumerate() {
                if word.load(Ordering::Relaxed) != handoff_pattern(index, turn - 1) {
                    failures += 1;
                }
            }
        }
        for (index, word) in HANDOFF.iter().enumerate() {
            word.store(handoff_pattern(index, turn), Ordering::Relaxed);
        }
        HANDOFF_TURN.store(turn + 1, Ordering::Release);
    }
    assert!(failures == 0, "CPU{} handoff failure: {} words", me, failures);
}

fn secondary(me: usize) -> ! {
    let gic = Gic::new();
    gic.init_this_cpu();
    gic.enable(RING_SGI);

    println!(
        "CPU{} up: MPIDR {:#010x} SCTLR {:#010x} ACTLR {:#010x} CPSR {:#010x} SP {:#010x}",
        me,
        cpu::read_mpidr(),
        cpu::read_sctlr(),
        cpu::read_actlr(),
        cpu::read_cpsr(),
        &raw const me as usize,
    );
    cpu::enable_irq();

    wait_for_stage(STAGE_ATOMIC);
    work_atomic();
    finish_stage();

    wait_for_stage(STAGE_LOCK);
    work_lock();
    finish_stage();

    wait_for_stage(STAGE_HANDOFF);
    work_handoff(me);
    finish_stage();

    wait_for_stage(STAGE_PHASES);
    if me == REPORTER {
        report_phases();
    }
    loop {
        store_burst_or_poll(me);
    }
}

/// One slice of a working core's job in the current phase.
fn store_burst_or_poll(me: usize) {
    let counter = match PHASE.load(Ordering::Relaxed) {
        PHASE_OWN_LINE => &WORK_PADDED[me].0,
        PHASE_SAME_LINE => &WORK_SAME_LINE.0[me],
        _ => {
            core::hint::spin_loop();
            return;
        }
    };
    let mut value = counter.load(Ordering::Relaxed);
    for _ in 0..STORE_BURST {
        value = value.wrapping_add(1);
        counter.store(value, Ordering::Relaxed);
    }
}

/// A core's counter, for the difference it makes each second.
fn work(cpu: usize, phase: u32) -> u32 {
    match phase {
        PHASE_SAME_LINE => WORK_SAME_LINE.0[cpu].load(Ordering::Relaxed),
        _ => WORK_PADDED[cpu].0.load(Ordering::Relaxed),
    }
}

/// CPU3's job: advances the phases and reports, writing straight to the UART so that
/// neither a stuck core nor the console lock can silence it.
fn report_phases() -> ! {
    let mct = Mct::new();
    let gic = Gic::new();
    let mut uart = console::uart();
    let mut seconds = 0u32;
    let mut last = [0u32; CPUS];
    let mut next = mct.count() + u64::from(soc::MCT_CLOCK_HZ);
    loop {
        let phase = PHASE.load(Ordering::Relaxed);
        store_burst_or_poll(REPORTER);
        if mct.count() < next {
            continue;
        }
        next += u64::from(soc::MCT_CLOCK_HZ);
        seconds += 1;

        let now: [u32; CPUS] = core::array::from_fn(|cpu| work(cpu, phase));
        let (pending, active) = gic.state(soc::MCT_G0_IRQ);
        let _ = writeln!(
            uart,
            "[phase {} s {}] work/s: {} {} {} {}  ticks: {} {} {} {}  G0 pending {} active {} target {:#x}  lock {:?}",
            phase,
            seconds,
            now[0].wrapping_sub(last[0]),
            now[1].wrapping_sub(last[1]),
            now[2].wrapping_sub(last[2]),
            now[3].wrapping_sub(last[3]),
            TICKS[0].load(Ordering::Relaxed),
            TICKS[1].load(Ordering::Relaxed),
            TICKS[2].load(Ordering::Relaxed),
            TICKS[3].load(Ordering::Relaxed),
            u8::from(pending),
            u8::from(active),
            gic.target_mask(soc::MCT_G0_IRQ),
            console::lock_owner(),
        );
        last = now;

        if seconds.is_multiple_of(PHASE_SECONDS) && phase < PHASE_LAST {
            let next_phase = phase + 1;
            PHASE.store(next_phase, Ordering::Relaxed);
            last = core::array::from_fn(|cpu| work(cpu, next_phase));
        }
    }
}

/// Links the first `words` words, one word per cache line, into one random cycle
/// (Sattolo's algorithm), so neither the stride nor the order helps a prefetcher.
fn build_chase(words: usize) {
    let lines = words / LINE_WORDS;
    for line in 0..lines {
        CHASE[line * LINE_WORDS].store(line as u32, Ordering::Relaxed);
    }
    let mut random = 0x1234_5678u32;
    for last in (1..lines).rev() {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        let pick = random as usize % last;
        let a = CHASE[last * LINE_WORDS].load(Ordering::Relaxed);
        let b = CHASE[pick * LINE_WORDS].load(Ordering::Relaxed);
        CHASE[last * LINE_WORDS].store(b, Ordering::Relaxed);
        CHASE[pick * LINE_WORDS].store(a, Ordering::Relaxed);
    }
}

/// Picoseconds per step of a random pointer chase.
fn chase_ps(mct: Mct) -> u64 {
    let mut line = 0usize;
    let start = mct.count();
    for _ in 0..CHASE_STEPS {
        line = CHASE[line * LINE_WORDS].load(Ordering::Relaxed) as usize;
    }
    let counts = mct.count() - start;
    // The compiler cannot drop the loads of an atomic, but keep the result visibly used.
    assert!(line * LINE_WORDS < CHASE_WORDS);
    counts * 1_000_000 / u64::from(soc::MCT_CLOCK_HZ / 1_000_000) / u64::from(CHASE_STEPS)
}

fn report_latency(mct: Mct, label: &str) {
    print!("{}:", label);
    for kib in LATENCY_SIZES_KIB {
        build_chase(kib * 1024 / 4);
        // The first pass only warms the caches.
        chase_ps(mct);
        let ps = chase_ps(mct);
        print!(" {}K={}.{}ns", kib, ps / 1000, ps / 100 % 10);
    }
    println!();
}

/// Lets the secondaries through `stage`, runs `work` here too, waits for all, returns ms.
fn run_stage(mct: Mct, stage: u32, work: impl FnOnce()) -> u64 {
    while READY.load(Ordering::Acquire) != CPUS as u32 - 1 {
        core::hint::spin_loop();
    }
    READY.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);
    fence(Ordering::SeqCst);
    let start = mct.count();
    STAGE.store(stage, Ordering::Release);
    work();
    while DONE.load(Ordering::Acquire) != CPUS as u32 - 1 {
        core::hint::spin_loop();
    }
    (mct.count() - start) / (u64::from(soc::MCT_CLOCK_HZ) / 1000)
}

fn ring_test(mct: Mct, gic: Gic) {
    let expected = |cpu: usize, laps: u32| RING_ARRIVALS[cpu].load(Ordering::Acquire) == laps;
    let start = mct.count();
    for lap in 1..=RING_ROUNDS {
        gic.send_sgi(1, RING_SGI);
        let lap_start = mct.count();
        while !expected(0, lap) {
            if mct.count() - lap_start > u64::from(soc::MCT_CLOCK_HZ) {
                println!(
                    "SGI ring stopped at lap {}, arrivals: {} {} {} {}",
                    lap,
                    RING_ARRIVALS[0].load(Ordering::Relaxed),
                    RING_ARRIVALS[1].load(Ordering::Relaxed),
                    RING_ARRIVALS[2].load(Ordering::Relaxed),
                    RING_ARRIVALS[3].load(Ordering::Relaxed),
                );
                return;
            }
        }
    }
    let counts = mct.count() - start;
    println!(
        "SGI ring: {} laps, {} ns per lap, arrivals {} {} {} {}",
        RING_ROUNDS,
        counts * 1000 / u64::from(soc::MCT_CLOCK_HZ / 1_000_000) / u64::from(RING_ROUNDS),
        RING_ARRIVALS[0].load(Ordering::Relaxed),
        RING_ARRIVALS[1].load(Ordering::Relaxed),
        RING_ARRIVALS[2].load(Ordering::Relaxed),
        RING_ARRIVALS[3].load(Ordering::Relaxed),
    );
}

fn main(_args: BootArgs) -> ! {
    println!("\nSMP test, MIDR {:#010x}", cpu::read_midr());
    let scu = Scu::new();
    println!(
        "Before: SCU CTRL {:#x} CONFIG {:#010x}, ACTLR {:#010x}, power state {:#x} {:#x} {:#x} {:#x}",
        scu.ctrl(),
        scu.config(),
        cpu::read_actlr(),
        smp::power_status(0),
        smp::power_status(1),
        smp::power_status(2),
        smp::power_status(3),
    );

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    println!(
        "MMU on: SCU CTRL {:#x} ({} cores), SCTLR {:#010x}, ACTLR {:#010x}",
        scu.ctrl(),
        scu.core_count(),
        cpu::read_sctlr(),
        cpu::read_actlr(),
    );

    report_latency(mct, "Latency L2 off");
    let l2c = L2c::new();
    console::uart().flush();
    l2c.enable();
    println!("L2 CTRL {:#x}", l2c.ctrl());
    report_latency(mct, "Latency L2 on ");

    let gic = Gic::new();
    gic.init();
    gic.enable(RING_SGI);

    for target in 1..CPUS {
        println!("CPU{} starting, previous entry address {:#010x}", target, smp::boot_address(target));
        let start = mct.count();
        let result = smp::start_cpu(target, secondary);
        let us = (mct.count() - start) / u64::from(soc::MCT_CLOCK_HZ / 1_000_000);
        println!();
        match result {
            Ok(()) => {
                println!("CPU{} reported, {} us", target, us);
                // Keeps the log in order: its first line comes before the next core's.
                let wait_start = mct.count();
                while READY.load(Ordering::Acquire) != target as u32 {
                    if mct.count() - wait_start > u64::from(soc::MCT_CLOCK_HZ) {
                        println!("CPU{} did not become ready", target);
                        cpu::halt();
                    }
                }
            }
            Err(error) => {
                println!(
                    "CPU{} ERROR {:?}, power state {:#x}, entry address {:#010x}",
                    target,
                    error,
                    smp::power_status(target),
                    smp::boot_address(target),
                );
                cpu::halt();
            }
        }
    }
    cpu::enable_irq();

    let ms = run_stage(mct, STAGE_ATOMIC, work_atomic);
    let total = SHARED_ATOMIC.load(Ordering::Acquire);
    println!("Atomic counter: {} expected {}, {} ms", total, ATOMIC_ROUNDS * CPUS as u32, ms);
    assert!(total == ATOMIC_ROUNDS * CPUS as u32);

    let ms = run_stage(mct, STAGE_LOCK, work_lock);
    let total = *SHARED_LOCKED.lock();
    println!("Locked counter: {} expected {}, {} ms", total, LOCK_ROUNDS * CPUS as u32, ms);
    assert!(total == LOCK_ROUNDS * CPUS as u32);

    let ms = run_stage(mct, STAGE_HANDOFF, || work_handoff(0));
    println!("Data handoff: {} rounds, no errors, {} ms", HANDOFF_ROUNDS * CPUS as u32, ms);

    ring_test(mct, gic);

    println!(
        "Leftover wake SGIs: {} {} {} {}",
        STRAY_SGI[0].load(Ordering::Relaxed),
        STRAY_SGI[1].load(Ordering::Relaxed),
        STRAY_SGI[2].load(Ordering::Relaxed),
        STRAY_SGI[3].load(Ordering::Relaxed),
    );

    // Delivered on CPU1, enabled from here: proves SPI routing and that core's interrupts.
    gic.enable(soc::MCT_G0_IRQ);
    gic.set_target(soc::MCT_G0_IRQ, 1);
    mct.start_periodic(soc::MCT_CLOCK_HZ);
    println!("Phases starting. Keep pressing keys: the echo comes from CPU0.");

    while READY.load(Ordering::Acquire) != CPUS as u32 - 1 {
        core::hint::spin_loop();
    }
    STAGE.store(STAGE_PHASES, Ordering::Release);

    let uart = console::uart();
    loop {
        store_burst_echo(uart);
    }
}

/// CPU0's job in each phase: a short store burst, so it keeps looking at the UART.
fn store_burst_echo(uart: exynos_4412_rs::drivers::uart::Uart) {
    let counter = match PHASE.load(Ordering::Relaxed) {
        PHASE_SAME_LINE => &WORK_SAME_LINE.0[0],
        _ => &WORK_PADDED[0].0,
    };
    let value = counter.load(Ordering::Relaxed).wrapping_add(1);
    counter.store(value, Ordering::Relaxed);
    match uart.try_read_byte() {
        Some(b'\r') => println!(),
        // Marked, to tell this from a terminal's own local echo.
        Some(byte) => print!("<{}>", byte as char),
        None => {}
    }
}
