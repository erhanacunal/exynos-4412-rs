//! FAT32 on one eMMC partition, through [`embedded_sdmmc`].
//!
//! [`PartitionDevice`] is the [`BlockDevice`] that crate needs. It shows one partition as a
//! whole disk, with a partition table of its own: block 0 is a master boot record made up in
//! memory, which lists one FAT32 partition from block 1 on, and block `n` from 1 on is block
//! `n - 1` of the partition. The card's own table is never changed, so the partition keeps its
//! type byte there. Every write goes through an [`MmcWriter`], which keeps it inside the
//! partition; writes to the made-up block 0 are refused. Writes are cached and merged into
//! multiple block commands: see [`PartitionDevice`] for when they reach the card.
//!
//! [`PartitionDevice::format`] makes an empty FAT32 file system, which `embedded_sdmmc` cannot
//! do. The layout follows Microsoft's FAT specification, `fatgen103`, cited as
//! `fatgen103 <page>`.
//!
//! ```ignore
//! let device = PartitionDevice::new(card, rca, 8, buffer)?;
//! if !device.probe()? {
//!     device.format(0x1234_5678, b"CACHE      ")?;
//! }
//! let volumes = VolumeManager::new(device, FixedTime::default());
//! let volume = volumes.open_volume(VolumeIdx(0))?;
//! // ... write and close files ...
//! volumes.device(|device| device.flush())?;
//! ```

use core::cell::RefCell;
use core::fmt;

pub use embedded_sdmmc;
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, TimeSource, Timestamp};

use crate::drivers::mmc::{self, BLOCK_SIZE, Mmc, MmcWriter, WriteError};
use crate::drivers::timer::Mct;
use crate::mmu;
use crate::partition::Partition;
use crate::soc;
use crate::time::DateTime;

/// The type byte of the made-up partition: FAT32 with LBA addressing.
const TYPE_FAT32_LBA: u8 = 0x0C;
/// Offset of the first partition entry in a master boot record, and of the signature.
const MBR_ENTRY: usize = 0x1BE;
const SIGNATURE_OFFSET: usize = 510;

/// fatgen103 p.9: a boot sector ends with `0x55 0xAA`.
const SIGNATURE: [u8; 2] = [0x55, 0xAA];
/// Reserved sectors before the first FAT. 32 is what fatgen103 p.9 recommends for FAT32.
const RESERVED_SECTORS: u32 = 32;
/// Copies of the FAT.
const FATS: u32 = 2;
/// fatgen103 p.9 `BPB_Media`: a fixed disk.
const MEDIA: u8 = 0xF8;
/// The sector of the FSInfo structure, and of the copy of the boot sector (fatgen103 p.12).
const FSINFO_SECTOR: u32 = 1;
const BACKUP_SECTOR: u32 = 6;
/// The root directory's first cluster.
const ROOT_CLUSTER: u32 = 2;
/// fatgen103 p.15: below this many clusters the volume is FAT16, whatever its boot sector says.
const MIN_FAT32_CLUSTERS: u32 = 65525;
/// fatgen103 p.21-22: the FSInfo signatures.
const FSINFO_LEAD: u32 = 0x4161_5252;
const FSINFO_STRUCT: u32 = 0x6141_7272;
const FSINFO_TRAIL: u32 = 0xAA55_0000;
/// A FAT32 entry uses 28 bits: fatgen103 p.16.
const FAT_ENTRY_MASK: u32 = 0x0FFF_FFFF;
/// fatgen103 p.18: the end of a cluster chain.
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;

/// Why the device failed or refused something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The card failed a read.
    Card(mmc::Error),
    /// The writer failed or refused a write.
    Write(WriteError),
    /// A write to block 0, the master boot record that only exists in memory.
    MadeUpRecord,
    /// A block past the end of the device.
    OutOfRange,
    /// A buffer smaller than [`MIN_BUFFER`], not whole blocks, or not on a cache line.
    BadBuffer,
    /// The partition is too small for FAT32, or too large for this formatter. Holds its blocks.
    BadSize(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x?}", self)
    }
}

impl core::error::Error for Error {}

