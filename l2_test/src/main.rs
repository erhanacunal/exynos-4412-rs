//! L2 cache test. Checks the MCT rate against the UART, measures memory speed at three
//! working set sizes with the L2 cache off and on, checks that data written before the
//! switch survives it, checks that dropping a range from the caches brings back what DRAM
//! holds, then keeps verifying memory under a 1 Hz timer interrupt.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::scu::Scu;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::uart::Uart;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

const WORDS_L1: usize = 16 * 1024 / 4;
const WORDS_L2: usize = 256 * 1024 / 4;
/// Twice the L2 cache size.
const WORDS_BEYOND: usize = 2 * 1024 * 1024 / 4;

/// Word accesses per measurement, so every working set does the same amount of work.
const ACCESSES: usize = 8 * 1024 * 1024;

/// 8N1 at 115200 baud is 11520 characters per second, so 1152 take 100 ms.
const UART_CHARS_PER_SECOND: u64 = 11_520;
const UART_LINES: u64 = 16;
const UART_LINE_CHARS: u64 = 72;

static BUFFER: [AtomicU32; WORDS_BEYOND] = [const { AtomicU32::new(0) }; WORDS_BEYOND];

/// The range the cache drop tests cover: four times the L1 cache, so that of what the CPU writes
/// last, most has gone on to the L2 cache and the rest is still in L1.
const RANGE_BYTES: usize = 128 * 1024;
const RANGE_WORDS: usize = RANGE_BYTES / 4;
const LINE_WORDS: usize = mmu::DCACHE_LINE / 4;
/// What DRAM holds, and what the CPU writes after, for word `index`.
const fn old_word(index: usize) -> u32 {
    0x0D0D_0000 ^ index as u32
}
const fn new_word(index: usize) -> u32 {
    0xC0C0_0000 ^ index as u32
}

/// The range, between two guard lines that must not be touched.
#[repr(C, align(32))]
struct Guarded {
    before: [AtomicU32; LINE_WORDS],
    range: [AtomicU32; RANGE_WORDS],
    after: [AtomicU32; LINE_WORDS],
}

static GUARDED: Guarded = Guarded {
    before: [const { AtomicU32::new(0) }; LINE_WORDS],
    range: [const { AtomicU32::new(0) }; RANGE_WORDS],
    after: [const { AtomicU32::new(0) }; LINE_WORDS],
};
static TICKS: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if irq.id() == soc::MCT_G0_IRQ {
        Mct::new().clear_interrupt();
        TICKS.fetch_add(1, Ordering::Relaxed);
    } else {
        println!("unexpected interrupt: {}", irq.id());
    }
    gic.end_of_interrupt(irq);
}

/// Times a known amount of UART output against the MCT. The terminal being readable means
/// the baud rate is right to within a few percent, which bounds the MCT rate as closely.
fn measure_mct_against_uart(mct: Mct, uart: Uart) -> u64 {
    uart.flush();
    let start = mct.count();
    for _ in 0..UART_LINES {
        for _ in 0..UART_LINE_CHARS - 2 {
            uart.write_byte(b'-');
        }
        uart.write_byte(b'\r');
        uart.write_byte(b'\n');
    }
    uart.flush();
    let counts = mct.count() - start;
    counts * UART_CHARS_PER_SECOND / (UART_LINES * UART_LINE_CHARS)
}

fn fill(words: usize, seed: u32) {
    for (index, word) in BUFFER[..words].iter().enumerate() {
        word.store(seed ^ (index as u32).wrapping_mul(0x9E37_79B1), Ordering::Relaxed);
    }
}

fn verify(words: usize, seed: u32) {
    for (index, word) in BUFFER[..words].iter().enumerate() {
        let expected = seed ^ (index as u32).wrapping_mul(0x9E37_79B1);
        let actual = word.load(Ordering::Relaxed);
        if actual != expected {
            panic!("memory error: index {} expected {:#x} read {:#x}", index, expected, actual);
        }
    }
}

/// Nanoseconds per word for repeated write and verify passes over `words` words.
fn benchmark(mct: Mct, words: usize) -> u64 {
    let passes = (ACCESSES / (2 * words)).max(1);
    let start = mct.count();
    for pass in 0..passes {
        let seed = pass as u32;
        fill(words, seed);
        verify(words, seed);
    }
    let counts = mct.count() - start;
    let accesses = (2 * words * passes) as u64;
    counts * 1000 / u64::from(soc::MCT_CLOCK_HZ / 1_000_000) / accesses
}

