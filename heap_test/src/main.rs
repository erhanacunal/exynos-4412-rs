//! Heap test: the `heap` feature's TLSF allocator, first on CPU0 alone, then on four cores at
//! once with interrupt handlers allocating too.
//!
//! 1. `Box`, a `Vec` grown to 4 MiB, a `String`, a `BTreeMap`.
//! 2. Time: random sizes allocated and freed on CPU0, each call timed on the MCT.
//! 3. A block of 768 MiB, written through; one of 1.5 GiB, more than the heap, which must fail.
//! 4. CPU1-3 started. Each core allocates, checks, resizes and frees blocks of random size and
//!    alignment, each filled with a pattern of its own; hands blocks to the next core, which
//!    checks and frees them; and sends the next core an SGI, whose handler allocates and frees.
//! 5. Every block freed: the heap must be back to nothing in use.
//!
//! Keys afterwards: `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::alloc::{alloc, dealloc, realloc};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::fmt::Write;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, heap, mmu, println, smp, soc};

exynos_4412_rs::entry!(main);
exynos_4412_rs::global_heap!();

const CPUS: usize = soc::CPU_COUNT;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

const TIMED_ROUNDS: u32 = 20_000;
const TIMED_SLOTS: usize = 256;
const BIG: usize = 768 * 1024 * 1024;
const TOO_BIG: usize = 1536 * 1024 * 1024;

/// Operations each core does in the stress phase.
const STRESS_OPS: u32 = 100_000;
const STRESS_SLOTS: usize = 128;
/// Every this many operations, a block goes to the next core, and an SGI.
const HANDOFF_EVERY: u32 = 64;
const SGI_EVERY: u32 = 256;
const HEAP_SGI: u16 = 2;

/// A block handed from core to core.
struct Parcel {
    from: usize,
    seed: u32,
    words: [u32; 64],
}

/// The parcel waiting for each core, or null.
static MAILBOX: [AtomicPtr<Parcel>; CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; CPUS];

#[derive(Clone, Copy, Default)]
struct CoreReport {
    ops: u32,
    allocations: u32,
    failures: u32,
    resizes: u32,
    corrupt: u32,
    sent: u32,
    received: u32,
    alloc_counts: u64,
    alloc_max: u64,
    frees: u32,
    free_counts: u64,
    free_max: u64,
}

static REPORTS: [smp::SpinLock<CoreReport>; CPUS] = [const {
    smp::SpinLock::new(CoreReport {
        ops: 0,
        allocations: 0,
        failures: 0,
        resizes: 0,
        corrupt: 0,
        sent: 0,
        received: 0,
        alloc_counts: 0,
        alloc_max: 0,
        frees: 0,
        free_counts: 0,
        free_max: 0,
    })
}; CPUS];
static IRQ_ALLOCS: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];
static IRQ_CORRUPT: [AtomicU32; CPUS] = [const { AtomicU32::new(0) }; CPUS];
static READY: AtomicU32 = AtomicU32::new(0);
static GO: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
/// MCT counts two back-to-back reads of the counter take, taken off each timed call.
static TIMER_COST: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    let me = cpu::id();
    if irq.id() == HEAP_SGI {
        // Allocates where the core may have been between two of its own calls.
        let length = 100 + (IRQ_ALLOCS[me].load(Ordering::Relaxed) as usize * 37) % 1000;
        let block: Vec<u8> = (0..length).map(|i| (i * 7 + me) as u8).collect();
        if block.iter().enumerate().any(|(i, &byte)| byte != (i * 7 + me) as u8) {
            IRQ_CORRUPT[me].fetch_add(1, Ordering::Relaxed);
        }
        drop(block);
        IRQ_ALLOCS[me].fetch_add(1, Ordering::Relaxed);
    } else if irq.id() != smp::WAKE_SGI {
        println!("CPU{} unexpected interrupt {}", me, irq.id());
    }
    gic.end_of_interrupt(irq);
}

/// xorshift32: different sequences per seed, the same on every run.
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

fn pattern(seed: u32, index: usize) -> u8 {
    (seed.wrapping_add(index as u32).wrapping_mul(0x9E37_79B1) >> 24) as u8
}

/// Fills `length` bytes from `start` with the pattern of `seed`, starting at `from`.
///
/// # Safety
///
/// `start..start + length` must be a block this core owns.
unsafe fn fill(start: *mut u8, from: usize, length: usize, seed: u32) {
    for index in from..length {
        // SAFETY: inside the block, as the caller promises.
        unsafe { start.add(index).write(pattern(seed, index)) };
    }
}