/// The layout [`PartitionDevice::format`] chose, in sectors of the partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Sectors in the file system: the whole partition.
    pub sectors: u32,
    /// Sectors per cluster.
    pub cluster_sectors: u32,
    /// Sectors in each FAT.
    pub fat_sectors: u32,
    /// The first sector of the data area, where cluster 2 starts.
    pub data_start: u32,
    /// Clusters in the data area.
    pub clusters: u32,
}

impl Geometry {
    /// The layout for a partition of `sectors` blocks.
    pub const fn for_sectors(sectors: u32) -> Result<Self, Error> {
        // fatgen103 p.20 `DskTableFAT32`: 8 sectors per cluster up to 8 GiB, then 16 up to
        // 16 GiB, 32 up to 32 GiB, 64 beyond. Below 260 MiB one sector per cluster.
        let cluster_sectors = match sectors {
            0..=66_600 => return Err(Error::BadSize(sectors)),
            66_601..=532_480 => 1,
            532_481..=16_777_216 => 8,
            16_777_217..=33_554_432 => 16,
            33_554_433..=67_108_864 => 32,
            _ => 64,
        };
        // fatgen103 p.21: the FAT size, which can be a little more than needed, never less.
        let tmp1 = sectors - RESERVED_SECTORS;
        let tmp2 = (256 * cluster_sectors + FATS) / 2;
        let fat_sectors = tmp1.div_ceil(tmp2);
        let data_start = RESERVED_SECTORS + FATS * fat_sectors;
        let clusters = (sectors - data_start) / cluster_sectors;
        // Each cluster, plus the two reserved entries, needs four bytes of FAT.
        if clusters < MIN_FAT32_CLUSTERS
            || (clusters as u64 + 2) * 4 > fat_sectors as u64 * BLOCK_SIZE as u64
            || clusters > FAT_ENTRY_MASK - 16
        {
            return Err(Error::BadSize(sectors));
        }
        Ok(Self { sectors, cluster_sectors, fat_sectors, data_start, clusters })
    }
}

/// Read-ahead windows at the start of the device's buffer, and the blocks each holds. Card
/// reads fill a whole window, from the block asked for on. File data and the FAT are read in
/// turns, so each keeps a window of its own; the one used least recently is refilled.
const WINDOWS: usize = 4;
const WINDOW_BLOCKS: usize = 32;
/// Dirty blocks kept apart from the run: the FAT blocks, both copies, and directory entries
/// that `embedded_sdmmc` writes again and again.
const SLOTS: usize = 8;
/// The smallest buffer [`PartitionDevice::new`] takes: the read-ahead windows, the slots and a
/// run of one block.
pub const MIN_BUFFER: usize = (WINDOWS * WINDOW_BLOCKS + SLOTS + 1) * BLOCK_SIZE;

/// What a [`PartitionDevice`] has done since it was made, in blocks of the partition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Read commands sent to the card.
    pub card_reads: u32,
    /// Blocks they read.
    pub blocks_read: u32,
    /// Blocks read from the cache, written but not yet on the card.
    pub cached_reads: u32,
    /// Blocks read from the read-ahead windows.
    pub window_reads: u32,
    /// Blocks written by `embedded_sdmmc` or the formatter.
    pub blocks_put: u32,
    /// Write commands sent to the card.
    pub card_writes: u32,
    /// Blocks they wrote.
    pub blocks_written: u32,
}

/// The write-back cache, with what it needs to reach the card.
struct Cache<'a> {
    card: Mmc,
    writer: MmcWriter,
    /// The read-ahead windows, where card reads go, since a [`Block`] need not start on a cache
    /// line. They hold the newest data of their blocks: writes update them too.
    windows: &'a mut [u8],
    /// The first block of each window, and how many it holds; 0 for an empty one.
    window_start: [u32; WINDOWS],
    window_len: [usize; WINDOWS],
    /// When each window was last used, from `clock`.
    window_used: [u32; WINDOWS],
    clock: u32,
    /// Single dirty blocks, and which block of the partition each holds.
    slots: &'a mut [u8],
    slot_blocks: [Option<u32>; SLOTS],
    /// Consecutive dirty blocks, from `run_start` on, written with one command.
    run: &'a mut [u8],
    run_start: u32,
    run_len: usize,
    stats: Stats,
}

