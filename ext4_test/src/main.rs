//! ext4 read test, with the `ext4-view` crate over the eMMC driver's DMA reads. Reads only: the
//! driver has no command that can change the card, and `ext4-view` only reads.
//!
//! Loads the file systems on EFS, SYSTEM and USERDATA, and prints each one's label, UUID and how
//! many entries its root has. Beyond that:
//!
//! - EFS holds calibration data: nothing more.
//! - SYSTEM, not an Android system on this board: the root's names and a walk of the tree.
//! - USERDATA holds the Linux root file system: the root's names, the distribution's name from
//!   `/etc/os-release`, a walk of the whole tree, and two timed reads of the largest file under
//!   4 MiB in `/usr`.
//!
//! A walk prints only counts, so no names from places such as `/home` appear.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mmc::{self, BLOCK_SIZE, Mmc};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::partition::{self, BlockRead, Partition};
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};
use ext4_view::{Ext4, Ext4Error, Ext4Read, PathBuf};
use linked_list_allocator::LockedHeap;

exynos_4412_rs::entry!(main);

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// The heap: `ext4-view` keeps its caches and what it reads there.
const HEAP_BYTES: usize = 8 << 20;

#[repr(C, align(32))]
struct Heap(UnsafeCell<[u8; HEAP_BYTES]>);

// SAFETY: only `main` touches it, once, to hand it to the allocator.
unsafe impl Sync for Heap {}

static HEAP: Heap = Heap(UnsafeCell::new([0; HEAP_BYTES]));

/// What one DMA read through the bounce buffer moves at most.
const BOUNCE_BYTES: usize = 64 * 1024;

/// The bounce buffer, on a cache line as DMA needs.
#[repr(C, align(32))]
struct Bounce([u8; BOUNCE_BYTES]);

/// What to do with one file system.
struct Plan {
    number: u32,
    name: &'static str,
    /// Print the root's names, not only their count.
    list_root: bool,
    /// Walk the whole tree, counting.
    walk: bool,
    /// Print its lines that start with one of these keys.
    info: Option<(&'static str, &'static [&'static str])>,
    /// Read the largest file under [`LARGEST_LIMIT`] in this directory twice.
    read_from: Option<&'static str>,
}

const FILE_SYSTEMS: [Plan; 3] = [
    Plan { number: 3, name: "EFS", list_root: false, walk: false, info: None, read_from: None },
    Plan { number: 9, name: "SYSTEM", list_root: true, walk: true, info: None, read_from: None },
    Plan {
        number: 10,
        name: "USERDATA",
        list_root: true,
        walk: true,
        info: Some(("/etc/os-release", &["PRETTY_NAME=", "BUILD_ID=", "VERSION_ID="])),
        read_from: Some("/usr"),
    },
];
/// The largest file read must fit the heap twice, with room left.
const LARGEST_LIMIT: u64 = 4 << 20;

/// What the reader has done: calls, bytes, and their time in MCT counts.
static READ_CALLS: AtomicU32 = AtomicU32::new(0);
static READ_BYTES: AtomicU64 = AtomicU64::new(0);
static READ_TIME: AtomicU64 = AtomicU64::new(0);

/// Why the reader could not give `ext4-view` its bytes.
#[derive(Debug)]
enum ReadError {
    /// Past the end of the partition.
    OutOfRange { start: u64, length: usize },
    /// The card read failed.
    Card(mmc::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { start, length } => write!(f, "{} bytes at {} are past the partition", length, start),
            Self::Card(error) => write!(f, "card read failed: {:x?}", error),
        }
    }
}

impl core::error::Error for ReadError {}

/// A partition as `ext4-view` reads it: bytes from its first block on.
struct PartitionReader {
    card: Mmc,
    mct: Mct,
    start: u32,
    blocks: u32,
    bounce: Box<Bounce>,
}

impl PartitionReader {
    fn new(card: Mmc, mct: Mct, partition: &Partition) -> Self {
        // SAFETY: all zeros is a valid `Bounce`, a byte array.
        let bounce = unsafe { Box::<Bounce>::new_zeroed().assume_init() };
        Self { card, mct, start: partition.start, blocks: partition.count, bounce }
    }