/// Whether the first `length` bytes from `start` hold the pattern of `seed`.
///
/// # Safety
///
/// As for [`fill`].
unsafe fn holds(start: *const u8, length: usize, seed: u32) -> bool {
    // SAFETY: inside the block, as the caller promises.
    (0..length).all(|index| unsafe { start.add(index).read() } == pattern(seed, index))
}

fn ns(counts: u64) -> u64 {
    counts * 1000 / COUNTS_PER_US
}

/// Times `call` on the MCT, less the cost of reading it.
fn timed<T>(mct: Mct, call: impl FnOnce() -> T) -> (T, u64) {
    let begin = mct.count();
    let result = call();
    let counts = (mct.count() - begin).saturating_sub(u64::from(TIMER_COST.load(Ordering::Relaxed)));
    (result, counts)
}

fn print_stats(label: &str) {
    let stats = heap::stats();
    println!(
        "{}: used {} B, peak {} B; {} allocations, {} frees, {} refused",
        label, stats.used, stats.peak, stats.allocations, stats.frees, stats.failures
    );
}

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

/// Step 1: the collections everyone uses.
fn basics() -> bool {
    let boxed = Box::new(0x1234_5678u32);
    let mut numbers: Vec<u32> = Vec::new();
    for value in 0..1024 * 1024 {
        numbers.push(value);
    }
    let sum: u64 = numbers.iter().map(|&value| u64::from(value)).sum();
    let mut text = String::new();
    let _ = write!(text, "{} numbers, the last {}", numbers.len(), numbers[numbers.len() - 1]);
    let mut map = BTreeMap::new();
    for key in 0..10_000u32 {
        map.insert(key.wrapping_mul(7919) % 10_007, key);
    }
    let good = *boxed == 0x1234_5678
        && sum == (1024 * 1024 - 1) * 1024 * 1024 / 2
        && text == "1048576 numbers, the last 1048575"
        && map.len() == 10_000
        && map.get(&0) == Some(&0);
    println!(
        "Basics: Box, Vec of 4 MiB grown one push at a time ({} B capacity), \"{}\", BTreeMap of {}: {}",
        numbers.capacity() * 4,
        text,
        map.len(),
        verdict(good)
    );
    good
}

/// Step 2: allocations and frees of 16 B to 4 KiB, timed one by one.
fn timing(mct: Mct) -> bool {
    let mut random = Random(0x2545_F491);
    let mut slots: [Option<(*mut u8, Layout)>; TIMED_SLOTS] = [None; TIMED_SLOTS];
    let (mut alloc_sum, mut alloc_max, mut free_sum, mut free_max, mut allocs, mut frees) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut good = true;
    for _ in 0..TIMED_ROUNDS {
        let r = random.next();
        let slot = &mut slots[r as usize % TIMED_SLOTS];
        match slot.take() {
            None => {
                let layout = Layout::from_size_align(16 + (r >> 8) as usize % 4080, 8).unwrap();
                // SAFETY: the layout has a size.
                let (block, counts) = timed(mct, || unsafe { alloc(layout) });
                good &= !block.is_null();
                if !block.is_null() {
                    *slot = Some((block, layout));
                }
                alloc_sum += counts;
                alloc_max = alloc_max.max(counts);
                allocs += 1;
            }
            Some((block, layout)) => {
                // SAFETY: allocated just so, above.
                let ((), counts) = timed(mct, || unsafe { dealloc(block, layout) });
                free_sum += counts;
                free_max = free_max.max(counts);
                frees += 1;
            }
        }
    }
    for (block, layout) in slots.iter().flatten() {
        // SAFETY: allocated just so, above.
        unsafe { dealloc(*block, *layout) };
    }
    println!(
        "Time, 16 B to 4 KiB on CPU0: {} allocations, mean {} ns, max {} ns; {} frees, mean {} ns, max {} ns",
        allocs,
        ns(alloc_sum / allocs.max(1)),
        ns(alloc_max),
        frees,
        ns(free_sum / frees.max(1)),
        ns(free_max)
    );
    good
}