impl Cache<'_> {
    fn run_capacity(&self) -> usize {
        self.run.len() / BLOCK_SIZE
    }

    fn slot_of(&self, block: u32) -> Option<usize> {
        self.slot_blocks.iter().position(|slot| *slot == Some(block))
    }

    fn run_index(&self, block: u32) -> Option<usize> {
        let index = block.checked_sub(self.run_start)? as usize;
        (index < self.run_len).then_some(index)
    }

    /// The dirty copy of `block`, if the cache holds one.
    fn dirty(&self, block: u32) -> Option<&[u8]> {
        if let Some(index) = self.run_index(block) {
            return Some(&self.run[index * BLOCK_SIZE..][..BLOCK_SIZE]);
        }
        self.slot_of(block).map(|slot| &self.slots[slot * BLOCK_SIZE..][..BLOCK_SIZE])
    }

    fn window_of(&self, block: u32) -> Option<(usize, usize)> {
        (0..WINDOWS).find_map(|window| {
            let index = block.checked_sub(self.window_start[window])? as usize;
            (index < self.window_len[window]).then_some((window, index))
        })
    }

    fn window_block(&mut self, window: usize, index: usize) -> &mut [u8] {
        &mut self.windows[(window * WINDOW_BLOCKS + index) * BLOCK_SIZE..][..BLOCK_SIZE]
    }

    /// Fills the least recently used window from the card, from `block` on, then puts the dirty
    /// blocks over what the card gave. Returns the window.
    fn refill(&mut self, block: u32) -> Result<usize, Error> {
        let partition = self.writer.partition();
        let window = (0..WINDOWS).min_by_key(|window| self.window_used[*window]).unwrap_or(0);
        let blocks = WINDOW_BLOCKS.min((partition.count - block) as usize);
        // Empty while the read is under way, so that a failed one leaves nothing stale.
        self.window_len[window] = 0;
        let bytes = &mut self.windows[window * WINDOW_BLOCKS * BLOCK_SIZE..][..blocks * BLOCK_SIZE];
        self.card.read_blocks_dma(partition.start + block, bytes).map_err(Error::Card)?;
        self.stats.card_reads += 1;
        self.stats.blocks_read += blocks as u32;
        self.window_start[window] = block;
        self.window_len[window] = blocks;
        for index in 0..blocks {
            let lba = block + index as u32;
            let dirty = if let Some(run) = self.run_index(lba) {
                Some((true, run))
            } else {
                self.slot_of(lba).map(|slot| (false, slot))
            };
            if let Some((in_run, at)) = dirty {
                let target = (window * WINDOW_BLOCKS + index) * BLOCK_SIZE;
                let source = if in_run { &self.run[at * BLOCK_SIZE..] } else { &self.slots[at * BLOCK_SIZE..] };
                self.windows[target..target + BLOCK_SIZE].copy_from_slice(&source[..BLOCK_SIZE]);
            }
        }
        Ok(window)
    }

    /// Reads through the cache: dirty blocks first, then the windows, refilled from the card
    /// on a miss.
    fn read(&mut self, block: u32, blocks: &mut [Block]) -> Result<(), Error> {
        for (offset, target) in blocks.iter_mut().enumerate() {
            let lba = block + offset as u32;
            if let Some(data) = self.dirty(lba) {
                target.contents.copy_from_slice(data);
                self.stats.cached_reads += 1;
                continue;
            }
            let (window, index) = match self.window_of(lba) {
                Some(found) => {
                    self.stats.window_reads += 1;
                    found
                }
                None => (self.refill(lba)?, 0),
            };
            self.clock = self.clock.wrapping_add(1);
            self.window_used[window] = self.clock;
            target.contents.copy_from_slice(self.window_block(window, index));
        }
        Ok(())
    }

    /// Takes one written block into the cache. Each block is in at most one place: the run
    /// or a slot.
    fn put(&mut self, block: u32, data: &[u8; BLOCK_SIZE]) -> Result<(), Error> {
        self.stats.blocks_put += 1;
        // Windows can overlap, a refill starting just before another: every copy is kept new.
        for window in 0..WINDOWS {
            if let Some(index) = block.checked_sub(self.window_start[window])
                && (index as usize) < self.window_len[window]
            {
                self.window_block(window, index as usize).copy_from_slice(data);
            }
        }
        if let Some(index) = self.run_index(block) {
            self.run[index * BLOCK_SIZE..][..BLOCK_SIZE].copy_from_slice(data);
            return Ok(());
        }
        if let Some(slot) = self.slot_of(block) {
            self.slots[slot * BLOCK_SIZE..][..BLOCK_SIZE].copy_from_slice(data);
            return Ok(());
        }
        if self.run_len > 0 && block == self.run_start + self.run_len as u32 {
            if self.run_len < self.run_capacity() {
                self.run[self.run_len * BLOCK_SIZE..][..BLOCK_SIZE].copy_from_slice(data);
                self.run_len += 1;
                return Ok(());
            }
            self.flush_run()?;
        }
        if self.run_len > 0 {
            match self.slot_blocks.iter().position(Option::is_none) {
                // A run of one is more likely a FAT or directory block, written before the
                // file data that follows: it moves to a slot, and the run starts again here.
                Some(free) if self.run_len == 1 => {
                    self.slots[free * BLOCK_SIZE..][..BLOCK_SIZE].copy_from_slice(&self.run[..BLOCK_SIZE]);
                    self.slot_blocks[free] = Some(self.run_start);
                    self.run_len = 0;
                }
                Some(free) => {
                    self.slots[free * BLOCK_SIZE..][..BLOCK_SIZE].copy_from_slice(data);
                    self.slot_blocks[free] = Some(block);
                    return Ok(());
                }
                None => self.flush()?,
            }
        }
        self.run[..BLOCK_SIZE].copy_from_slice(data);
        self.run_start = block;
        self.run_len = 1;
        Ok(())
    }

    fn card_write(writer: &MmcWriter, stats: &mut Stats, block: u32, bytes: &[u8]) -> Result<(), Error> {
        writer.write(block, bytes).map_err(Error::Write)?;
        stats.card_writes += 1;
        stats.blocks_written += (bytes.len() / BLOCK_SIZE) as u32;
        Ok(())
    }

    fn flush_run(&mut self) -> Result<(), Error> {
        if self.run_len > 0 {
            Self::card_write(&self.writer, &mut self.stats, self.run_start, &self.run[..self.run_len * BLOCK_SIZE])?;
            self.run_len = 0;
        }
        Ok(())
    }

    /// The run first, so that file data reaches the card before the FAT entries that point at it.
    fn flush(&mut self) -> Result<(), Error> {
        self.flush_run()?;
        for slot in 0..SLOTS {
            if let Some(block) = self.slot_blocks[slot] {
                let bytes = &self.slots[slot * BLOCK_SIZE..][..BLOCK_SIZE];
                Self::card_write(&self.writer, &mut self.stats, block, bytes)?;
                self.slot_blocks[slot] = None;
            }
        }
        Ok(())
    }

    /// Writes `count` zero blocks straight to the card, from `block` on. The cache must be empty.
    fn zero(&mut self, block: u32, count: u32) -> Result<(), Error> {
        self.window_len = [0; WINDOWS];
        self.run.fill(0);
        let per_chunk = self.run_capacity() as u32;
        let mut done = 0;
        while done < count {
            let blocks = per_chunk.min(count - done);
            Self::card_write(&self.writer, &mut self.stats, block + done, &self.run[..blocks as usize * BLOCK_SIZE])?;
            done += blocks;
        }
        Ok(())
    }
}

