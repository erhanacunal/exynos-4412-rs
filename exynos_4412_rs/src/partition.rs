//! MBR partition table, with its chain of extended boot records.
//!
//! Only parses. The layout is that of Linux 5.15.198 `include/linux/msdos_partition.h`,
//! cited as `msdos_partition.h:<line>`, and the numbering that of `block/partitions/msdos.c`:
//! primary partitions are 1 to 4 by slot, logical ones count from 5.

/// Bytes in the blocks this reads.
pub const BLOCK_SIZE: usize = 512;

/// Offset of the first of four entries: `block/partitions/msdos.c:74`.
const TABLE_OFFSET: usize = 0x1BE;
const ENTRY_SIZE: usize = 16;
/// msdos_partition.h:12 `sys_ind`, the fifth byte of an entry.
const ENTRY_TYPE: usize = 4;
/// `start_sect`, then `nr_sects`, the last two fields of an entry (msdos_partition.h:16-17).
const ENTRY_START: usize = 8;
const ENTRY_COUNT: usize = 12;
/// msdos_partition.h:5 `MSDOS_LABEL_MAGIC`, in the last two bytes.
const SIGNATURE: u16 = 0xAA55;
/// msdos_partition.h:26 `DOS_EXTENDED_PARTITION`.
const TYPE_EXTENDED: u8 = 0x05;
/// msdos_partition.h:28 `WIN98_EXTENDED_PARTITION`.
const TYPE_EXTENDED_LBA: u8 = 0x0F;
/// msdos_partition.h:27 `LINUX_EXTENDED_PARTITION`.
const TYPE_EXTENDED_LINUX: u8 = 0x85;
/// Stops a table that links back to itself.
const MAX_LOGICAL: u32 = 64;

/// Something blocks can be read from.
pub trait BlockRead {
    /// What a failed read gives.
    type Error;

    /// Reads one block.
    fn read_block(&self, lba: u32, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), Self::Error>;
}

/// One partition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Partition {
    /// Its number, as Linux gives it: `mmcblk0p<number>`.
    pub number: u32,
    /// The type byte.
    pub kind: u8,
    /// First block.
    pub start: u32,
    /// Number of blocks.
    pub count: u32,
    /// The block of the table that describes it: 0, the MBR, for a primary partition, or its
    /// extended boot record for a logical one.
    pub record: u32,
}

impl Partition {
    /// Whether this is the container of the logical partitions.
    pub const fn is_extended(&self) -> bool {
        matches!(self.kind, TYPE_EXTENDED | TYPE_EXTENDED_LBA | TYPE_EXTENDED_LINUX)
    }
}

/// Why the table could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// Reading a block failed.
    Read(E),
    /// This block lacks the signature of a partition table.
    NoSignature(u32),
    /// The table has more partitions than the room given, or a chain with no end.
    TooMany,
}

fn entry(block: &[u8; BLOCK_SIZE], slot: usize) -> (u8, u32, u32) {
    let raw = &block[TABLE_OFFSET + slot * ENTRY_SIZE..][..ENTRY_SIZE];
    let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
    (raw[ENTRY_TYPE], word(ENTRY_START), word(ENTRY_COUNT))
}

fn read_table<D: BlockRead>(
    device: &D,
    lba: u32,
    block: &mut [u8; BLOCK_SIZE],
) -> Result<(), Error<D::Error>> {
    device.read_block(lba, block).map_err(Error::Read)?;
    if u16::from_le_bytes([block[BLOCK_SIZE - 2], block[BLOCK_SIZE - 1]]) != SIGNATURE {
        return Err(Error::NoSignature(lba));
    }
    Ok(())
}

/// Fills `partitions` from the table at the start of `device`, and returns how many it found.
pub fn scan<D: BlockRead>(device: &D, partitions: &mut [Partition]) -> Result<usize, Error<D::Error>> {
    let mut block = [0; BLOCK_SIZE];
    read_table(device, 0, &mut block)?;

    let mut found = 0;
    let mut extended = None;
    for slot in 0..4 {
        let (kind, start, count) = entry(&block, slot);
        if kind == 0 || count == 0 {
            continue;
        }
        let partition = Partition { number: slot as u32 + 1, kind, start, count, record: 0 };
        if partition.is_extended() && extended.is_none() {
            extended = Some(start);
        }
        *partitions.get_mut(found).ok_or(Error::TooMany)? = partition;
        found += 1;
    }

    // Each record holds one logical partition, relative to the record itself, and a link to
    // the next record, relative to the start of the extended partition.
    let Some(base) = extended else {
        return Ok(found);
    };
    let mut record = base;
    for number in 5..5 + MAX_LOGICAL {
        read_table(device, record, &mut block)?;
        let (kind, start, count) = entry(&block, 0);
        if kind != 0 && count != 0 {
            let partition = Partition { number, kind, start: record + start, count, record };
            *partitions.get_mut(found).ok_or(Error::TooMany)? = partition;
            found += 1;
        }
        let (next_kind, next_start, next_count) = entry(&block, 1);
        if next_kind == 0 || next_count == 0 {
            return Ok(found);
        }
        record = base + next_start;
    }
    Err(Error::TooMany)
}