    fn read_into(&mut self, start_byte: u64, dst: &mut [u8]) -> Result<(), ReadError> {
        let size = u64::from(self.blocks) * BLOCK_SIZE as u64;
        if start_byte.checked_add(dst.len() as u64).is_none_or(|end| end > size) {
            return Err(ReadError::OutOfRange { start: start_byte, length: dst.len() });
        }
        let mut done = 0;
        while done < dst.len() {
            let position = start_byte + done as u64;
            let lba = self.start + (position / BLOCK_SIZE as u64) as u32;
            let skip = (position % BLOCK_SIZE as u64) as usize;
            let rest = &mut dst[done..];
            // Whole blocks into a buffer on a cache line go straight there.
            if skip == 0 && rest.len() >= BLOCK_SIZE && (rest.as_ptr() as usize).is_multiple_of(mmu::DCACHE_LINE) {
                let length = rest.len() / BLOCK_SIZE * BLOCK_SIZE;
                self.card.read_blocks_dma(lba, &mut rest[..length]).map_err(ReadError::Card)?;
                done += length;
                continue;
            }
            let length = (skip + rest.len()).next_multiple_of(BLOCK_SIZE).min(BOUNCE_BYTES);
            let bounce = &mut self.bounce.0[..length];
            self.card.read_blocks_dma(lba, bounce).map_err(ReadError::Card)?;
            let count = (length - skip).min(rest.len());
            rest[..count].copy_from_slice(&bounce[skip..skip + count]);
            done += count;
        }
        Ok(())
    }
}

impl Ext4Read for PartitionReader {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> Result<(), Box<dyn core::error::Error + Send + Sync + 'static>> {
        let begin = self.mct.count();
        let result = self.read_into(start_byte, dst);
        READ_CALLS.fetch_add(1, Ordering::Relaxed);
        READ_BYTES.fetch_add(dst.len() as u64, Ordering::Relaxed);
        READ_TIME.fetch_add(self.mct.count() - begin, Ordering::Relaxed);
        result.map_err(|error| Box::new(error) as _)
    }
}

/// The partition table reader: single blocks, polled.
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
    bytes.iter().fold(0x811C_9DC5, |hash: u32, byte| (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193))
}

fn heap_used() -> usize {
    ALLOCATOR.lock().used()
}

fn reset_read_stats() {
    READ_CALLS.store(0, Ordering::Relaxed);
    READ_BYTES.store(0, Ordering::Relaxed);
    READ_TIME.store(0, Ordering::Relaxed);
}

fn report_read_stats(what: &str) {
    println!(
        "  {}: {} reads, {} KiB, {} us in the reader",
        what,
        READ_CALLS.load(Ordering::Relaxed),
        READ_BYTES.load(Ordering::Relaxed) / 1024,
        us(READ_TIME.load(Ordering::Relaxed)),
    );
}

/// Label, UUID and the root's entries, with their names if `list`.
fn report_root(fs: &Ext4, list: bool) -> Result<(), Ext4Error> {
    println!("  label {:?}, UUID {}", fs.label().to_str().unwrap_or("not UTF-8"), fs.uuid());
    let mut count = 0;
    for entry in fs.read_dir("/")? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_str() == Ok(".") || name.as_str() == Ok("..") {
            continue;
        }
        count += 1;
        if list {
            let kind = entry.file_type()?;
            let suffix = if kind.is_dir() { "/" } else if kind.is_symlink() { " ->" } else { "" };
            println!("    {}{}", name.display(), suffix);
        }
    }
    println!("  root: {} entries", count);
    Ok(())
}

fn report_info(fs: &Ext4, path: &str, keys: &[&str]) -> Result<(), Ext4Error> {
    let text = fs.read_to_string(path)?;
    println!("  {}: {} bytes", path, text.len());
    for line in text.lines().filter(|line| keys.iter().any(|key| line.starts_with(key))) {
        println!("    {}", line);
    }
    Ok(())
}

/// What a walk of the whole tree finds.
#[derive(Default)]
struct Walk {
    directories: u32,
    files: u32,
    symlinks: u32,
    others: u32,
    bytes: u64,
    largest: Option<(PathBuf, u64)>,
}

/// Visits every entry under `top`, without following symbolic links. Keeps the largest regular
/// file under [`LARGEST_LIMIT`].
fn walk(fs: &Ext4, top: &str) -> Result<Walk, Ext4Error> {
    let mut found = Walk { directories: 1, ..Walk::default() };
    let mut pending: Vec<PathBuf> = Vec::new();
    // `top` is one of this file's constants.
    pending.push(PathBuf::try_from(top).expect("a valid path"));
    while let Some(directory) = pending.pop() {
        for entry in fs.read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.as_str() == Ok(".") || name.as_str() == Ok("..") {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                found.directories += 1;
                pending.push(entry.path());
            } else if kind.is_symlink() {
                found.symlinks += 1;
            } else if kind.is_regular_file() {
                found.files += 1;
                let length = entry.metadata()?.len();
                found.bytes += length;
                let larger = found.largest.as_ref().is_none_or(|(_, best)| length > *best);
                if length <= LARGEST_LIMIT && larger {
                    found.largest = Some((entry.path(), length));
                }
            } else {
                found.others += 1;
            }
        }
    }
    Ok(found)
}