/// One eMMC partition as a disk for `embedded_sdmmc`. See the module documentation.
///
/// Writes stay in a write-back cache until [`flush`](Self::flush), or until it is full: one
/// run of consecutive blocks, written to the card with one command, and a few single blocks.
/// Reads go through read-ahead windows of 16 KiB, each filled with one command.
/// `embedded_sdmmc` never asks for a flush, so call it after closing files, through
/// `VolumeManager::device`. Dropping the device flushes too, but cannot report a failure.
pub struct PartitionDevice<'a> {
    partition: Partition,
    cache: RefCell<Cache<'a>>,
}

impl<'a> PartitionDevice<'a> {
    /// A device for partition `number`, on the card at address `rca` (see [`MmcWriter::new`]).
    ///
    /// `buffer` must be whole blocks, start on a cache line ([`mmu::DCACHE_LINE`]), lie in DRAM
    /// and hold at least [`MIN_BUFFER`] bytes. 68 KiB of it go to the read-ahead windows and
    /// the single blocks; the rest holds the run, so it sets how much one write command carries.
    pub fn new(card: Mmc, rca: u16, number: u32, buffer: &'a mut [u8]) -> Result<Self, Error> {
        if buffer.len() < MIN_BUFFER
            || !buffer.len().is_multiple_of(BLOCK_SIZE)
            || !(buffer.as_ptr() as usize).is_multiple_of(mmu::DCACHE_LINE)
        {
            return Err(Error::BadBuffer);
        }
        let writer = MmcWriter::new(card, rca, number).map_err(Error::Write)?;
        let (windows, rest) = buffer.split_at_mut(WINDOWS * WINDOW_BLOCKS * BLOCK_SIZE);
        let (slots, run) = rest.split_at_mut(SLOTS * BLOCK_SIZE);
        let cache = Cache {
            card,
            writer,
            windows,
            window_start: [0; WINDOWS],
            window_len: [0; WINDOWS],
            window_used: [0; WINDOWS],
            clock: 0,
            slots,
            slot_blocks: [None; SLOTS],
            run,
            run_start: 0,
            run_len: 0,
            stats: Stats::default(),
        };
        Ok(Self { partition: writer.partition(), cache: RefCell::new(cache) })
    }

