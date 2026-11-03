//! FAT32 test on CACHE (p8), through `exynos_4412_rs::fat` and `embedded_sdmmc`.
//!
//! If CACHE holds no FAT32 file system, formats it: this erases what it held. Then counts
//! the boots in `BOOTS.TXT`, writes a 1 MiB file and reads it back, timed and hashed, deletes
//! it, appends a line to `LOGS/BOOT.LOG` and lists the root directory.
//!
//! Every write goes through an `MmcWriter` for CACHE, which refuses blocks outside it.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::ops::ControlFlow;

use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mmc::{self, Mmc};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::fat::embedded_sdmmc::{Directory, Mode, VolumeIdx, VolumeManager};
use exynos_4412_rs::fat::{self, FixedTime, PartitionDevice};
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

/// The partition used: CACHE.
const TARGET: u32 = 8;
const LABEL: &[u8; 11] = b"CACHE      ";
const BOUNCE_BYTES: usize = 32 * 1024;
const CHUNK_BYTES: usize = 32 * 1024;
const BIG_BYTES: u32 = 1024 * 1024;
/// Guesses at the address U-Boot gave the card, which it does not report.
const RCA_GUESSES: [u16; 2] = [1, 2];

type FsError = fat::embedded_sdmmc::Error<fat::Error>;
type Root<'a> = Directory<'a, PartitionDevice<'static>, FixedTime, 4, 4, 1>;

#[repr(C, align(32))]
struct Buffer<const N: usize>(UnsafeCell<[u8; N]>);

// SAFETY: `main` is the only user, and takes the one reference to each once.
unsafe impl<const N: usize> Sync for Buffer<N> {}

static BOUNCE: Buffer<BOUNCE_BYTES> = Buffer(UnsafeCell::new([0; BOUNCE_BYTES]));
static CHUNK: Buffer<CHUNK_BYTES> = Buffer(UnsafeCell::new([0; CHUNK_BYTES]));

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

/// KiB per second, for `bytes` in `micros`.
fn kib_per_s(bytes: u32, micros: u64) -> u64 {
    u64::from(bytes) * 1_000_000 / 1024 / micros.max(1)
}

