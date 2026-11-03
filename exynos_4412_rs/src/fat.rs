//! FAT32 on one eMMC partition, through [`embedded_sdmmc`].
//!
//! [`PartitionDevice`] is the [`BlockDevice`] that crate needs. It shows one partition as a
//! whole disk, with a partition table of its own: block 0 is a master boot record made up in
//! memory, which lists one FAT32 partition from block 1 on, and block `n` from 1 on is block
//! `n - 1` of the partition. The card's own table is never changed, so the partition keeps its
//! type byte there. Every write goes through an [`MmcWriter`], which keeps it inside the
//! partition; writes to the made-up block 0 are refused.
//!
//! [`PartitionDevice::format`] makes an empty FAT32 file system, which `embedded_sdmmc` cannot
//! do. The layout follows Microsoft's FAT specification, `fatgen103`, cited as
//! `fatgen103 <page>`.
//!
//! ```ignore
//! let device = PartitionDevice::new(card, rca, 8, bounce)?;
//! if !device.probe()? {
//!     device.format(0x1234_5678, b"CACHE      ")?;
//! }
//! let volumes = VolumeManager::new(device, FixedTime::default());
//! let volume = volumes.open_volume(VolumeIdx(0))?;
//! ```

use core::cell::RefCell;
use core::fmt;

pub use embedded_sdmmc;
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, TimeSource, Timestamp};

use crate::drivers::mmc::{self, BLOCK_SIZE, Mmc, MmcWriter, WriteError};
use crate::mmu;
use crate::partition::Partition;

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
    /// A bounce buffer that is empty, not whole blocks, or not on a cache line.
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

/// One eMMC partition as a disk for `embedded_sdmmc`. See the module documentation.
pub struct PartitionDevice<'a> {
    card: Mmc,
    writer: MmcWriter,
    /// Where the DMA reads and writes go, since a [`Block`] need not start on a cache line.
    bounce: RefCell<&'a mut [u8]>,
}

impl<'a> PartitionDevice<'a> {
    /// A device for partition `number`, on the card at address `rca` (see [`MmcWriter::new`]).
    /// `bounce` must be whole blocks and start on a cache line ([`mmu::DCACHE_LINE`]); 32 KiB
    /// is plenty, as `embedded_sdmmc` moves one block at a time.
    pub fn new(card: Mmc, rca: u16, number: u32, bounce: &'a mut [u8]) -> Result<Self, Error> {
        if bounce.is_empty()
            || !bounce.len().is_multiple_of(BLOCK_SIZE)
            || !(bounce.as_ptr() as usize).is_multiple_of(mmu::DCACHE_LINE)
        {
            return Err(Error::BadBuffer);
        }
        let writer = MmcWriter::new(card, rca, number).map_err(Error::Write)?;
        Ok(Self { card, writer, bounce: RefCell::new(bounce) })
    }

    /// The partition, as the card's table gives it.
    pub const fn partition(&self) -> Partition {
        self.writer.partition()
    }

    /// Reads blocks of the partition, counted from its start.
    pub fn read_partition(&self, block: u32, blocks: &mut [Block]) -> Result<(), Error> {
        let partition = self.partition();
        if u64::from(block) + blocks.len() as u64 > u64::from(partition.count) {
            return Err(Error::OutOfRange);
        }
        let mut bounce = self.bounce.borrow_mut();
        let per_chunk = bounce.len() / BLOCK_SIZE;
        let mut lba = partition.start + block;
        for chunk in blocks.chunks_mut(per_chunk) {
            let bytes = &mut bounce[..chunk.len() * BLOCK_SIZE];
            self.card.read_blocks_dma(lba, bytes).map_err(Error::Card)?;
            for (block, data) in chunk.iter_mut().zip(bytes.as_chunks::<BLOCK_SIZE>().0) {
                block.contents.copy_from_slice(data);
            }
            lba += chunk.len() as u32;
        }
        Ok(())
    }

    /// Writes blocks of the partition, counted from its start.
    pub fn write_partition(&self, block: u32, blocks: &[Block]) -> Result<(), Error> {
        let mut bounce = self.bounce.borrow_mut();
        let per_chunk = bounce.len() / BLOCK_SIZE;
        let mut block = block;
        for chunk in blocks.chunks(per_chunk) {
            let bytes = &mut bounce[..chunk.len() * BLOCK_SIZE];
            for (data, block) in bytes.as_chunks_mut::<BLOCK_SIZE>().0.iter_mut().zip(chunk) {
                data.copy_from_slice(&block.contents);
            }
            self.writer.write(block, bytes).map_err(Error::Write)?;
            block += chunk.len() as u32;
        }
        Ok(())
    }

    /// Writes `count` zero blocks of the partition, from `block` on.
    fn zero_partition(&self, block: u32, count: u32) -> Result<(), Error> {
        let mut bounce = self.bounce.borrow_mut();
        bounce.fill(0);
        let per_chunk = (bounce.len() / BLOCK_SIZE) as u32;
        let mut done = 0;
        while done < count {
            let blocks = per_chunk.min(count - done);
            self.writer.write(block + done, &bounce[..blocks as usize * BLOCK_SIZE]).map_err(Error::Write)?;
            done += blocks;
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
            && total <= self.partition().count
            && used < u64::from(total)
            && (total - used as u32) / cluster_sectors >= MIN_FAT32_CLUSTERS
            && u16_at(42) == 0
            && &data[82..90] == b"FAT32   ")
    }

    /// Makes an empty FAT32 file system over the whole partition, erasing what it held. The
    /// boot sector is written last, so an interrupted format leaves no volume that
    /// [`probe`](Self::probe) accepts. `label` is padded with spaces, as in the boot sector.
    ///
    /// Writes the reserved sectors, both FATs and the root directory's cluster: under 1 MiB
    /// for an 8 GiB partition.
    pub fn format(&self, volume_id: u32, label: &[u8; 11]) -> Result<Geometry, Error> {
        let geometry = Geometry::for_sectors(self.partition().count)?;
        let root = geometry.data_start + (ROOT_CLUSTER - 2) * geometry.cluster_sectors;
        // Boot sector first, cleared, so that a stale one is gone before anything else.
        self.zero_partition(0, geometry.data_start)?;
        self.zero_partition(root, geometry.cluster_sectors)?;

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
        let boot = boot_sector(&geometry, self.partition().start, volume_id, label);
        self.write_partition(BACKUP_SECTOR, core::slice::from_ref(&boot))?;
        self.write_partition(0, core::slice::from_ref(&boot))?;
        Ok(geometry)
    }

    /// The made-up master boot record: one bootable-flag-clear FAT32 entry over the partition.
    fn master_boot_record(&self) -> Block {
        let mut block = Block::new();
        let entry = &mut block.contents[MBR_ENTRY..MBR_ENTRY + 16];
        entry[4] = TYPE_FAT32_LBA;
        entry[8..12].copy_from_slice(&1u32.to_le_bytes());
        entry[12..16].copy_from_slice(&self.partition().count.to_le_bytes());
        block.contents[SIGNATURE_OFFSET..].copy_from_slice(&SIGNATURE);
        block
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
        Ok(BlockCount(self.partition().count + 1))
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