    /// The partition, as the card's table gives it.
    pub const fn partition(&self) -> Partition {
        self.partition
    }

    /// What the device has done so far.
    pub fn stats(&self) -> Stats {
        self.cache.borrow().stats
    }

    /// Writes every cached block to the card. Returns once the card has programmed them.
    pub fn flush(&self) -> Result<(), Error> {
        self.cache.borrow_mut().flush()
    }

    fn check_range(&self, block: u32, blocks: usize) -> Result<(), Error> {
        if u64::from(block) + blocks as u64 > u64::from(self.partition.count) {
            return Err(Error::OutOfRange);
        }
        Ok(())
    }

    /// Reads blocks of the partition, counted from its start, including those still in the cache.
    pub fn read_partition(&self, block: u32, blocks: &mut [Block]) -> Result<(), Error> {
        self.check_range(block, blocks.len())?;
        self.cache.borrow_mut().read(block, blocks)
    }

    /// Writes blocks of the partition, counted from its start, into the cache.
    pub fn write_partition(&self, block: u32, blocks: &[Block]) -> Result<(), Error> {
        self.check_range(block, blocks.len())?;
        let mut cache = self.cache.borrow_mut();
        for (offset, data) in blocks.iter().enumerate() {
            cache.put(block + offset as u32, &data.contents)?;
        }
        Ok(())
    }