fn fold(hash: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(hash, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

const FOLD_START: u32 = 0x811C_9DC5;

/// The test file's byte at `offset`: no run repeats within a block or across blocks.
fn pattern(offset: u32) -> u8 {
    (offset.wrapping_mul(0x9E37_79B1) >> 24) as u8 ^ (offset >> 9) as u8
}

fn find_rca(card: Mmc) -> Option<u16> {
    RCA_GUESSES.into_iter().find(|rca| {
        card.read_status(*rca)
            .is_ok_and(|status| mmc::card_state(status) == 4)
    })
}

/// Decimal digits of `value` into `buffer`, then a line end. Returns the bytes used.
fn decimal_line(value: u32, buffer: &mut [u8; 16]) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut rest = value;
    loop {
        digits[count] = b'0' + (rest % 10) as u8;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    for (index, digit) in digits[..count].iter().rev().enumerate() {
        buffer[index] = *digit;
    }
    buffer[count] = b'\r';
    buffer[count + 1] = b'\n';
    count + 2
}

/// Reads the count in `BOOTS.TXT`, 0 if there is no such file, and writes it back plus one.
fn count_boot(root: &Root<'_>) -> Result<u32, FsError> {
    let previous = match root.open_file_in_dir("BOOTS.TXT", Mode::ReadOnly) {
        Ok(file) => {
            let mut text = [0u8; 16];
            let read = file.read(&mut text)?;
            file.close()?;
            text[..read]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .fold(0u32, |value, digit| {
                    value.wrapping_mul(10).wrapping_add(u32::from(digit - b'0'))
                })
        }
        Err(FsError::NotFound) => 0,
        Err(error) => return Err(error),
    };
    let boot = previous + 1;
    let mut line = [0u8; 16];
    let length = decimal_line(boot, &mut line);
    let file = root.open_file_in_dir("BOOTS.TXT", Mode::ReadWriteCreateOrTruncate)?;
    file.write(&line[..length])?;
    file.close()?;
    Ok(boot)
}

/// Writes `TEST.BIN`, reads it back and compares, then deletes it.
fn big_file(root: &Root<'_>, mct: Mct, chunk: &mut [u8; CHUNK_BYTES]) -> Result<(), FsError> {
    let file = root.open_file_in_dir("TEST.BIN", Mode::ReadWriteCreateOrTruncate)?;
    let mut written = FOLD_START;
    let begin = mct.count();
    for start in (0..BIG_BYTES).step_by(CHUNK_BYTES) {
        for (index, byte) in chunk.iter_mut().enumerate() {
            *byte = pattern(start + index as u32);
        }
        written = fold(written, chunk);
        file.write(chunk)?;
    }
    file.close()?;
    let write_us = us(mct.count() - begin);
    println!(
        "TEST.BIN: wrote {} KiB in {} ms, {} KiB/s, hash {:#010x}",
        BIG_BYTES / 1024,
        write_us / 1000,
        kib_per_s(BIG_BYTES, write_us),
        written
    );

    let file = root.open_file_in_dir("TEST.BIN", Mode::ReadOnly)?;
    let length = file.length();
    let mut read = FOLD_START;
    let mut total = 0u32;
    let mut first_bad = None;
    let begin = mct.count();
    while !file.is_eof() {
        let count = file.read(chunk)?;
        if count == 0 {
            break;
        }
        if first_bad.is_none() {
            first_bad = chunk[..count]
                .iter()
                .enumerate()
                .find(|(index, byte)| **byte != pattern(total + *index as u32))
                .map(|(index, _)| total + index as u32);
        }
        read = fold(read, &chunk[..count]);
        total += count as u32;
    }
    let read_us = us(mct.count() - begin);
    file.close()?;
    let good = length == BIG_BYTES && total == BIG_BYTES && read == written && first_bad.is_none();
    println!(
        "TEST.BIN: length {}, read {} KiB in {} ms, {} KiB/s, hash {:#010x}, first bad byte {:?} ({})",
        length,
        total / 1024,
        read_us / 1000,
        kib_per_s(total, read_us),
        read,
        first_bad,
        if good { "OK" } else { "MISMATCH" }
    );
    root.delete_entry_in_dir("TEST.BIN")?;
    let gone = matches!(
        root.find_directory_entry("TEST.BIN"),
        Err(FsError::NotFound)
    );
    println!(
        "TEST.BIN: deleted ({})",
        if gone { "OK" } else { "MISMATCH" }
    );
    Ok(())
}

/// Appends the boot number to `LOGS/BOOT.LOG`, making the directory if needed.
fn append_log(root: &Root<'_>, boot: u32) -> Result<(), FsError> {
    match root.make_dir_in_dir("LOGS") {
        Ok(()) => println!("LOGS: made"),
        Err(FsError::DirAlreadyExists) => {}
        Err(error) => return Err(error),
    }
    let logs = root.open_dir("LOGS")?;
    let file = logs.open_file_in_dir("BOOT.LOG", Mode::ReadWriteCreateOrAppend)?;
    let mut line = [0u8; 16];
    let length = decimal_line(boot, &mut line);
    file.write(b"boot ")?;
    file.write(&line[..length])?;
    let size = file.length();
    file.close()?;
    println!(
        "LOGS/BOOT.LOG: {} bytes after the append (expected {})",
        size,
        boot_log_size(boot)
    );
    logs.close()
}

/// The size `LOGS/BOOT.LOG` has after `boots` appends: "boot <n>\r\n" each.
fn boot_log_size(boots: u32) -> u32 {
    let mut line = [0u8; 16];
    (1..=boots)
        .map(|boot| 5 + decimal_line(boot, &mut line) as u32)
        .sum()
}

fn list(root: &Root<'_>) -> Result<(), FsError> {
    println!("Root directory:");
    root.iterate_dir(|entry| {
        println!(
            "  {:12} {:>8} {:?} {}",
            entry.name, entry.size, entry.attributes, entry.mtime
        );
        ControlFlow::Continue(())
    })
}

fn run(card: Mmc, rca: u16, mct: Mct) -> Result<(), FsError> {
    // SAFETY: `run` is called once, and these are the only references to the buffers.
    let (bounce, chunk) = unsafe { (&mut *BOUNCE.0.get(), &mut *CHUNK.0.get()) };
    let device = PartitionDevice::new(card, rca, TARGET, bounce)?;
    let partition = device.partition();
    println!(
        "CACHE (p{}): blocks {}..{}, {} MiB, type {:#04x} in the card's table",
        partition.number,
        partition.start,
        partition.start + partition.count,
        partition.count / 2048,
        partition.kind
    );
    if device.probe()? {
        println!("FAT32 found");
    } else {
        println!("No FAT32 found: formatting CACHE");
        let begin = mct.count();
        let geometry = device.format(mct.count() as u32, LABEL)?;
        println!(
            "Formatted in {} ms: {:?}",
            us(mct.count() - begin) / 1000,
            geometry
        );
        let found = device.probe()?;
        println!(
            "FAT32 found after formatting: {}",
            if found { "OK" } else { "MISMATCH" }
        );
        if !found {
            return Ok(());
        }
    }

    let volumes: VolumeManager<_, _, 4, 4, 1> = VolumeManager::new(device, FixedTime::default());
    let volume = volumes.open_volume(VolumeIdx(0))?;
    let root = volume.open_root_dir()?;
    let boot = count_boot(&root)?;
    println!("BOOTS.TXT: this is boot {} with this file system", boot);
    big_file(&root, mct, chunk)?;
    append_log(&root, boot)?;
    list(&root)?;
    root.close()?;
    volume.close()?;
    Ok(())
}

fn main(_args: BootArgs) -> ! {
    println!("\nFAT32 test");

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
        Ok(ext_csd) if ext_csd.reads_user_area() => {}
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
    let begin = mct.count();
    match run(card, rca, mct) {
        Ok(()) => println!("All steps done in {} ms", us(mct.count() - begin) / 1000),
        Err(error) => println!("ERROR {:x?}", error),
    }
    idle(mct)
}
