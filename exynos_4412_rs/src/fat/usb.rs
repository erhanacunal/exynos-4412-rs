//! A USB flash drive ([`Disk`]) as the [`BlockDevice`] of [`embedded_sdmmc`].
//!
//! Unlike [`PartitionDevice`](super::PartitionDevice), the whole drive is shown as it is: the
//! crate reads the drive's own master boot record, and [`VolumeIdx(0)`](embedded_sdmmc::VolumeIdx)
//! is its first partition. That partition must be FAT16 or FAT32 (types `0x06`, `0x0B`,
//! `0x0C`, `0x0E`). A GPT drive has a protective record of type `0xEE`, and a drive with a file
//! system from block 0 on ("superfloppy") has none: the crate opens neither.
//!
//! `embedded_sdmmc` reads one block at a time. Every drive read fills a whole read-ahead
//! window instead, from the block asked for on, so that a file is read with a few large
//! commands; the FAT and the file's data are read in turns, so each keeps a window of its
//! own. Writes go straight to the drive and update the windows: nothing is held back, and
//! [`UsbDevice::flush`] only asks the drive to empty its own cache.
//!
//! ```ignore
//! let device = UsbDevice::new(&mut disk, windows)?;
//! let volumes = VolumeManager::new(device, FixedTime::default());
//! let volume = volumes.open_volume(VolumeIdx(0))?;
//! let root = volume.open_root_dir()?;
//! ```

use core::cell::RefCell;
use core::fmt;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

use crate::mmu;
use crate::usb::host::msc::{self, Disk};

const BLOCK_SIZE: usize = 512;
/// Read-ahead windows.
pub const WINDOWS: usize = 4;
/// The smallest buffer [`UsbDevice::new`] takes: one block per window.
pub const MIN_BUFFER: usize = WINDOWS * BLOCK_SIZE;

/// Why the device failed or refused something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The drive's.
    Disk(msc::Error),
    /// A block past the end of the drive.
    OutOfRange,
    /// A buffer smaller than [`MIN_BUFFER`], or not on a cache line.
    BadBuffer,
}

impl From<msc::Error> for Error {
    fn from(error: msc::Error) -> Self {
        Self::Disk(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x?}", self)
    }
}

impl core::error::Error for Error {}

/// What a [`UsbDevice`] has done since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Read commands sent to the drive.
    pub disk_reads: u32,
    /// Blocks they read.
    pub blocks_read: u32,
    /// Blocks given from the windows without a command.
    pub window_reads: u32,
    /// Write commands sent to the drive.
    pub disk_writes: u32,
    /// Blocks they wrote.
    pub blocks_written: u32,
}

struct Inner<'a, 'd> {
    disk: &'a mut Disk<'d>,
    /// [`WINDOWS`] windows of `window_blocks` blocks each, on a cache line, so that drive reads
    /// go straight to the controller's DMA.
    buffer: &'a mut [u8],
    window_blocks: usize,
    /// The first block of each window, and how many it holds; 0 for an empty one.
    start: [u32; WINDOWS],
    len: [usize; WINDOWS],
    /// When each window was last used, from `clock`.
    used: [u32; WINDOWS],
    clock: u32,
    stats: Stats,
}

impl Inner<'_, '_> {
    fn window_of(&self, block: u32) -> Option<(usize, usize)> {
        (0..WINDOWS).find_map(|window| {
            let index = block.checked_sub(self.start[window])? as usize;
            (index < self.len[window]).then_some((window, index))
        })
    }

    fn window_block(&mut self, window: usize, index: usize) -> &mut [u8] {
        &mut self.buffer[(window * self.window_blocks + index) * BLOCK_SIZE..][..BLOCK_SIZE]
    }

    /// Fills the least recently used window from the drive, from `block` on.
    fn refill(&mut self, block: u32) -> Result<usize, Error> {
        let window = (0..WINDOWS).min_by_key(|window| self.used[*window]).unwrap_or(0);
        let blocks = self.window_blocks.min((self.disk.blocks() - block) as usize);
        // Empty while the read is under way, so that a failed one leaves nothing stale.
        self.len[window] = 0;
        let bytes = &mut self.buffer[window * self.window_blocks * BLOCK_SIZE..][..blocks * BLOCK_SIZE];
        self.disk.read(block, bytes)?;
        self.stats.disk_reads += 1;
        self.stats.blocks_read += blocks as u32;
        self.start[window] = block;
        self.len[window] = blocks;
        Ok(window)
    }