    /// Whether the partition holds a FAT32 file system this formatter could have made: a boot
    /// sector with 512-byte sectors and a geometry that fits the partition.
    pub fn probe(&self) -> Result<bool, Error> {
        let mut sector = [Block::new()];
        self.read_partition(0, &mut sector)?;
        let data = &sector[0].contents;
        let u16_at = |offset: usize| u16::from_le_bytes([data[offset], data[offset + 1]]);
        let u32_at =
            |offset: usize| u32::from_le_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]);
        let cluster_sectors = u32::from(data[13]);
        let reserved = u32::from(u16_at(14));
        let fats = u32::from(data[16]);
        let total = u32_at(32);
        let fat_sectors = u32_at(36);
        let used = u64::from(reserved) + u64::from(fats) * u64::from(fat_sectors);
        Ok(data[SIGNATURE_OFFSET..] == SIGNATURE
            && u16_at(11) as usize == BLOCK_SIZE
            && cluster_sectors.is_power_of_two()
            && reserved != 0
            && fats != 0
            && u16_at(17) == 0
            && u16_at(19) == 0
            && u16_at(22) == 0
            && fat_sectors != 0
            && total != 0
            && total <= self.partition.count
            && used < u64::from(total)
            && (total - used as u32) / cluster_sectors >= MIN_FAT32_CLUSTERS
            && u16_at(42) == 0
            && &data[82..90] == b"FAT32   ")
    }

    /// Makes an empty FAT32 file system over the whole partition, erasing what it held. The
    /// boot sector is written last, so an interrupted format leaves no volume that
    /// [`probe`](Self::probe) accepts. `label` is padded with spaces, as in the boot sector.
    /// Flushes the cache first, and leaves it empty.
    ///
    /// Writes the reserved sectors, both FATs and the root directory's cluster: under 1 MiB
    /// for an 8 GiB partition.
    pub fn format(&self, volume_id: u32, label: &[u8; 11]) -> Result<Geometry, Error> {
        let geometry = Geometry::for_sectors(self.partition.count)?;
        let root = geometry.data_start + (ROOT_CLUSTER - 2) * geometry.cluster_sectors;
        {
            let mut cache = self.cache.borrow_mut();
            cache.flush()?;
            // Boot sector first, cleared, so that a stale one is gone before anything else.
            cache.zero(0, geometry.data_start)?;
            cache.zero(root, geometry.cluster_sectors)?;
        }

        // fatgen103 p.18: entry 0 holds the media byte, entry 1 the end of chain mark; entry 2,
        // the root directory, is a chain of one cluster.
        let mut fat = Block::new();
        for (index, value) in [FAT_ENTRY_MASK & (0x0FFF_FF00 | u32::from(MEDIA)), END_OF_CHAIN, END_OF_CHAIN]
            .into_iter()
            .enumerate()
        {
            fat.contents[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        for copy in 0..FATS {
            self.write_partition(RESERVED_SECTORS + copy * geometry.fat_sectors, core::slice::from_ref(&fat))?;
        }
        let fsinfo = fsinfo(geometry.clusters - 1, ROOT_CLUSTER + 1);
        self.write_partition(FSINFO_SECTOR, core::slice::from_ref(&fsinfo))?;
        self.write_partition(BACKUP_SECTOR + FSINFO_SECTOR, core::slice::from_ref(&fsinfo))?;
        let boot = boot_sector(&geometry, self.partition.start, volume_id, label);
        self.write_partition(BACKUP_SECTOR, core::slice::from_ref(&boot))?;
        // Everything else reaches the card before the boot sector.
        self.flush()?;
        self.write_partition(0, core::slice::from_ref(&boot))?;
        self.flush()?;
        Ok(geometry)
    }

    /// The made-up master boot record: one bootable-flag-clear FAT32 entry over the partition.
    fn master_boot_record(&self) -> Block {
        let mut block = Block::new();
        let entry = &mut block.contents[MBR_ENTRY..MBR_ENTRY + 16];
        entry[4] = TYPE_FAT32_LBA;
        entry[8..12].copy_from_slice(&1u32.to_le_bytes());
        entry[12..16].copy_from_slice(&self.partition.count.to_le_bytes());
        block.contents[SIGNATURE_OFFSET..].copy_from_slice(&SIGNATURE);
        block
    }
}

impl Drop for PartitionDevice<'_> {
    fn drop(&mut self) {
        // Nowhere to report a failure: callers that care flush first.
        let _ = self.cache.get_mut().flush();
    }
}

impl BlockDevice for PartitionDevice<'_> {
    type Error = Error;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Error> {
        let (blocks, start) = match (blocks, start.0) {
            ([], _) => return Ok(()),
            ([first, rest @ ..], 0) => {
                *first = self.master_boot_record();
                (rest, 1)
            }
            (blocks, start) => (blocks, start),
        };
        if blocks.is_empty() {
            return Ok(());
        }
        self.read_partition(start - 1, blocks)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Error> {
        match start.0 {
            _ if blocks.is_empty() => Ok(()),
            0 => Err(Error::MadeUpRecord),
            start => self.write_partition(start - 1, blocks),
        }
    }

    fn num_blocks(&self) -> Result<BlockCount, Error> {
        Ok(BlockCount(self.partition.count + 1))
    }
}

