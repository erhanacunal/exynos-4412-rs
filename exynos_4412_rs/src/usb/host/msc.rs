//! A mass storage device on the host side: a USB flash drive, say. Bulk-Only Transport
//! (Universal Serial Bus Mass Storage Class, Bulk-Only Transport 1.0) with SCSI commands
//! (SPC-2, SBC-2) to logical unit 0: INQUIRY, TEST UNIT READY, REQUEST SENSE, READ CAPACITY
//! (10), READ (10), WRITE (10), SYNCHRONIZE CACHE (10).
//!
//! A failed command reads the sense data; a transport that went wrong gets the reset recovery
//! of 5.3.4: the class reset, then both halts cleared.

use crate::drivers::ehci::{self, Ehci, Pipe};
use crate::drivers::timer::Mct;
use crate::usb::host::{Error as HostError, MassStorage};
use crate::usb::msc::Sense;
use crate::usb::{Setup, request};
use crate::{sleep, soc};

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;
const REQUEST_RESET: u8 = 0xFF;
const REQUEST_GET_MAX_LUN: u8 = 0xFE;

const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;

/// Commands and data: generous, a slow drive's first write can take seconds.
const DATA_TIMEOUT_MS: u64 = 10_000;
/// How long a drive may say it is not ready yet, after it is plugged in.
const READY_TIMEOUT_MS: u64 = 10_000;
/// The most READ (10) and WRITE (10) move at a time: 65535 blocks, kept to 1 MiB.
const MAX_BLOCKS: u32 = 2048;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Enumeration's, or the control pipe's.
    Host(HostError),
    /// The controller's, on a bulk pipe.
    Ehci(ehci::Error),
    /// The command failed: its operation code and the sense data.
    Failed(u8, Sense),
    /// The device lost track of the command (phase error), or sent a status that is not one.
    Transport,
    /// Blocks are not 512 bytes, or the device has none.
    BlockSize(u32),
    /// Blocks out of the device, or a buffer that is not whole blocks.
    OutOfRange,
}

impl From<ehci::Error> for Error {
    fn from(error: ehci::Error) -> Self {
        Self::Ehci(error)
    }
}

impl From<HostError> for Error {
    fn from(error: HostError) -> Self {
        Self::Host(error)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    In,
    Out,
    None,
}

/// A mass storage device, opened: ready, its size known.
pub struct Disk<'a> {
    ehci: &'a mut Ehci,
    mct: Mct,
    interface: u8,
    tag: u32,
    luns: u8,
    blocks: u32,
    block_size: u32,
    inquiry: [u8; 36],
    no_cache: bool,
}

impl<'a> Disk<'a> {
    /// Opens the mass storage interface `storage` of the enumerated device: the bulk pipes,
    /// INQUIRY, TEST UNIT READY until it is ready, READ CAPACITY.
    pub fn open(ehci: &'a mut Ehci, storage: MassStorage, mct: Mct) -> Result<Self, Error> {
        ehci.set_bulk(
            crate::usb::host::ADDRESS,
            (storage.input.address & 0x0F, storage.input.packet),
            (storage.output.address & 0x0F, storage.output.packet),
        );
        let mut disk = Self {
            ehci,
            mct,
            interface: storage.interface,
            tag: 0,
            luns: 1,
            blocks: 0,
            block_size: 0,
            inquiry: [0; 36],
            no_cache: false,
        };
        // 3.2: a device with one unit may stall this.
        let get_max_lun = Setup {
            request_type: 0xA1,
            request: REQUEST_GET_MAX_LUN,
            value: 0,
            index: u16::from(disk.interface),
            length: 1,
        };
        let mut lun = [0u8; 1];
        if disk.ehci.control(get_max_lun, &mut lun) == Ok(1) {
            disk.luns = lun[0].saturating_add(1);
        }

        let mut inquiry = [0u8; 36];
        disk.command(&[INQUIRY, 0, 0, 0, 36, 0], Direction::In, &mut inquiry)?;
        disk.inquiry = inquiry;

        let begin = mct.count();
        loop {
            match disk.command(&[TEST_UNIT_READY, 0, 0, 0, 0, 0], Direction::None, &mut []) {
                Ok(_) => break,
                // Unit attention after the reset, or not ready while it starts.
                Err(Error::Failed(_, Sense(key, ..))) if key == 0x06 || key == 0x02 => {
                    if mct.count() - begin > READY_TIMEOUT_MS * u64::from(soc::MCT_CLOCK_HZ / 1000) {
                        return Err(Error::Failed(TEST_UNIT_READY, Sense(key, 0, 0)));
                    }
                    sleep::for_us(mct, 100_000);
                }
                Err(error) => return Err(error),
            }
        }

        let mut capacity = [0u8; 8];
        disk.command(&[READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0], Direction::In, &mut capacity)?;
        let last = u32::from_be_bytes([capacity[0], capacity[1], capacity[2], capacity[3]]);
        disk.block_size = u32::from_be_bytes([capacity[4], capacity[5], capacity[6], capacity[7]]);
        disk.blocks = last.wrapping_add(1);
        Ok(disk)
    }