/// Step 3: a block most of the heap long, and one longer than the heap.
fn extremes(mct: Mct) -> bool {
    let layout = Layout::from_size_align(BIG, 4096).unwrap();
    // SAFETY: the layout has a size.
    let (block, counts) = timed(mct, || unsafe { alloc(layout) });
    let (mut write_ms, mut read_ms) = (0, 0);
    let big_good = if block.is_null() {
        false
    } else {
        // SAFETY: the block is ours, BIG bytes long, 4096-aligned.
        let words = unsafe { core::slice::from_raw_parts_mut(block.cast::<u32>(), BIG / 4) };
        let begin = mct.count();
        for (index, word) in words.iter_mut().enumerate() {
            *word = index as u32 ^ 0xA5A5_A5A5;
        }
        let middle = mct.count();
        let good = words.iter().enumerate().all(|(index, &word)| word == index as u32 ^ 0xA5A5_A5A5);
        write_ms = (middle - begin) / COUNTS_PER_US / 1000;
        read_ms = (mct.count() - middle) / COUNTS_PER_US / 1000;
        // SAFETY: allocated just so, above.
        unsafe { dealloc(block, layout) };
        good
    };
    println!(
        "Extremes: {} MiB at {:#010x} in {} ns; written in {} ms, read in {} ms: {}",
        BIG >> 20,
        block as usize,
        ns(counts),
        write_ms,
        read_ms,
        verdict(big_good)
    );
    let before = heap::stats().failures;
    let too_big = Layout::from_size_align(TOO_BIG, 8).unwrap();
    // SAFETY: the layout has a size.
    let refused = unsafe { alloc(too_big) };
    let refused_good = refused.is_null() && heap::stats().failures == before + 1;
    println!("Extremes: {} MiB refused with null, counted: {}", TOO_BIG >> 20, verdict(refused_good));
    big_good && refused_good
}

/// Step 4, on every core.
fn stress(me: usize) {
    let mct = Mct::new();
    let gic = Gic::new();
    let next = (me + 1) % CPUS;
    let mut random = Random(0x1000_0001 + me as u32 * 0x0101_0101);
    let mut slots: [Option<(*mut u8, Layout, u32)>; STRESS_SLOTS] = [None; STRESS_SLOTS];
    let mut report = CoreReport::default();
    let mut seed = me as u32 * 0x0100_0000;

    for op in 0..STRESS_OPS {
        let r = random.next();
        let slot = &mut slots[r as usize % STRESS_SLOTS];
        match slot.take() {
            None => {
                let length = if r & 0x1F == 0 { 1 + (r >> 8) as usize % 65536 } else { 1 + (r >> 8) as usize % 512 };
                let layout = Layout::from_size_align(length, 1 << ((r >> 5) % 7)).unwrap();
                // SAFETY: the layout has a size.
                let (block, counts) = timed(mct, || unsafe { alloc(layout) });
                report.alloc_counts += counts;
                report.alloc_max = report.alloc_max.max(counts);
                if block.is_null() {
                    report.failures += 1;
                } else {
                    report.allocations += 1;
                    report.corrupt += u32::from(!(block as usize).is_multiple_of(layout.align()));
                    seed = seed.wrapping_add(1);
                    // SAFETY: the block is ours and `length` long.
                    unsafe { fill(block, 0, length, seed) };
                    *slot = Some((block, layout, seed));
                }
            }
            Some((block, layout, seed)) => {
                // SAFETY: the block is ours and `layout.size()` long.
                report.corrupt += u32::from(!unsafe { holds(block, layout.size(), seed) });
                if r & 0x7 == 0 {
                    // Resize, then check what was kept.
                    let length = 1 + (r >> 8) as usize % 2048;
                    // SAFETY: allocated with `layout`; the new size is not zero.
                    let moved = unsafe { realloc(block, layout, length) };
                    if moved.is_null() {
                        report.failures += 1;
                        *slot = Some((block, layout, seed));
                    } else {
                        report.resizes += 1;
                        let kept = length.min(layout.size());
                        // SAFETY: the block is ours and `length` long.
                        unsafe {
                            report.corrupt += u32::from(!holds(moved, kept, seed));
                            fill(moved, kept, length, seed);
                        }
                        *slot = Some((moved, Layout::from_size_align(length, layout.align()).unwrap(), seed));
                    }
                } else {
                    // SAFETY: allocated just so.
                    let ((), counts) = timed(mct, || unsafe { dealloc(block, layout) });
                    report.frees += 1;
                    report.free_counts += counts;
                    report.free_max = report.free_max.max(counts);
                }
            }
        }

        if op % HANDOFF_EVERY == 0 {
            let parcel = MAILBOX[me].swap(core::ptr::null_mut(), Ordering::Acquire);
            if !parcel.is_null() {
                // SAFETY: another core made it with `Box::into_raw` and let go of it.
                let parcel = unsafe { Box::from_raw(parcel) };
                let good = parcel.from == (me + CPUS - 1) % CPUS
                    && parcel.words.iter().enumerate().all(|(i, &word)| word == parcel.seed ^ i as u32);
                report.corrupt += u32::from(!good);
                report.received += 1;
            }
            if MAILBOX[next].load(Ordering::Relaxed).is_null() {
                let seed = random.next();
                let mut parcel = Box::new(Parcel { from: me, seed, words: [0; 64] });
                for (i, word) in parcel.words.iter_mut().enumerate() {
                    *word = seed ^ i as u32;
                }
                // Only `me` fills the next core's mailbox, so it is still empty.
                MAILBOX[next].store(Box::into_raw(parcel), Ordering::Release);
                report.sent += 1;
            }
        }
        if op % SGI_EVERY == 0 {
            gic.send_sgi(next, HEAP_SGI);
        }
        report.ops += 1;
    }

    for (block, layout, seed) in slots.iter().flatten() {
        // SAFETY: allocated just so.
        unsafe {
            report.corrupt += u32::from(!holds(*block, layout.size(), *seed));
            dealloc(*block, *layout);
        }
    }
    *REPORTS[me].lock() = report;
    DONE.fetch_add(1, Ordering::AcqRel);
}

