//! eMMC read test. Reads only: the driver has no command that can change the card.
//!
//! Reports what U-Boot left, reads `EXT_CSD`, and parses the partition table, comparing it with
//! the table known for this board. Then checks a few partitions' first blocks, and that
//! repeated reads agree and how fast they are. Prints no partition contents, except the first
//! bytes of the boot partition and magic numbers.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mmc::{self, BLOCK_SIZE, Mmc};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::partition::{self, BlockRead, Partition};
use exynos_4412_rs::{BootArgs, console, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

/// The table on this board: number, type, start, count, name.
///
/// The listing first given for this board had 28229568 blocks for p10. That is the count of the
/// extended partition minus the start of p10, as if the extended partition started at block 0.
/// The value here is the one on the card, which ends p10 exactly where p4 ends.
const EXPECTED: [(u32, u8, u32, u32, &str); 10] = [
    (1, 0x83, 20480, 2048, "PARAM"),
    (2, 0x83, 22528, 2048, "MISC"),
    (3, 0x83, 24576, 40960, "EFS"),
    (4, 0x05, 65536, 30691328, "EXTENDED"),
    (5, 0x83, 65600, 20416, "BOOT"),
    (6, 0x83, 86080, 20416, "RECOVERY"),
    (7, 0x83, 106560, 10176, "LOGO"),
    (8, 0x83, 116800, 911296, "CACHE"),
    (9, 0x83, 1028160, 1433536, "SYSTEM"),
    (10, 0x83, 2461760, 28295104, "USERDATA"),
];
/// `bootcmd` reads the image from here (`mmc read 0 40008000 10040 4FC0`): the BOOT partition.
const BOOTCMD_LBA: u32 = 0x10040;
/// `mkbootimg`'s header starts with this.
const ANDROID_MAGIC: &[u8] = b"ANDROID!";
/// Partitions likely to hold a file system.
const FILESYSTEM_PARTITIONS: [usize; 4] = [3, 8, 9, 10];
/// The ext superblock is at byte 1024, so in the third block, with its magic at offset 56.
const EXT_SUPERBLOCK_LBA: u32 = 2;
const EXT_MAGIC_OFFSET: usize = 56;
const EXT_MAGIC: u16 = 0xEF53;
/// Guesses at the address U-Boot gave the card, which it does not report.
const RCA_GUESSES: [u16; 2] = [1, 2];
/// Blocks for comparing single and multiple block reads.
const COMPARE_BLOCKS: usize = 8;
const BLOCKS_PER_READ: usize = 2048;
const READS: u32 = 16;
/// Where the speed test reads: the start of SYSTEM.
const SPEED_LBA: u32 = 1028160;
const CHUNK_BYTES: usize = BLOCKS_PER_READ * BLOCK_SIZE;

/// Read buffer, too large for the stack.
#[repr(align(32))]
struct Chunk(UnsafeCell<[u8; CHUNK_BYTES]>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Chunk {}

static CHUNK: Chunk = Chunk(UnsafeCell::new([0; CHUNK_BYTES]));

struct Device(Mmc);

impl BlockRead for Device {
    type Error = mmc::Error;

    fn read_block(&self, lba: u32, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), mmc::Error> {
        self.0.read_blocks(lba, buffer)
    }
}

/// Stays where the board can still be restarted from the UART or the keys. A failed read may
/// have left the card unusable until then.
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

fn report_state() {
    println!("--- State left by U-Boot ---");
    let clocks = Cmu::new().mmc4_clocks();
    println!(
        "GATE_IP_FSYS {:#010x} SRC_FSYS {:#010x} SRC_MASK_FSYS {:#010x} DIV_FSYS3 {:#010x}",
        clocks.gate, clocks.src, clocks.src_mask, clocks.div,
    );
    match mmc::controller_state() {
        Some(state) => {
            println!("VERID {:#010x} HCON {:#010x}", state.verid, state.hcon);
            println!("CTRL {:#010x} BMOD {:#010x} STATUS {:#010x}", state.ctrl, state.bmod, state.status);
            println!(
                "CLKENA {:#x} CLKDIV {:#x} CLKSEL {:#010x} CTYPE {:#x} FIFOTH {:#010x}",
                state.clkena, state.clkdiv, state.clksel, state.ctype, state.fifoth,
            );
        }
        None => println!("Controller not readable: clock gate closed"),
    }
}

fn report_status(card: Mmc) {
    for rca in RCA_GUESSES {
        match card.read_status(rca) {
            Ok(status) => {
                println!(
                    "CMD13 to address {}: status {:#010x}, state {} ({})",
                    rca,
                    status,
                    mmc::card_state(status),
                    if mmc::card_state(status) == 4 { "transfer" } else { "not transfer" },
                );
                return;
            }
            Err(error) => println!("CMD13 to address {}: {:?}", rca, error),
        }
    }
}

fn check_table(partitions: &[Partition]) {
    println!("Number  Type  Start     Count     Size MiB  Name");
    for partition in partitions {
        let expected = EXPECTED.iter().find(|entry| entry.0 == partition.number);
        println!(
            "{:<6}  {:#04x}  {:<8}  {:<8}  {:<8}  {}",
            partition.number,
            partition.kind,
            partition.start,
            partition.count,
            partition.count / 2048,
            expected.map_or("?", |entry| entry.4),
        );
    }
    let same = partitions.len() == EXPECTED.len()
        && EXPECTED.iter().zip(partitions).all(|(expected, partition)| {
            *partition
                == Partition { number: expected.0, kind: expected.1, start: expected.2, count: expected.3 }
        });
    println!("Same as expected table: {}", if same { "OK" } else { "MISMATCH" });
}

fn check_boot(card: Mmc) {
    let mut block = [0; BLOCK_SIZE];
    match card.read_blocks(BOOTCMD_LBA, &mut block) {
        Ok(()) => println!(
            "Block {:#x} starts with {:?}, the image magic ({})",
            BOOTCMD_LBA,
            core::str::from_utf8(&block[..ANDROID_MAGIC.len()]).unwrap_or("not text"),
            if block.starts_with(ANDROID_MAGIC) { "OK" } else { "MISMATCH" },
        ),
        Err(error) => println!("Block {:#x} ERROR {:?}", BOOTCMD_LBA, error),
    }
}

fn check_filesystems(card: Mmc, partitions: &[Partition]) {
    let mut block = [0; BLOCK_SIZE];
    for index in FILESYSTEM_PARTITIONS {
        let Some(partition) = partitions.iter().find(|partition| partition.number as usize == index) else {
            continue;
        };
        match card.read_blocks(partition.start + EXT_SUPERBLOCK_LBA, &mut block) {
            Ok(()) => {
                let magic = u16::from_le_bytes([block[EXT_MAGIC_OFFSET], block[EXT_MAGIC_OFFSET + 1]]);
                println!(
                    "p{}: ext superblock magic {:#06x} ({})",
                    index,
                    magic,
                    if magic == EXT_MAGIC { "ext" } else { "not ext" },
                );
            }
            Err(error) => println!("p{} ERROR {:?}", index, error),
        }
    }
}

/// The same blocks, one at a time and all at once, must read the same.
fn check_single_against_multiple(card: Mmc, buffer: &mut [u8]) {
    let (one_by_one, together) = buffer.split_at_mut(COMPARE_BLOCKS * BLOCK_SIZE);
    let together = &mut together[..COMPARE_BLOCKS * BLOCK_SIZE];
    for index in 0..COMPARE_BLOCKS {
        let block = &mut one_by_one[index * BLOCK_SIZE..][..BLOCK_SIZE];
        if let Err(error) = card.read_blocks(SPEED_LBA + index as u32, block) {
            println!("Single block read ERROR {:?}", error);
            return;
        }
    }
    if let Err(error) = card.read_blocks(SPEED_LBA, together) {
        println!("Multiple block read ERROR {:?}", error);
        return;
    }
    println!(
        "{} blocks, one by one and together: {}",
        COMPARE_BLOCKS,
        if one_by_one == together { "OK" } else { "MISMATCH" },
    );
}

fn fold(buffer: &[u8]) -> u32 {
    buffer.iter().fold(0x811C_9DC5, |hash: u32, byte| (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193))
}

/// Reads a stretch twice, measuring speed. The two passes must agree.
fn check_speed(mct: Mct, card: Mmc, buffer: &mut [u8]) {
    let mut hashes = [0u32; 2];
    for (pass, hash) in hashes.iter_mut().enumerate() {
        let mut time = 0;
        for round in 0..READS {
            let start = mct.count();
            let lba = SPEED_LBA + round * BLOCKS_PER_READ as u32;
            if let Err(error) = card.read_blocks(lba, buffer) {
                println!("Pass {} read at block {} ERROR {:?}", pass + 1, lba, error);
                return;
            }
            time += mct.count() - start;
            *hash = hash.wrapping_mul(31).wrapping_add(fold(buffer));
        }
        let bytes = u64::from(READS) * CHUNK_BYTES as u64;
        let us = time / (u64::from(soc::MCT_CLOCK_HZ) / 1_000_000);
        println!(
            "Pass {}: {} MiB in {} us, {} KiB/s, hash {:#010x}",
            pass + 1,
            bytes / (1024 * 1024),
            us,
            bytes * 1_000_000 / 1024 / us.max(1),
            *hash,
        );
    }
    println!("Both passes: {}", if hashes[0] == hashes[1] { "OK" } else { "MISMATCH" });
}

fn main(_args: BootArgs) -> ! {
    println!("\neMMC read test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    report_state();

    let card = match Mmc::new() {
        Ok(card) => card,
        Err(error) => {
            println!("Controller ERROR {:?}", error);
            idle(mct);
        }
    };
    println!("FIFO {} bits", if card.fifo_is_64_bit() { 64 } else { 32 });

    report_status(card);

    let ext_csd = match card.read_ext_csd() {
        Ok(ext_csd) => ext_csd,
        Err(error) => {
            println!("EXT_CSD ERROR {:?}", error);
            idle(mct);
        }
    };
    println!(
        "EXT_CSD: revision {}, {} blocks ({} MiB), PARTITION_CONFIG {:#04x}, BUS_WIDTH {}, HS_TIMING {}, boot {} KiB each",
        ext_csd.revision,
        ext_csd.sectors,
        ext_csd.sectors / 2048,
        ext_csd.part_config,
        ext_csd.bus_width,
        ext_csd.hs_timing,
        u32::from(ext_csd.boot_mult) * 128,
    );
    if !ext_csd.reads_user_area() {
        println!("Reads do not come from the user area (MISMATCH): stopping here");
        idle(mct);
    }

    let mut partitions = [Partition::default(); 16];
    let found = match partition::scan(&Device(card), &mut partitions) {
        Ok(found) => found,
        Err(error) => {
            println!("Partition table ERROR {:?}", error);
            let mut block = [0; BLOCK_SIZE];
            if card.read_blocks(0, &mut block).is_ok() {
                print!("Block 0 ends with:");
                for byte in &block[BLOCK_SIZE - 16..] {
                    print!(" {:02x}", byte);
                }
                println!();
            }
            idle(mct);
        }
    };
    let partitions = &partitions[..found];
    check_table(partitions);

    check_boot(card);
    check_filesystems(card, partitions);

    // SAFETY: the only reference to this buffer, taken once: see `Chunk`.
    let buffer: &mut [u8] = unsafe { &mut *CHUNK.0.get() };
    check_single_against_multiple(card, buffer);
    check_speed(mct, card, buffer);

    idle(mct);
}