    /// INQUIRY's vendor, product and revision fields, spaces and all.
    pub fn identity(&self) -> (&[u8], &[u8], &[u8]) {
        (&self.inquiry[8..16], &self.inquiry[16..32], &self.inquiry[32..36])
    }

    /// Whether the medium is removable, as INQUIRY says.
    pub const fn removable(&self) -> bool {
        self.inquiry[1] & 0x80 != 0
    }

    /// Logical units, of which only 0 is used.
    pub const fn luns(&self) -> u8 {
        self.luns
    }

    /// Whether the device is still plugged in.
    pub fn connected(&self) -> bool {
        self.ehci.port_status(ehci::PORT).connected()
    }

    /// Blocks on the medium.
    pub const fn blocks(&self) -> u32 {
        self.blocks
    }

    /// Bytes per block.
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    fn check(&self, lba: u32, len: usize) -> Result<u32, Error> {
        if self.block_size != 512 {
            return Err(Error::BlockSize(self.block_size));
        }
        if len == 0 || !len.is_multiple_of(512) {
            return Err(Error::OutOfRange);
        }
        let count = (len / 512) as u64;
        if u64::from(lba) + count > u64::from(self.blocks) {
            return Err(Error::OutOfRange);
        }
        Ok(count as u32)
    }

    /// Reads whole 512-byte blocks into `buffer`, from block `lba` on. A buffer on a cache line
    /// and of whole cache lines goes straight to the controller's DMA.
    pub fn read(&mut self, lba: u32, buffer: &mut [u8]) -> Result<(), Error> {
        let count = self.check(lba, buffer.len())?;
        let mut done = 0;
        while done < count {
            let n = (count - done).min(MAX_BLOCKS);
            let at = lba + done;
            let range = done as usize * 512..(done + n) as usize * 512;
            let [a, b, c, d] = at.to_be_bytes();
            let [e, f] = (n as u16).to_be_bytes();
            self.command(&[READ_10, 0, a, b, c, d, 0, e, f, 0], Direction::In, &mut buffer[range])?;
            done += n;
        }
        Ok(())
    }

    /// Writes whole 512-byte blocks from `data`, from block `lba` on.
    pub fn write(&mut self, lba: u32, data: &[u8]) -> Result<(), Error> {
        let count = self.check(lba, data.len())?;
        let mut done = 0;
        while done < count {
            let n = (count - done).min(MAX_BLOCKS);
            let at = lba + done;
            let range = done as usize * 512..(done + n) as usize * 512;
            let [a, b, c, d] = at.to_be_bytes();
            let [e, f] = (n as u16).to_be_bytes();
            self.command_out(&[WRITE_10, 0, a, b, c, d, 0, e, f, 0], &data[range])?;
            done += n;
        }
        Ok(())
    }

