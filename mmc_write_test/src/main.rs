//! eMMC write test. Writes only the last 2 MiB of CACHE (p8), after keeping a copy of them,
//! and writes the copy back at the end, also after a failure.
//!
//! First, without writing: shows which blocks before the first partition are not zero. Then
//! copies the region, checks that [`MmcWriter`] refuses what it must before anything reaches
//! the card, writes 1, 9 and 4096 blocks of a pattern, reads each back through DMA and polled,
//! and restores the copy.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mmc::{
    self, BLOCK_SIZE, Mmc, MmcWriter, PROTECTED_BLOCKS, WriteError,
};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::partition::{self, BlockRead, Partition};
use exynos_4412_rs::{BootArgs, console, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

/// The partition written: CACHE.
const TARGET: u32 = 8;
const REGION_BLOCKS: usize = 4096;
const REGION_BYTES: usize = REGION_BLOCKS * BLOCK_SIZE;
/// Write cases: blocks, and their offset into the region, in blocks. The 9 blocks also start
/// one cache line into the pattern buffer, off a 4 KiB boundary; 4096 blocks take two commands.
const CASES: [(usize, usize, usize); 3] = [(1, 0, 0), (9, 17, 1), (REGION_BLOCKS, 0, 0)];
/// Ranges of non-zero blocks printed at most, before the first partition.
const MAX_RANGES: usize = 32;
/// The survey reads this much at a time.
const SURVEY_BLOCKS: u32 = 2048;
/// Guesses at the address U-Boot gave the card, which it does not report.
const RCA_GUESSES: [u16; 2] = [1, 2];

#[repr(C, align(32))]
struct Buffer(UnsafeCell<[u8; REGION_BYTES + mmu::DCACHE_LINE]>);

// SAFETY: `main` is the only user, and takes the one reference to each once.
unsafe impl Sync for Buffer {}

static BACKUP: Buffer = Buffer(UnsafeCell::new([0; REGION_BYTES + mmu::DCACHE_LINE]));
static PATTERN: Buffer = Buffer(UnsafeCell::new([0; REGION_BYTES + mmu::DCACHE_LINE]));
static READBACK: Buffer = Buffer(UnsafeCell::new([0; REGION_BYTES + mmu::DCACHE_LINE]));

struct Device(Mmc);

impl BlockRead for Device {
    type Error = mmc::Error;

    fn read_block(&self, lba: u32, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), mmc::Error> {
        self.0.read_blocks(lba, buffer)
    }
}

/// Stays where the board can still be restarted from the UART or the keys.
fn idle(mct: Mct) -> ! {
    println!("Done. UART: r restart, f restart into fastboot. Keys: as in power_test.");
    let uart = console::uart();
    let mut keys = PowerKeys::new();
    loop {
        keys.service(mct);
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
    }
}

fn us(mct_counts: u64) -> u64 {
    mct_counts / u64::from(soc::MCT_CLOCK_HZ / 1_000_000)
}

fn fold(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811C_9DC5, |hash: u32, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

fn find_rca(card: Mmc) -> Option<u16> {
    RCA_GUESSES.into_iter().find(|rca| {
        card.read_status(*rca)
            .is_ok_and(|status| mmc::card_state(status) == 4)
    })
}

/// Reads everything before [`PROTECTED_BLOCKS`] and prints the ranges of blocks that are not
/// all zero. Nothing of their contents.
fn survey(card: Mmc, buffer: &mut [u8]) {
    let chunk = &mut buffer[..SURVEY_BLOCKS as usize * BLOCK_SIZE];
    let mut ranges = 0;
    let mut open: Option<u32> = None;
    print!("Blocks 0..{} not all zero:", PROTECTED_BLOCKS);
    for first in (0..PROTECTED_BLOCKS).step_by(SURVEY_BLOCKS as usize) {
        if let Err(error) = card.read_blocks_dma(first, chunk) {
            println!(" read at {} ERROR {:x?}", first, error);
            return;
        }
        for (index, block) in chunk.chunks(BLOCK_SIZE).enumerate() {
            let lba = first + index as u32;
            let used = block.iter().any(|byte| *byte != 0);
            match (used, open) {
                (true, None) => open = Some(lba),
                (false, Some(start)) => {
                    if ranges < MAX_RANGES {
                        print!(" {}..{}", start, lba);
                    }
                    ranges += 1;
                    open = None;
                }
                _ => {}
            }
        }
    }
    if let Some(start) = open {
        print!(" {}..{}", start, PROTECTED_BLOCKS);
        ranges += 1;
    }
    println!(
        " ({} ranges{})",
        ranges,
        if ranges > MAX_RANGES {
            ", first ones shown"
        } else {
            ""
        }
    );
}

fn expect_refused(name: &str, result: Result<(), WriteError>, expected: WriteError) {
    let good = result == Err(expected);
    println!(
        "Refuses {}: {:x?} ({})",
        name,
        result,
        if good { "OK" } else { "MISMATCH" }
    );
}

/// Writers that must not be made, and writes that must not start. None reaches the card's data.
fn check_refusals(card: Mmc, rca: u16, writer: &MmcWriter, pattern: &[u8]) {
    let count = writer.partition().count;
    expect_refused(
        "the extended partition",
        MmcWriter::new(card, rca, 4).map(|_| ()),
        WriteError::Extended,
    );
    expect_refused(
        "a partition not in the table",
        MmcWriter::new(card, rca, 11).map(|_| ()),
        WriteError::NoPartition(11),
    );
    expect_refused(
        "a block past the end",
        writer.write(count, &pattern[..BLOCK_SIZE]),
        WriteError::OutOfRange,
    );
    expect_refused(
        "two blocks across the end",
        writer.write(count - 1, &pattern[..2 * BLOCK_SIZE]),
        WriteError::OutOfRange,
    );
    expect_refused(
        "a buffer off a cache line",
        writer.write(count - 1, &pattern[4..4 + BLOCK_SIZE]),
        WriteError::Card(mmc::Error::BadBuffer),
    );
    expect_refused(
        "part of a block",
        writer.write(count - 1, &pattern[..100]),
        WriteError::Card(mmc::Error::BadLength),
    );
}

fn fill_pattern(pattern: &mut [u8], salt: u32) {
    for (index, word) in pattern.chunks_mut(4).enumerate() {
        let value = salt ^ (index as u32).wrapping_mul(0x9E37_79B1);
        word.copy_from_slice(&value.to_le_bytes()[..word.len()]);
    }
}

/// Reads `blocks` from `lba` both ways into `readback` and compares with `expected`.
fn verify(card: Mmc, lba: u32, expected: &[u8], readback: &mut [u8]) -> (bool, bool) {
    let readback = &mut readback[..expected.len()];
    readback.fill(0);
    let dma = card.read_blocks_dma(lba, readback).is_ok() && readback == expected;
    readback.fill(0);
    let polled = card.read_blocks(lba, readback).is_ok() && readback == expected;
    (dma, polled)
}

/// The cases, each written, timed and read back. Returns whether all were right.
fn write_cases(
    mct: Mct,
    card: Mmc,
    writer: &MmcWriter,
    region: u32,
    pattern: &[u8],
    readback: &mut [u8],
) -> bool {
    let mut all = true;
    for (blocks, offset, lines) in CASES {
        let data = &pattern[lines * mmu::DCACHE_LINE..][..blocks * BLOCK_SIZE];
        let begin = mct.count();
        let result = writer.write(region + offset as u32, data);
        let time = us(mct.count() - begin);
        if let Err(error) = result {
            println!("Write {} blocks at +{}: ERROR {:x?}", blocks, offset, error);
            return false;
        }
        let lba = writer.partition().start + region + offset as u32;
        let (dma, polled) = verify(card, lba, data, readback);
        println!(
            "Write {} blocks at +{} in {} us ({} KiB/s): read back through DMA {}, polled {}: {}",
            blocks,
            offset,
            time,
            data.len() as u64 * 1_000_000 / 1024 / time.max(1),
            dma,
            polled,
            if dma && polled { "OK" } else { "MISMATCH" },
        );
        all &= dma && polled;
    }
    all
}

fn main(_args: BootArgs) -> ! {
    println!("\neMMC write test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let card = match Mmc::new() {
        Ok(card) => card,
        Err(error) => {
            println!("Controller ERROR {:x?}", error);
            idle(mct);
        }
    };
    match card.read_ext_csd() {
        Ok(ext_csd) if ext_csd.reads_user_area() => {
            println!(
                "EXT_CSD: PARTITION_CONFIG {:#04x}, user area selected",
                ext_csd.part_config
            )
        }
        Ok(ext_csd) => {
            println!(
                "PARTITION_CONFIG {:#04x}: not the user area (MISMATCH): stopping",
                ext_csd.part_config
            );
            idle(mct);
        }
        Err(error) => {
            println!("EXT_CSD ERROR {:x?}", error);
            idle(mct);
        }
    }
    let Some(rca) = find_rca(card) else {
        println!(
            "No card in transfer state at addresses {:?}: stopping",
            RCA_GUESSES
        );
        idle(mct);
    };
    let mut table = [Partition::default(); 16];
    let found = match partition::scan(&Device(card), &mut table) {
        Ok(found) => found,
        Err(error) => {
            println!("Partition table ERROR {:x?}", error);
            idle(mct);
        }
    };
    for partition in &table[..found] {
        println!(
            "p{}: type {:#04x}, blocks {}..{}, table record at {}",
            partition.number,
            partition.kind,
            partition.start,
            partition.start + partition.count,
            partition.record,
        );
    }

    // SAFETY: the only references to these buffers, taken once: see `Buffer`.
    let (backup, pattern, readback): (&mut [u8], &mut [u8], &mut [u8]) = unsafe {
        (
            &mut *BACKUP.0.get(),
            &mut *PATTERN.0.get(),
            &mut *READBACK.0.get(),
        )
    };
    survey(card, readback);

    let writer = match MmcWriter::new(card, rca, TARGET) {
        Ok(writer) => writer,
        Err(error) => {
            println!("Writer for p{} ERROR {:x?}: stopping", TARGET, error);
            idle(mct);
        }
    };
    let target = writer.partition();
    let region = target.count - REGION_BLOCKS as u32;
    println!(
        "Writer for p{}: blocks {}..{}; region {}..{}",
        TARGET,
        target.start,
        target.start + target.count,
        target.start + region,
        target.start + target.count
    );
    // The copy, read twice to be sure of it before anything is written.
    let backup = &mut backup[..REGION_BYTES];
    let lba = target.start + region;
    if let Err(error) = card.read_blocks_dma(lba, backup) {
        println!("Backup read ERROR {:x?}: stopping, nothing written", error);
        idle(mct);
    }
    let (dma, polled) = verify(card, lba, backup, readback);
    let kept = fold(backup);
    println!(
        "Backup of the region: hash {:#010x}, read again through DMA {}, polled {}",
        kept, dma, polled
    );
    if !(dma && polled) {
        println!("Backup not stable (MISMATCH): stopping, nothing written");
        idle(mct);
    }
    // After the copy: two refusals aim at the region's last block, which the restore covers.
    check_refusals(card, rca, &writer, pattern);

    fill_pattern(pattern, (mct.count() as u32) | 1);
    let written = write_cases(mct, card, &writer, region, pattern, readback);

    // Always put the copy back, even after a failure.
    let begin = mct.count();
    let restored = writer.write(region, backup);
    let time = us(mct.count() - begin);
    match restored {
        Ok(()) => {
            let (dma, polled) = verify(card, lba, backup, readback);
            println!(
                "Restore in {} us: read back through DMA {}, polled {}, hash {:#010x}: {}",
                time,
                dma,
                polled,
                fold(&readback[..REGION_BYTES]),
                if dma && polled { "OK" } else { "MISMATCH" },
            );
        }
        Err(error) => println!(
            "Restore ERROR {:x?}: the region at {}..{} has the test pattern",
            error,
            lba,
            lba + REGION_BLOCKS as u32
        ),
    }
    println!("Writes: {}", if written { "OK" } else { "MISMATCH" });

    idle(mct);
}
