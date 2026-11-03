//! Async eMMC test: DMA transfers awaited on the controller's interrupt, with Embassy.
//!
//! A task on CPU0 ticks every 1 ms throughout and counts how late it wakes. Then:
//!
//! 1. 16 MiB from the start of USERDATA, read async, then polled: same hash, and the ticker
//!    runs during the first only.
//! 2. The same 16 MiB read async from a task on CPU1, which the interrupt on CPU0 wakes.
//! 3. A read dropped by a timeout part way, then a whole one of the same data.
//! 4. The last 2 MiB of CACHE (p8): kept, written with a pattern async, read back, and written
//!    back, as `mmc_write_test` does polled. Every write goes through an `MmcWriter` for p8.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mmc::{self, BLOCK_SIZE, Mmc, MmcWriter};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::partition::{self, BlockRead, Partition};
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println, smp};

exynos_4412_rs::entry!(main);

const BUFFER_BYTES: usize = 2 * 1024 * 1024;
const BUFFER_BLOCKS: u32 = (BUFFER_BYTES / BLOCK_SIZE) as u32;
/// Read in phases 1 and 2: 8 buffers.
const READ_BYTES: usize = 16 * 1024 * 1024;
const MIB: usize = 1024 * 1024;
/// Hashing goes in pieces of this, with a yield between, so that the ticker runs.
const HASH_PIECE: usize = 64 * 1024;
const USERDATA: u32 = 10;
const CACHE: u32 = 8;
const RCA_GUESSES: [u16; 2] = [1, 2];
const FOLD_START: u32 = 0x811C_9DC5;

#[repr(C, align(32))]
struct Buffer(UnsafeCell<[u8; BUFFER_BYTES]>);

// SAFETY: each buffer has one user: CPU0's tasks take `MAIN`, `BACKUP` and `PATTERN` one at a
// time, in order, and only CPU1's task takes `SECOND`.
unsafe impl Sync for Buffer {}

static MAIN: Buffer = Buffer(UnsafeCell::new([0; BUFFER_BYTES]));
static SECOND: Buffer = Buffer(UnsafeCell::new([0; BUFFER_BYTES]));
static BACKUP: Buffer = Buffer(UnsafeCell::new([0; BUFFER_BYTES]));
static PATTERN: Buffer = Buffer(UnsafeCell::new([0; BUFFER_BYTES]));

static TICKS: AtomicU32 = AtomicU32::new(0);
static MAX_LATE_US: AtomicU32 = AtomicU32::new(0);

/// A read for CPU1: the card, the first block, and where to send the hash.
#[derive(Clone, Copy)]
struct Request {
    card: Mmc,
    lba: u32,
}

/// CPU1's answer: hash, time, or the error.
type Answer = Result<(u32, u64), mmc::Error>;

static REQUESTS: Channel<CriticalSectionRawMutex, Request, 1> = Channel::new();
static ANSWERS: Channel<CriticalSectionRawMutex, Answer, 1> = Channel::new();

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

struct Device(Mmc);

impl BlockRead for Device {
    type Error = mmc::Error;

    fn read_block(&self, lba: u32, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), mmc::Error> {
        self.0.read_blocks(lba, buffer)
    }
}