    fn read(&mut self, block: u32, blocks: &mut [Block]) -> Result<(), Error> {
        for (offset, target) in blocks.iter_mut().enumerate() {
            let lba = block + offset as u32;
            let (window, index) = match self.window_of(lba) {
                Some(found) => {
                    self.stats.window_reads += 1;
                    found
                }
                None => (self.refill(lba)?, 0),
            };
            self.clock = self.clock.wrapping_add(1);
            self.used[window] = self.clock;
            target.contents.copy_from_slice(self.window_block(window, index));
        }
        Ok(())
    }

    fn write(&mut self, block: u32, blocks: &[Block]) -> Result<(), Error> {
        // One command for all of them, from the first window's room: `Block` need not start
        // on a cache line, so it does not go to the DMA itself.
        let room = self.window_blocks;
        for (chunk_index, chunk) in blocks.chunks(room).enumerate() {
            let at = block + (chunk_index * room) as u32;
            // The window used least recently is the bounce buffer, and is left empty.
            let window = (0..WINDOWS).min_by_key(|window| self.used[*window]).unwrap_or(0);
            self.len[window] = 0;
            let bytes = &mut self.buffer[window * room * BLOCK_SIZE..][..chunk.len() * BLOCK_SIZE];
            for (target, source) in bytes.as_chunks_mut::<BLOCK_SIZE>().0.iter_mut().zip(chunk) {
                target.copy_from_slice(&source.contents);
            }
            self.disk.write(at, bytes)?;
            self.stats.disk_writes += 1;
            self.stats.blocks_written += chunk.len() as u32;
            // Windows can overlap, a refill starting just before another: every copy is kept new.
            for (offset, source) in chunk.iter().enumerate() {
                let lba = at + offset as u32;
                for other in 0..WINDOWS {
                    if let Some(index) = lba.checked_sub(self.start[other])
                        && (index as usize) < self.len[other]
                    {
                        self.window_block(other, index as usize).copy_from_slice(&source.contents);
                    }
                }
            }
        }
        Ok(())
    }
}

/// An opened [`Disk`], whole, for [`embedded_sdmmc::VolumeManager`]. See the module.
pub struct UsbDevice<'a, 'd> {
    inner: RefCell<Inner<'a, 'd>>,
}

impl<'a, 'd> UsbDevice<'a, 'd> {
    /// The drive, with `buffer` for the read-ahead windows: [`WINDOWS`] equal parts of whole
    /// blocks, which is what one drive read gets. It must start on a cache line
    /// ([`mmu::DCACHE_LINE`]) and hold at least [`MIN_BUFFER`] bytes; 256 KiB gives 64 KiB
    /// reads.
    pub fn new(disk: &'a mut Disk<'d>, buffer: &'a mut [u8]) -> Result<Self, Error> {
        if buffer.len() < MIN_BUFFER || !(buffer.as_ptr() as usize).is_multiple_of(mmu::DCACHE_LINE) {
            return Err(Error::BadBuffer);
        }
        let window_blocks = buffer.len() / WINDOWS / BLOCK_SIZE;
        Ok(Self {
            inner: RefCell::new(Inner {
                disk,
                buffer,
                window_blocks,
                start: [0; WINDOWS],
                len: [0; WINDOWS],
                used: [0; WINDOWS],
                clock: 0,
                stats: Stats::default(),
            }),
        })
    }

    /// What it has done so far.
    pub fn stats(&self) -> Stats {
        self.inner.borrow().stats
    }

    /// Forgets what the windows hold, so that the next reads come from the drive.
    pub fn invalidate(&self) {
        self.inner.borrow_mut().len = [0; WINDOWS];
    }

    /// Asks the drive to write what it holds in its cache: writes reach the drive at once.
    pub fn flush(&self) -> Result<(), Error> {
        Ok(self.inner.borrow_mut().disk.flush()?)
    }

    /// Whether the drive is still on the port.
    pub fn connected(&self) -> bool {
        self.inner.borrow().disk.connected()
    }
}

impl BlockDevice for UsbDevice<'_, '_> {
    type Error = Error;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Error> {
        let mut inner = self.inner.borrow_mut();
        if u64::from(start.0) + blocks.len() as u64 > u64::from(inner.disk.blocks()) {
            return Err(Error::OutOfRange);
        }
        inner.read(start.0, blocks)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Error> {
        let mut inner = self.inner.borrow_mut();
        if u64::from(start.0) + blocks.len() as u64 > u64::from(inner.disk.blocks()) {
            return Err(Error::OutOfRange);
        }
        inner.write(start.0, blocks)
    }

    fn num_blocks(&self) -> Result<BlockCount, Error> {
        Ok(BlockCount(self.inner.borrow().disk.blocks()))
    }
}
