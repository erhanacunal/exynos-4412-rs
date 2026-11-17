//! ext4 on one eMMC partition, through [`ext4_rs`] (journaled; it formats its own file systems).
//!
//! A crate of its own, not a feature of `exynos_4412_rs`: `ext4_rs` needs `alloc`, and a feature
//! turned on by one application of the workspace would reach all of them, most without an
//! allocator.
//!
//! [`PartitionBlocks`] is the [`BlockDevice`] that crate needs: 4 KiB blocks of one partition,
//! counted from its start, written through an [`MmcWriter`], so nothing outside the partition
//! can be reached. Every write returns once the card has programmed it (the card's own cache is
//! never turned on), so [`flush`](BlockDevice::flush) has nothing left to do.
//!
//! The IDMAC needs buffers on a cache line in DRAM: the caller's buffer is used as it is when it
//! is one, otherwise the data goes through the bounce buffer, a piece at a time.
//!
//! ```ignore
//! let device = PartitionBlocks::new(card, rca, 10, bounce, clock)?;
//! let fs = Ext4::mount(device, MountOptions::default())?;
//! ```

#![no_std]
#![warn(missing_docs)]

pub use ext4_rs;
use ext4_rs::{BlockDevice, IoError};

use exynos_4412_rs::drivers::mmc::{self, Mmc, MmcWriter, WriteError};
use exynos_4412_rs::mmu;
use exynos_4412_rs::partition::Partition;
use exynos_4412_rs::soc;

/// Bytes of an ext4 block.
pub const BLOCK_BYTES: usize = ext4_rs::BLOCK_SIZE;
const SECTORS: u32 = (BLOCK_BYTES / mmc::BLOCK_SIZE) as u32;

/// Why [`PartitionBlocks::new`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The writer could not be made: no such partition, or the card refused.
    Write(WriteError),
    /// The bounce buffer is not whole blocks, not on a cache line, or not in DRAM.
    BadBuffer,
}

/// What the device has done so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Read calls.
    pub reads: u32,
    /// Blocks they read.
    pub blocks_read: u64,
    /// Write calls.
    pub writes: u32,
    /// Blocks they wrote.
    pub blocks_written: u64,
    /// Blocks that went through the bounce buffer.
    pub bounced: u64,
    /// Reads and writes the card failed.
    pub errors: u32,
}

/// One partition as an ext4 device. See the module documentation.
pub struct PartitionBlocks<'a> {
    writer: MmcWriter,
    bounce: &'a mut [u8],
    blocks: u64,
    clock: fn() -> u32,
    stats: Stats,
}

fn dma_ok(buffer: &[u8]) -> bool {
    let start = buffer.as_ptr() as usize;
    start.is_multiple_of(mmu::DCACHE_LINE)
        && start >= soc::DRAM_BASE
        && start + buffer.len() <= soc::DRAM_BASE + soc::DRAM_SIZE
}

impl<'a> PartitionBlocks<'a> {
    /// Partition `number` of the card at `rca` (see [`MmcWriter::new`]). `bounce`: whole 4 KiB
    /// blocks, on a cache line, in DRAM; 64 KiB or more keeps unaligned transfers fast. `clock`
    /// gives seconds since 1970 for timestamps (0 when unknown).
    pub fn new(card: Mmc, rca: u16, number: u32, bounce: &'a mut [u8], clock: fn() -> u32) -> Result<Self, Error> {
        if bounce.is_empty() || !bounce.len().is_multiple_of(BLOCK_BYTES) || !dma_ok(bounce) {
            return Err(Error::BadBuffer);
        }
        let writer = MmcWriter::new(card, rca, number).map_err(Error::Write)?;
        let blocks = u64::from(writer.partition().count / SECTORS);
        Ok(Self { writer, bounce, blocks, clock, stats: Stats::default() })
    }

    /// The partition, as the card's table gives it.
    pub const fn partition(&self) -> Partition {
        self.writer.partition()
    }

    /// What the device has done so far.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn sector(&self, block: u64, bytes: usize) -> Result<u32, IoError> {
        let count = (bytes / BLOCK_BYTES) as u64;
        if bytes == 0 || !bytes.is_multiple_of(BLOCK_BYTES) || block + count > self.blocks {
            return Err(IoError);
        }
        Ok(block as u32 * SECTORS)
    }

    fn failed<T>(&mut self, r: Result<T, WriteError>) -> Result<T, IoError> {
        r.map_err(|_| {
            self.stats.errors += 1;
            IoError
        })
    }
}

impl BlockDevice for PartitionBlocks<'_> {
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let sector = self.sector(block, buf.len())?;
        self.stats.reads += 1;
        self.stats.blocks_read += (buf.len() / BLOCK_BYTES) as u64;
        if dma_ok(buf) {
            let r = self.writer.read(sector, buf);
            return self.failed(r);
        }
        let step = self.bounce.len();
        let mut at = sector;
        for chunk in buf.chunks_mut(step) {
            let r = self.writer.read(at, &mut self.bounce[..chunk.len()]);
            self.failed(r)?;
            chunk.copy_from_slice(&self.bounce[..chunk.len()]);
            self.stats.bounced += (chunk.len() / BLOCK_BYTES) as u64;
            at += (chunk.len() / mmc::BLOCK_SIZE) as u32;
        }
        Ok(())
    }

    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), IoError> {
        let sector = self.sector(block, data.len())?;
        self.stats.writes += 1;
        self.stats.blocks_written += (data.len() / BLOCK_BYTES) as u64;
        if dma_ok(data) {
            let r = self.writer.write(sector, data);
            return self.failed(r);
        }
        let step = self.bounce.len();
        let mut at = sector;
        for chunk in data.chunks(step) {
            self.bounce[..chunk.len()].copy_from_slice(chunk);
            let r = self.writer.write(at, &self.bounce[..chunk.len()]);
            self.failed(r)?;
            self.stats.bounced += (chunk.len() / BLOCK_BYTES) as u64;
            at += (chunk.len() / mmc::BLOCK_SIZE) as u32;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        // `MmcWriter::write` returns once the card has programmed the blocks.
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.blocks
    }

    fn now(&self) -> u32 {
        (self.clock)()
    }
}