fn report_speed(mct: Mct, label: &str) {
    println!(
        "{}: 16 KiB {} ns, 256 KiB {} ns, 2 MiB {} ns",
        label,
        benchmark(mct, WORDS_L1),
        benchmark(mct, WORDS_L2),
        benchmark(mct, WORDS_BEYOND),
    );
}

fn store_all(words: &[AtomicU32], value: fn(usize) -> u32) {
    for (index, word) in words.iter().enumerate() {
        word.store(value(index), Ordering::Relaxed);
    }
}

/// How many words of `words` read `value` of their index.
fn count_matching(words: &[AtomicU32], value: fn(usize) -> u32) -> usize {
    words.iter().enumerate().filter(|(index, word)| word.load(Ordering::Relaxed) == value(*index)).count()
}

/// Rounds of each drop test: speculation and prefetch make a single run show little.
const DROP_ROUNDS: u32 = 16;

/// What a drop call must leave behind. Either may write what the CPU wrote out to DRAM before
/// it returns, as the caches may on their own: before a device starts, that does no harm.
/// The board does both (see README).
#[derive(Clone, Copy, PartialEq)]
enum Promise {
    /// No dirty line: nothing reaches DRAM after it returns. Reads right after may differ
    /// from DRAM, as a line can come back clean into L1 from L2 between the two steps.
    NothingDirty,
    /// That, and reads give what DRAM holds, right away.
    ReadsFromDram,
}

/// The uncached address of a word of the test range.
fn uncached_word(words: &[AtomicU32], index: usize) -> u32 {
    let start = mmu::uncached(words.as_ptr() as usize).expect("test range outside the uncached view");
    // SAFETY: `uncached` maps the address of a live static to the same DRAM, readable, aligned
    // as the original, and `index` is within it. Reading changes nothing in caches or DRAM.
    unsafe { core::ptr::read_volatile((start as *const u32).add(index)) }
}

/// How many words of `words` read differently through the caches than in DRAM.
fn differ_from_dram(words: &[AtomicU32]) -> usize {
    words
        .iter()
        .enumerate()
        .filter(|(index, word)| word.load(Ordering::Relaxed) != uncached_word(words, *index))
        .count()
}

/// How many words of `words` DRAM itself holds the old value for, read through
/// [`mmu::DRAM_UNCACHED`], past both caches.
fn dram_old(words: &[AtomicU32]) -> usize {
    (0..words.len()).filter(|index| uncached_word(words, *index) == old_word(*index)).count()
}

/// Puts the old values in DRAM, writes the new ones over them in the caches only, then lets
/// `drop` discard the range. DRAM is read through [`mmu::DRAM_UNCACHED`] before and after,
/// and again after reading 2 MiB elsewhere, which pushes the range out of both caches: a dirty
/// line left behind would then have reached DRAM. The range is read through the caches right
/// after the drop and after the 2 MiB, and compared with DRAM. The guard lines on both sides must
/// keep the new values throughout.
fn drop_test(mct: Mct, name: &str, drop: fn(usize, usize), promise: Promise) {
    let start = GUARDED.range.as_ptr() as usize;
    let mut good_rounds = 0;
    let mut written_before = 0;
    let mut written_during = 0;
    for round in 1..=DROP_ROUNDS {
        // Dirty lines elsewhere fill the L2 cache first, so that it has to write them out while
        // the range comes in: the first round of the first run was the only one to fail.
        fill(WORDS_BEYOND, round);
        store_all(&GUARDED.before, old_word);
        store_all(&GUARDED.range, old_word);
        store_all(&GUARDED.after, old_word);
        mmu::clean_dcache_range(GUARDED.before.as_ptr() as usize, size_of::<Guarded>());
        let dram_cleaned = dram_old(&GUARDED.range);
        store_all(&GUARDED.before, new_word);
        store_all(&GUARDED.range, new_word);
        store_all(&GUARDED.after, new_word);
        // Before the drop, DRAM should still hold every old value: the new ones are only in
        // the caches. A line written out by the caches on their own shows here.
        let dram_before = dram_old(&GUARDED.range);

        let begin = mct.count();
        drop(start, RANGE_BYTES);
        let us = (mct.count() - begin) / u64::from(soc::MCT_CLOCK_HZ / 1_000_000);
        let dram_after = dram_old(&GUARDED.range);

        let differ_right_after = differ_from_dram(&GUARDED.range);
        let mut sum = 0u32;
        for word in &BUFFER {
            sum = sum.wrapping_add(word.load(Ordering::Relaxed));
        }
        core::hint::black_box(sum);
        // A dirty line left behind would have been written out by now.
        let dram_final = dram_old(&GUARDED.range);
        let differ_final = differ_from_dram(&GUARDED.range);
        let guards = count_matching(&GUARDED.before, new_word) + count_matching(&GUARDED.after, new_word);

        let good = dram_cleaned == RANGE_WORDS
            && dram_final == dram_after
            && differ_final == 0
            && guards == 2 * LINE_WORDS
            && (promise == Promise::NothingDirty || differ_right_after == 0);
        if good {
            good_rounds += 1;
        }
        written_before += RANGE_WORDS - dram_before;
        written_during += dram_before - dram_after;
        println!(
            "{} {}: {} us; new words in DRAM before drop {}, after {}, after 2 MiB elsewhere {}; \
             reads differing from DRAM right after {}, after 2 MiB {}; guard words {} of {}",
            name,
            round,
            us,
            RANGE_WORDS - dram_before,
            RANGE_WORDS - dram_after,
            RANGE_WORDS - dram_final,
            differ_right_after,
            differ_final,
            guards,
            2 * LINE_WORDS,
        );
    }
    println!(
        "{} of {} KiB: {} of {} rounds {}: {}; the caches wrote {} words out before it and {} during",
        name,
        RANGE_BYTES / 1024,
        good_rounds,
        DROP_ROUNDS,
        match promise {
            Promise::NothingDirty => "left nothing dirty",
            Promise::ReadsFromDram => "left nothing dirty and read DRAM right away",
        },
        if good_rounds == DROP_ROUNDS { "OK" } else { "MISMATCH" },
        written_before,
        written_during,
    );
}