/// fatgen103 p.9-12: the boot sector and BPB of a FAT32 volume.
fn boot_sector(geometry: &Geometry, hidden: u32, volume_id: u32, label: &[u8; 11]) -> Block {
    let mut block = Block::new();
    let data = &mut block.contents;
    let mut put = |offset: usize, bytes: &[u8]| data[offset..offset + bytes.len()].copy_from_slice(bytes);
    // A jump over the BPB, then the OEM name.
    put(0, &[0xEB, 0x58, 0x90]);
    put(3, b"EXYNOS  ");
    put(11, &(BLOCK_SIZE as u16).to_le_bytes());
    put(13, &[geometry.cluster_sectors as u8]);
    put(14, &(RESERVED_SECTORS as u16).to_le_bytes());
    put(16, &[FATS as u8]);
    // Root entries, 16-bit total and 16-bit FAT size stay zero on FAT32.
    put(21, &[MEDIA]);
    // A geometry for tools that still want one: 63 sectors per track, 255 heads.
    put(24, &63u16.to_le_bytes());
    put(26, &255u16.to_le_bytes());
    // Sectors before the volume on the disk: the partition's start on the card.
    put(28, &hidden.to_le_bytes());
    put(32, &geometry.sectors.to_le_bytes());
    put(36, &geometry.fat_sectors.to_le_bytes());
    // Flags 0: the FATs are mirrored. Version 0.0.
    put(44, &ROOT_CLUSTER.to_le_bytes());
    put(48, &(FSINFO_SECTOR as u16).to_le_bytes());
    put(50, &(BACKUP_SECTOR as u16).to_le_bytes());
    // Drive number 0x80, then the extended boot signature that says the next three fields
    // are there.
    put(64, &[0x80, 0, 0x29]);
    put(67, &volume_id.to_le_bytes());
    put(71, label);
    put(82, b"FAT32   ");
    put(SIGNATURE_OFFSET, &SIGNATURE);
    block
}

/// fatgen103 p.21-22: the FSInfo sector, with the free cluster count and where to look next.
fn fsinfo(free: u32, next_free: u32) -> Block {
    let mut block = Block::new();
    let data = &mut block.contents;
    data[0..4].copy_from_slice(&FSINFO_LEAD.to_le_bytes());
    data[484..488].copy_from_slice(&FSINFO_STRUCT.to_le_bytes());
    data[488..492].copy_from_slice(&free.to_le_bytes());
    data[492..496].copy_from_slice(&next_free.to_le_bytes());
    data[508..512].copy_from_slice(&FSINFO_TRAIL.to_le_bytes());
    block
}

/// A clock that always gives the same time, for a board without a running real-time clock.
#[derive(Clone, Copy, Debug)]
pub struct FixedTime(pub Timestamp);

impl Default for FixedTime {
    /// 2026-01-01 00:00:00.
    fn default() -> Self {
        Self(Timestamp {
            year_since_1970: (2026 - 1970) as u8,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        })
    }
}

impl TimeSource for FixedTime {
    fn get_timestamp(&self) -> Timestamp {
        self.0
    }
}

/// A clock that starts at a given time and runs on the MCT, such as one read from the RTC at
/// boot: [`max77686::Rtc`](crate::drivers::max77686::Rtc).
///
/// FAT stores local time, so `start` should be local time too.
#[derive(Clone, Copy, Debug)]
pub struct MctClock {
    start: DateTime,
    start_count: u64,
    mct: Mct,
}

impl MctClock {
    /// A clock that reads `start` now, on `mct`, which must be running.
    pub fn new(start: DateTime, mct: Mct) -> Self {
        Self { start, start_count: mct.count(), mct }
    }

    /// The time now.
    pub fn now(&self) -> DateTime {
        let elapsed = (self.mct.count() - self.start_count) / u64::from(soc::MCT_CLOCK_HZ);
        self.start.add_seconds(elapsed as i64)
    }
}

impl TimeSource for MctClock {
    /// The time now. FAT timestamps cover 1980 to 2107, and `Timestamp` 1970 to 2225: a time
    /// outside gives the nearest end of 1980 to 2107.
    fn get_timestamp(&self) -> Timestamp {
        let now = self.now();
        let now = match now.year {
            ..1980 => DateTime { year: 1980, month: 1, day: 1, hour: 0, minute: 0, second: 0 },
            2108.. => DateTime { year: 2107, month: 12, day: 31, hour: 23, minute: 59, second: 58 },
            _ => now,
        };
        Timestamp {
            year_since_1970: (now.year - 1970) as u8,
            zero_indexed_month: now.month - 1,
            zero_indexed_day: now.day - 1,
            hours: now.hour,
            minutes: now.minute,
            seconds: now.second,
        }
    }
}