    /// Asks the device to write what it holds in its cache. A device that refuses the command
    /// with ILLEGAL REQUEST has none to write, and is not asked again: flash drives answer
    /// INVALID COMMAND OPERATION CODE or INVALID FIELD IN PARAMETER LIST, and report no write
    /// cache, so Linux's `sd` does not send it to them. Each write has completed by its status.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.no_cache {
            return Ok(());
        }
        match self.command(&[SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0], Direction::None, &mut []) {
            Ok(_) => Ok(()),
            Err(Error::Failed(_, Sense(0x05, ..))) => {
                self.no_cache = true;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Whether the device refused SYNCHRONIZE CACHE, so [`flush`](Self::flush) sends nothing.
    pub const fn no_cache(&self) -> bool {
        self.no_cache
    }

    fn command_out(&mut self, block: &[u8], data: &[u8]) -> Result<u32, Error> {
        self.send_command(block, Direction::Out, data.len())?;
        let stalled = match self.ehci.bulk_out(data, DATA_TIMEOUT_MS) {
            Ok(()) => false,
            Err(ehci::Error::Stall) => true,
            Err(error) => return Err(self.recover(error)),
        };
        self.finish(block[0], stalled, Pipe::BulkOut)
    }

    /// Sends a command block, moves its data, takes its status: the residue. A failed command
    /// reads the sense data into the error.
    fn command(&mut self, block: &[u8], direction: Direction, data: &mut [u8]) -> Result<u32, Error> {
        self.send_command(block, direction, data.len())?;
        let stalled = match direction {
            Direction::In => match self.ehci.bulk_in(data, DATA_TIMEOUT_MS) {
                Ok(_) => false,
                Err(ehci::Error::Stall) => true,
                Err(error) => return Err(self.recover(error)),
            },
            Direction::Out => match self.ehci.bulk_out(data, DATA_TIMEOUT_MS) {
                Ok(()) => false,
                Err(ehci::Error::Stall) => true,
                Err(error) => return Err(self.recover(error)),
            },
            Direction::None => false,
        };
        let pipe = if direction == Direction::Out { Pipe::BulkOut } else { Pipe::BulkIn };
        self.finish(block[0], stalled, pipe)
    }

    fn send_command(&mut self, block: &[u8], direction: Direction, len: usize) -> Result<(), Error> {
        self.tag = self.tag.wrapping_add(1);
        let mut cbw = [0u8; 32];
        cbw[..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        cbw[4..8].copy_from_slice(&self.tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&(len as u32).to_le_bytes());
        cbw[12] = if direction == Direction::In { 0x80 } else { 0 };
        cbw[13] = 0;
        cbw[14] = block.len() as u8;
        cbw[15..15 + block.len()].copy_from_slice(block);
        match self.ehci.bulk_out(&cbw[..CBW_LEN], DATA_TIMEOUT_MS) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.recover(error)),
        }
    }

    /// The status stage, after clearing a halt the data stage met (6.7.2, 6.7.3).
    fn finish(&mut self, opcode: u8, stalled: bool, pipe: Pipe) -> Result<u32, Error> {
        if stalled {
            self.clear_halt(pipe)?;
        }
        let mut csw = [0u8; CSW_LEN];
        let mut read = self.ehci.bulk_in(&mut csw, DATA_TIMEOUT_MS);
        if read == Err(ehci::Error::Stall) {
            // 6.7.2: one more try after clearing the halt.
            self.clear_halt(Pipe::BulkIn)?;
            read = self.ehci.bulk_in(&mut csw, DATA_TIMEOUT_MS);
        }
        let read = match read {
            Ok(read) => read,
            Err(error) => return Err(self.recover(error)),
        };
        let signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
        let tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
        if read != CSW_LEN || signature != CSW_SIGNATURE || tag != self.tag || csw[12] > 1 {
            self.reset_recovery();
            return Err(Error::Transport);
        }
        let residue = u32::from_le_bytes([csw[8], csw[9], csw[10], csw[11]]);
        if csw[12] == 1 {
            let sense = if opcode == REQUEST_SENSE { Sense(0, 0, 0) } else { self.sense() };
            return Err(Error::Failed(opcode, sense));
        }
        Ok(residue)
    }

    fn sense(&mut self) -> Sense {
        let mut data = [0u8; 18];
        match self.command(&[REQUEST_SENSE, 0, 0, 0, 18, 0], Direction::In, &mut data) {
            Ok(_) => Sense(data[2] & 0x0F, data[12], data[13]),
            Err(_) => Sense(0xFF, 0xFF, 0xFF),
        }
    }

    /// `CLEAR_FEATURE` (`ENDPOINT_HALT`) to the device's endpoint, and DATA0 here.
    fn clear_halt(&mut self, pipe: Pipe) -> Result<(), Error> {
        let address = self.endpoint_address(pipe);
        let clear = Setup {
            request_type: 0x02,
            request: request::CLEAR_FEATURE,
            value: request::FEATURE_ENDPOINT_HALT,
            index: u16::from(address),
            length: 0,
        };
        self.ehci.control(clear, &mut []).map_err(|error| Error::Host(HostError::Ehci(error)))?;
        self.ehci.reset_toggle(pipe);
        Ok(())
    }

    fn endpoint_address(&self, pipe: Pipe) -> u8 {
        let (input, output) = self.ehci.bulk_endpoints();
        if pipe == Pipe::BulkIn { 0x80 | input } else { output }
    }

    /// 5.3.4: the class reset, then the halts of both endpoints cleared. Returns `error`.
    fn recover(&mut self, error: ehci::Error) -> Error {
        self.reset_recovery();
        Error::Ehci(error)
    }

    fn reset_recovery(&mut self) {
        let reset = Setup {
            request_type: 0x21,
            request: REQUEST_RESET,
            value: 0,
            index: u16::from(self.interface),
            length: 0,
        };
        let _ = self.ehci.control(reset, &mut []);
        let _ = self.clear_halt(Pipe::BulkIn);
        let _ = self.clear_halt(Pipe::BulkOut);
        sleep::for_us(self.mct, 1000);
    }
}

/// The disk for anything that takes a [`crate::usb::msc::BlockDevice`].
impl crate::usb::msc::BlockDevice for Disk<'_> {
    type Error = Error;

    fn blocks(&self) -> u32 {
        self.blocks
    }

    fn read(&mut self, lba: u32, buffer: &mut [u8]) -> Result<(), Error> {
        Disk::read(self, lba, buffer)
    }

    fn write(&mut self, lba: u32, data: &[u8]) -> Result<(), Error> {
        Disk::write(self, lba, data)
    }
}