fn report_walk(mct: Mct, fs: &Ext4) -> Result<(), Ext4Error> {
    reset_read_stats();
    let begin = mct.count();
    let found = walk(fs, "/")?;
    println!(
        "  walk: {} directories, {} files ({} MiB), {} symbolic links, {} others, in {} ms",
        found.directories,
        found.files,
        found.bytes >> 20,
        found.symlinks,
        found.others,
        us(mct.count() - begin) / 1000,
    );
    report_read_stats("walk");
    Ok(())
}

/// Reads the largest file in `top` twice, timing it. The passes must agree.
fn report_largest(mct: Mct, fs: &Ext4, top: &str) -> Result<(), Ext4Error> {
    let Some((path, length)) = walk(fs, top)?.largest else {
        println!("  no regular file to read in {}", top);
        return Ok(());
    };
    let mut hashes = [0; 2];
    for (pass, hash) in hashes.iter_mut().enumerate() {
        reset_read_stats();
        let begin = mct.count();
        let data = fs.read(&path)?;
        let time = us(mct.count() - begin);
        *hash = fold(&data);
        println!(
            "  read {} ({} bytes), pass {}: {} bytes in {} us, {} KiB/s, hash {:#010x}, heap {} KiB in use",
            path.display(),
            length,
            pass + 1,
            data.len(),
            time,
            data.len() as u64 * 1_000_000 / 1024 / time.max(1),
            *hash,
            heap_used() / 1024,
        );
        report_read_stats("read");
    }
    println!("  both passes: {}", if hashes[0] == hashes[1] { "OK" } else { "MISMATCH" });
    Ok(())
}

fn report(mct: Mct, fs: &Ext4, plan: &Plan) -> Result<(), Ext4Error> {
    report_root(fs, plan.list_root)?;
    if let Some((path, keys)) = plan.info {
        report_info(fs, path, keys)?;
    }
    if plan.walk {
        report_walk(mct, fs)?;
    }
    if let Some(top) = plan.read_from {
        report_largest(mct, fs, top)?;
    }
    Ok(())
}

fn main(_args: BootArgs) -> ! {
    println!("\next4 read test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    // SAFETY: the heap is a static that nothing else uses, handed over once, after the MMU is
    // on, so the allocator's lock works.
    unsafe { ALLOCATOR.lock().init(HEAP.0.get().cast::<u8>(), HEAP_BYTES) };

    let card = match Mmc::new() {
        Ok(card) => card,
        Err(error) => {
            println!("Controller ERROR {:x?}", error);
            idle(mct);
        }
    };
    match card.read_ext_csd() {
        Ok(ext_csd) if ext_csd.reads_user_area() => {}
        Ok(_) => {
            println!("Reads do not come from the user area (MISMATCH): stopping here");
            idle(mct);
        }
        Err(error) => {
            println!("EXT_CSD ERROR {:x?}", error);
            idle(mct);
        }
    }
    let mut partitions = [Partition::default(); 16];
    let found = match partition::scan(&Device(card), &mut partitions) {
        Ok(found) => found,
        Err(error) => {
            println!("Partition table ERROR {:x?}", error);
            idle(mct);
        }
    };
    let partitions = &partitions[..found];

    for plan in &FILE_SYSTEMS {
        let Some(partition) = partitions.iter().find(|partition| partition.number == plan.number) else {
            println!("p{} {}: not in the table", plan.number, plan.name);
            continue;
        };
        println!("p{} {}: {} MiB", plan.number, plan.name, partition.count / 2048);
        reset_read_stats();
        let begin = mct.count();
        let fs = match Ext4::load(Box::new(PartitionReader::new(card, mct, partition))) {
            Ok(fs) => fs,
            Err(error) => {
                println!("  load ERROR {:?}", error);
                continue;
            }
        };
        println!("  loaded in {} us, heap {} KiB in use", us(mct.count() - begin), heap_used() / 1024);
        report_read_stats("load");
        if let Err(error) = report(mct, &fs, plan) {
            println!("  ERROR {:?}", error);
        }
    }
    println!("Heap {} KiB in use at the end", heap_used() / 1024);

    idle(mct);
}