fn report_l2(l2c: L2c) {
    println!(
        "L2: CTRL {:#x} AUX {:#010x} TAG {:#x} DATA {:#x} PREFETCH {:#010x} POWER {:#x}",
        l2c.ctrl(),
        l2c.aux_ctrl(),
        l2c.tag_latency(),
        l2c.data_latency(),
        l2c.prefetch_ctrl(),
        l2c.power_ctrl(),
    );
}

fn main(_args: BootArgs) -> ! {
    println!("\nL2 test");

    // SAFETY: the chip ID block is always readable, and reading it has no side effects.
    let chip_id = unsafe { core::ptr::read_volatile(soc::CHIPID_BASE as *const u32) };
    println!("Chip ID: {:#010x}", chip_id);

    let mct = Mct::new();
    mct.start();
    let uart = console::uart();
    let l2c = L2c::new();

    mmu::enable();
    report_l2(l2c);

    let rate = measure_mct_against_uart(mct, uart);
    println!("MCT rate (measured against UART): {} Hz, expected {}", rate, soc::MCT_CLOCK_HZ);

    report_speed(mct, "L2 off");

    // Dirty in L1 and DRAM at the moment of the switch; must read back unchanged after it.
    fill(WORDS_L2, 0xA5A5_5A5A);

    println!("L2 enabling...");
    uart.flush();
    l2c.enable();
    println!("L2 enabled");
    report_l2(l2c);

    verify(WORDS_L2, 0xA5A5_5A5A);
    println!("Data written before L2 enable is correct");

    report_speed(mct, "L2 on ");

    println!("L2 revision {:#04x} (0x08 is r3p2)", l2c.revision());
    let scu = Scu::new();
    println!("SCU diagnostic {:#010x}: 764369 workaround {}", scu.diagnostic(), scu.erratum_764369_workaround());
    drop_test(mct, "Discard", mmu::discard_dcache_range, Promise::NothingDirty);
    drop_test(mct, "Invalidate", mmu::invalidate_dcache_range, Promise::ReadsFromDram);

    let gic = Gic::new();
    gic.init();
    gic.enable(soc::MCT_G0_IRQ);
    mct.start_periodic(soc::MCT_CLOCK_HZ);
    cpu::enable_irq();

    let mut printed = 0;
    let mut seed = 0u32;
    loop {
        seed = seed.wrapping_add(1);
        fill(WORDS_BEYOND, seed);
        verify(WORDS_BEYOND, seed);

        let ticks = TICKS.load(Ordering::Relaxed);
        if ticks != printed {
            printed = ticks;
            println!("tick {}  2 MiB verify passes: {}, no errors", ticks, seed);
        }
        match uart.try_read_byte() {
            Some(b'\r') => println!(),
            Some(byte) => print!("{}", byte as char),
            None => {}
        }
    }
}