fn fold(hash: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(hash, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

/// Hashes `bytes` on from `hash`, yielding between pieces.
async fn fold_yielding(mut hash: u32, bytes: &[u8]) -> u32 {
    for piece in bytes.chunks(HASH_PIECE) {
        hash = fold(hash, piece);
        yield_now().await;
    }
    hash
}

fn find_rca(card: Mmc) -> Option<u16> {
    RCA_GUESSES.into_iter().find(|rca| {
        card.read_status(*rca)
            .is_ok_and(|status| mmc::card_state(status) == 4)
    })
}

/// Ticks every 1 ms and records how late it woke at most since the last [`ticker_window`].
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

/// Starts a ticker window: the ticks so far, with the lateness reset. Waits for one tick
/// first, so that a tick held back by what this task did before, printing above all, which
/// is polled, counts in no window.
async fn ticker_window() -> u32 {
    let before = TICKS.load(Ordering::Relaxed);
    while TICKS.load(Ordering::Relaxed) == before {
        yield_now().await;
    }
    MAX_LATE_US.store(0, Ordering::Relaxed);
    TICKS.load(Ordering::Relaxed)
}

/// The ticks since `start`, and the latest wake-up in between, in µs.
fn ticker_report(start: u32) -> (u32, u32) {
    (
        TICKS.load(Ordering::Relaxed) - start,
        MAX_LATE_US.load(Ordering::Relaxed),
    )
}

/// How a read is done in phase 1.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Async,
    Polled,
}

/// Reads [`READ_BYTES`] from `lba` into `buffer`, one buffer at a time. Returns the hash, the
/// hash of the first MiB, and the time spent reading, in µs.
async fn read_region(
    card: Mmc,
    lba: u32,
    buffer: &mut [u8; BUFFER_BYTES],
    way: Way,
) -> Result<(u32, u32, u64), mmc::Error> {
    let (mut hash, mut first_mib, mut reading) = (FOLD_START, 0, 0);
    for index in 0..(READ_BYTES / BUFFER_BYTES) as u32 {
        let begin = Instant::now();
        let at = lba + index * BUFFER_BLOCKS;
        match way {
            Way::Async => card.read_blocks_async(at, buffer).await?,
            Way::Polled => card.read_blocks_dma(at, buffer)?,
        }
        reading += begin.elapsed().as_micros();
        if index == 0 {
            first_mib = fold_yielding(FOLD_START, &buffer[..MIB]).await;
        }
        hash = fold_yielding(hash, buffer).await;
    }
    Ok((hash, first_mib, reading))
}

fn mb_per_s(bytes: usize, us: u64) -> u64 {
    bytes as u64 / us.max(1)
}

#[embassy_executor::task]
async fn reader_on_cpu1() {
    // SAFETY: only this task takes `SECOND`, once.
    let buffer = unsafe { &mut *SECOND.0.get() };
    loop {
        let request = REQUESTS.receive().await;
        let answer = read_region(request.card, request.lba, buffer, Way::Async)
            .await
            .map(|(hash, _, us)| (hash, us));
        ANSWERS.send(answer).await;
    }
}

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

#[embassy_executor::task]
async fn supervisor(card: Mmc, rca: u16) {
    // SAFETY: this task is the only user of these three buffers.
    let (main, backup, pattern) = unsafe {
        (
            &mut *MAIN.0.get(),
            &mut *BACKUP.0.get(),
            &mut *PATTERN.0.get(),
        )
    };
    let mut table = [Partition::default(); 16];
    let found = match partition::scan(&Device(card), &mut table) {
        Ok(found) => found,
        Err(error) => {
            println!("Partition table ERROR {:x?}", error);
            return;
        }
    };
    let Some(userdata) = table[..found].iter().find(|p| p.number == USERDATA) else {
        println!("No USERDATA partition");
        return;
    };
    let lba = userdata.start;

    // 1. Async, then polled.
    let mut results = [(0, 0, 0); 2];
    for (way, name, slot) in [(Way::Async, "async", 0), (Way::Polled, "polled", 1)] {
        let window = ticker_window().await;
        let begin = Instant::now();
        let interrupts = mmc::interrupts();
        match read_region(card, lba, main, way).await {
            Ok(result) => {
                let total = begin.elapsed().as_micros();
                let (ticks, late) = ticker_report(window);
                println!(
                    "USERDATA 16 MiB {}: {} ms reading, {} MB/s, hash {:#010x}; {} interrupts; ticker {} ticks in {} ms, latest {} us late",
                    name,
                    result.2 / 1000,
                    mb_per_s(READ_BYTES, result.2),
                    result.0,
                    mmc::interrupts() - interrupts,
                    ticks,
                    total / 1000,
                    late
                );
                results[slot] = result;
            }
            Err(error) => {
                println!("USERDATA {} read ERROR {:x?}", name, error);
                return;
            }
        }
    }
    println!(
        "Async and polled hashes: {}",
        verdict(results[0].0 == results[1].0)
    );
    let (hash, first_mib) = (results[0].0, results[0].1);

    // 2. From CPU1.
    let window = ticker_window().await;
    REQUESTS.send(Request { card, lba }).await;
    match ANSWERS.receive().await {
        Ok((cpu1_hash, us)) => {
            let (ticks, late) = ticker_report(window);
            println!(
                "CPU1 async: {} ms reading, {} MB/s, hash {:#010x} ({}); CPU0 ticker {} ticks, latest {} us late",
                us / 1000,
                mb_per_s(READ_BYTES, us),
                cpu1_hash,
                verdict(cpu1_hash == hash),
                ticks,
                late
            );
        }
        Err(error) => println!("CPU1 read ERROR {:x?}", error),
    }

    // 3. A read dropped part way, then a whole one.
    let cut = with_timeout(
        Duration::from_micros(300),
        card.read_blocks_async(lba, &mut main[..MIB]),
    )
    .await;
    let again = card.read_blocks_async(lba, &mut main[..MIB]).await;
    let again_hash = fold(FOLD_START, &main[..MIB]);
    println!(
        "Read of 1 MiB cut after 300 us: {}; the next one: {:x?}, hash {:#010x} ({})",
        if cut.is_err() {
            "timed out"
        } else {
            "finished first"
        },
        again,
        again_hash,
        verdict(again.is_ok() && again_hash == first_mib)
    );

    // 4. The end of CACHE.
    let writer = match MmcWriter::new(card, rca, CACHE) {
        Ok(writer) => writer,
        Err(error) => {
            println!("Writer ERROR {:x?}", error);
            return;
        }
    };
    let cache = writer.partition();
    let relative = cache.count - BUFFER_BLOCKS;
    let absolute = cache.start + relative;
    if let Err(error) = card.read_blocks_async(absolute, backup).await {
        println!("Backup read ERROR {:x?}: nothing written", error);
        return;
    }
    let kept = fold(FOLD_START, backup);
    println!(
        "CACHE blocks {}..{} kept, hash {:#010x}",
        absolute,
        absolute + BUFFER_BLOCKS,
        kept
    );
    for (index, byte) in pattern.iter_mut().enumerate() {
        *byte = (index as u32).wrapping_mul(0x9E37_79B1).to_le_bytes()[3] ^ (index >> 9) as u8;
    }
    let window = ticker_window().await;
    let begin = Instant::now();
    let wrote = writer.write_async(relative, pattern).await;
    let write_us = begin.elapsed().as_micros();
    let (ticks, late) = ticker_report(window);
    let read_back = card.read_blocks_async(absolute, main).await;
    let same = read_back.is_ok() && main[..] == pattern[..];
    println!(
        "Pattern written async: {:x?}, {} ms, {} MB/s; ticker {} ticks, latest {} us late; read back {:x?} ({})",
        wrote,
        write_us / 1000,
        mb_per_s(BUFFER_BYTES, write_us),
        ticks,
        late,
        read_back,
        verdict(wrote.is_ok() && same)
    );
    let restored = writer.write_async(relative, backup).await;
    let check = card.read_blocks_async(absolute, main).await;
    let restored_hash = fold(FOLD_START, main);
    println!(
        "Restored: {:x?}, read {:x?}, hash {:#010x} ({})",
        restored,
        check,
        restored_hash,
        verdict(restored.is_ok() && check.is_ok() && restored_hash == kept)
    );
    println!("{} controller interrupts in all", mmc::interrupts());
    println!("Done. UART: r restart, f restart into fastboot. Keys: as in power_test.");
}

#[embassy_executor::task]
async fn console_task() {
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

fn secondary(_core: usize) -> ! {
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    embassy::run(|spawner| spawner.spawn(reader_on_cpu1().unwrap()))
}

fn main(_args: BootArgs) -> ! {
    println!("\nAsync eMMC test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    mmc::init_interrupt();
    cpu::enable_irq();

    let card = match Mmc::new() {
        Ok(card) => card,
        Err(error) => {
            println!("Controller ERROR {:x?}", error);
            cpu::halt();
        }
    };
    match card.read_ext_csd() {
        Ok(ext_csd) if ext_csd.reads_user_area() => {}
        other => {
            println!("EXT_CSD {:x?}: not the user area: stopping", other);
            cpu::halt();
        }
    }
    let Some(rca) = find_rca(card) else {
        println!("No card in transfer state at {:?}: stopping", RCA_GUESSES);
        cpu::halt();
    };
    if let Err(error) = smp::start_cpu(1, secondary) {
        println!("CPU1 ERROR {:?}", error);
    }
    embassy::run(|spawner| {
        spawner.spawn(ticker().unwrap());
        spawner.spawn(console_task().unwrap());
        spawner.spawn(supervisor(card, rca).unwrap());
    })
}