fn secondary(me: usize) -> ! {
    let gic = Gic::new();
    gic.init_this_cpu();
    gic.enable(HEAP_SGI);
    cpu::enable_irq();
    READY.fetch_add(1, Ordering::AcqRel);
    while GO.load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }
    stress(me);
    loop {
        core::hint::spin_loop();
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nHeap test: TLSF on one core, then on four");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    let gic = Gic::new();
    gic.init();
    gic.enable(HEAP_SGI);

    let begin = mct.count();
    let size = match heap::init() {
        Ok(size) => size,
        Err(error) => {
            println!("heap::init ERROR {:?}", error);
            console::flush();
            cpu::halt();
        }
    };
    println!("Heap: {} MiB, given in {} us", size >> 20, (mct.count() - begin) / COUNTS_PER_US);
    let probe = mct.count();
    TIMER_COST.store((mct.count() - probe) as u32, Ordering::Relaxed);

    let mut good = basics();
    good &= timing(mct);
    good &= extremes(mct);
    print_stats("After steps 1-3");
    let used_before = heap::stats().used;

    for target in 1..CPUS {
        if let Err(error) = smp::start_cpu(target, secondary) {
            println!("CPU{} ERROR {:?}", target, error);
            console::flush();
            cpu::halt();
        }
    }
    while READY.load(Ordering::Acquire) != CPUS as u32 - 1 {
        core::hint::spin_loop();
    }
    cpu::enable_irq();
    println!("Stress: {} operations on each of {} cores", STRESS_OPS, CPUS);
    let begin = mct.count();
    GO.store(1, Ordering::Release);
    stress(0);
    while DONE.load(Ordering::Acquire) != CPUS as u32 {
        core::hint::spin_loop();
    }
    let ms = (mct.count() - begin) / COUNTS_PER_US / 1000;
    // Parcels nobody took yet.
    for mailbox in &MAILBOX {
        let parcel = mailbox.swap(core::ptr::null_mut(), Ordering::Acquire);
        if !parcel.is_null() {
            // SAFETY: made with `Box::into_raw` by a core that has finished.
            drop(unsafe { Box::from_raw(parcel) });
        }
    }

    let mut corrupt = 0;
    let mut failures = 0;
    for (me, report) in REPORTS.iter().enumerate() {
        let r = *report.lock();
        corrupt += r.corrupt + IRQ_CORRUPT[me].load(Ordering::Relaxed);
        failures += r.failures;
        println!(
            "CPU{}: {} ops, {} allocs, {} resizes, {} refused, {} corrupt; parcels {} out, {} in; {} in IRQ",
            me,
            r.ops,
            r.allocations,
            r.resizes,
            r.failures,
            r.corrupt + IRQ_CORRUPT[me].load(Ordering::Relaxed),
            r.sent,
            r.received,
            IRQ_ALLOCS[me].load(Ordering::Relaxed)
        );
        println!(
            "  alloc mean {} ns, max {} ns; free mean {} ns, max {} ns",
            ns(r.alloc_counts / u64::from(r.allocations + r.failures).max(1)),
            ns(r.alloc_max),
            ns(r.free_counts / u64::from(r.frees).max(1)),
            ns(r.free_max)
        );
    }
    println!("Stress: {} ms; corrupt {}, refused {}: {}", ms, corrupt, failures, verdict(corrupt == 0 && failures == 0));
    print_stats("After stress");
    let leak_free = heap::stats().used == used_before && used_before == 0;
    println!("Nothing left in use: {}", verdict(leak_free));
    good &= corrupt == 0 && failures == 0 && leak_free;
    println!("Heap test: {}. Keys: r restart, f fastboot", verdict(good));

    let uart = console::uart();
    loop {
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => core::hint::spin_loop(),
        }
    }
}
