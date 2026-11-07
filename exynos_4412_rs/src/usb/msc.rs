//! A USB mass storage device: the Bulk-Only Transport (USB Mass Storage Class, Bulk-Only
//! Transport 1.0) carrying SCSI commands (SPC-2, SBC-2) for one logical unit, over a
//! [`BlockDevice`] of 512-byte blocks. It runs on the OTG controller ([`Otg`]): feed it every
//! [`Event`] that [`Otg::poll`] returns.
//!
//! The host sees one removable disk. Commands: INQUIRY, REQUEST SENSE, TEST UNIT READY, READ
//! CAPACITY (10), READ FORMAT CAPACITIES, MODE SENSE (6) and (10), PREVENT ALLOW MEDIUM
//! REMOVAL, START STOP UNIT (an eject takes the medium away until the next start, or a bus
//! reset), SYNCHRONIZE CACHE (10), VERIFY (10), READ and WRITE (10) and (12). Others fail with
//! ILLEGAL REQUEST, INVALID COMMAND OPERATION CODE.
//!
//! Data goes through one buffer, in turns: the block device fills it, the host takes it, and
//! the other way for writes.

use core::fmt::Debug;

use crate::drivers::otg::{Event, Otg};
use crate::usb::{DIR_IN, Descriptors, Kind, Recipient, Speed, descriptor};

/// Bytes per block.
pub const BLOCK_SIZE: usize = 512;

/// The endpoints: bulk IN 1 and bulk OUT 1.
pub const EP_IN: u8 = DIR_IN | 1;
/// See [`EP_IN`].
pub const EP_OUT: u8 = 1;
const EP: u8 = 1;

/// pid.codes' test vendor and product (`1209:0001`), for devices that are not published.
pub const VENDOR_ID: u16 = 0x1209;
/// See [`VENDOR_ID`].
pub const PRODUCT_ID: u16 = 0x0001;

/// Strings 1, 2 and 3 of the descriptors: manufacturer, product, serial number. The Bulk-Only
/// Transport (4.1.1) wants a serial number of at least 12 hexadecimal digits.
pub type Strings = [&'static str; 3];

const fn device_descriptor() -> [u8; 18] {
    let vid = VENDOR_ID.to_le_bytes();
    let pid = PRODUCT_ID.to_le_bytes();
    [
        18,
        descriptor::DEVICE,
        0x00,
        0x02, // USB 2.0
        0,
        0,
        0,  // class in the interface
        64, // endpoint 0
        vid[0],
        vid[1],
        pid[0],
        pid[1],
        0x00,
        0x01, // device release 1.00
        1,
        2,
        3, // strings
        1, // configurations
    ]
}

const fn configuration(packet: u16) -> [u8; 32] {
    let mps = packet.to_le_bytes();
    [
        // Configuration: 32 bytes in all, one interface, value 1, bus powered, 100 mA.
        9,
        descriptor::CONFIGURATION,
        32,
        0,
        1,
        1,
        0,
        0x80,
        50,
        // Interface 0: mass storage (8), SCSI transparent (6), Bulk-Only (0x50), two endpoints.
        9,
        descriptor::INTERFACE,
        0,
        0,
        2,
        0x08,
        0x06,
        0x50,
        0,
        // Bulk IN.
        7,
        descriptor::ENDPOINT,
        EP_IN,
        2,
        mps[0],
        mps[1],
        0,
        // Bulk OUT.
        7,
        descriptor::ENDPOINT,
        EP_OUT,
        2,
        mps[0],
        mps[1],
        0,
    ]
}

static DEVICE: [u8; 18] = device_descriptor();
static HIGH_SPEED: [u8; 32] = configuration(512);
static FULL_SPEED: [u8; 32] = configuration(64);

/// The descriptors to give [`Otg::new`].
pub const fn descriptors(strings: &'static Strings) -> Descriptors<'static> {
    Descriptors { device: &DEVICE, high_speed: &HIGH_SPEED, full_speed: &FULL_SPEED, strings }
}

/// Storage in 512-byte blocks.
pub trait BlockDevice {
    /// What a failed read or write reports.
    type Error: Debug;

    /// How many blocks there are.
    fn blocks(&self) -> u32;

    /// Whether writes are refused.
    fn read_only(&self) -> bool {
        false
    }

    /// Reads whole blocks into `buffer`, from block `lba` on. `buffer` starts on a cache line.
    fn read(&mut self, lba: u32, buffer: &mut [u8]) -> Result<(), Self::Error>;

    /// Writes whole blocks from `data`, from block `lba` on, and returns once they are stored.
    /// `data` starts on a cache line.
    fn write(&mut self, lba: u32, data: &[u8]) -> Result<(), Self::Error>;
}

// Bulk-Only Transport, 5.1 and 5.2.
const CBW_SIGNATURE: u32 = 0x4342_5355;
const CBW_LEN: usize = 31;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const CSW_LEN: usize = 13;
// Bulk-Only Transport, 3.1 and 3.2.
const REQUEST_RESET: u8 = 0xFF;
const REQUEST_GET_MAX_LUN: u8 = 0xFE;

/// `bCSWStatus`.
const STATUS_PASSED: u8 = 0;
const STATUS_FAILED: u8 = 1;
const STATUS_PHASE_ERROR: u8 = 2;

// SCSI operation codes: SPC-2 and SBC-2.
const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const MODE_SENSE_6: u8 = 0x1A;
const START_STOP_UNIT: u8 = 0x1B;
const PREVENT_ALLOW: u8 = 0x1E;
const READ_FORMAT_CAPACITIES: u8 = 0x23;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;
const VERIFY_10: u8 = 0x2F;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;
const MODE_SENSE_10: u8 = 0x5A;
const READ_12: u8 = 0xA8;
const WRITE_12: u8 = 0xAA;

/// Sense key, additional sense code and qualifier: SPC-2, tables 107 and 108.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense(pub u8, pub u8, pub u8);

impl Sense {
    const NONE: Self = Self(0, 0, 0);
    const MEDIUM_NOT_PRESENT: Self = Self(0x02, 0x3A, 0x00);
    const READ_ERROR: Self = Self(0x03, 0x11, 0x00);
    const WRITE_ERROR: Self = Self(0x03, 0x0C, 0x00);
    const INVALID_COMMAND: Self = Self(0x05, 0x20, 0x00);
    const LBA_OUT_OF_RANGE: Self = Self(0x05, 0x21, 0x00);
    const INVALID_FIELD: Self = Self(0x05, 0x24, 0x00);
    const WRITE_PROTECTED: Self = Self(0x07, 0x27, 0x00);
}

/// Counts, for a report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Commands taken.
    pub commands: u32,
    /// Commands that failed.
    pub failed: u32,
    /// Blocks read and sent.
    pub blocks_read: u64,
    /// Blocks received and written.
    pub blocks_written: u64,
    /// Command blocks refused as invalid, each followed by a reset by the host.
    pub invalid: u32,
    /// Bulk-Only resets.
    pub resets: u32,
    /// The last failure's operation code and sense.
    pub last_failure: Option<(u8, Sense)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Not configured.
    Off,
    /// Waiting for a command block.
    Command,
    /// Sending a reply that is in the buffer.
    Reply,
    /// Sending blocks, one buffer at a time.
    ReadBlocks,
    /// Receiving blocks, one buffer at a time.
    WriteBlocks,
    /// An endpoint is halted until the host clears it; the status comes next.
    Halted(u8),
    /// Sending the status.
    Status,
    /// An invalid command block: both endpoints halted until a Bulk-Only reset.
    NeedReset,
}

/// The command at work.
#[derive(Clone, Copy, Debug, Default)]
struct Command {
    tag: u32,
    /// What the host means to move, and which way.
    host_len: u32,
    host_in: bool,
    /// What has moved.
    moved: u32,
    opcode: u8,
    status: u8,
    lba: u32,
    /// Blocks left to move, and in the buffer now.
    left: u32,
    chunk: u32,
}

/// What a command does after it is decoded.
enum Action {
    /// Done, with this status and nothing to move.
    None,
    /// Send the first bytes of the buffer.
    Reply(usize),
    /// Send blocks.
    Read(u32, u32),
    /// Receive blocks.
    Write(u32, u32),
}

/// The mass storage device over `D`.
pub struct MassStorage<D: BlockDevice> {
    device: D,
    buffer: &'static mut [u8],
    state: State,
    command: Command,
    sense: Sense,
    ejected: bool,
    packet: u16,
    identity: [u8; 28],
    stats: Stats,
}

impl<D: BlockDevice> MassStorage<D> {
    /// A device over `device`, moving data through `buffer`: a whole number of blocks, at least
    /// one, starting on a cache line. Bigger moves more at a time; 64 KiB is plenty. `vendor`
    /// (8 bytes) and `product` (16 bytes) are what INQUIRY says, padded with spaces.
    ///
    /// # Panics
    ///
    /// If `buffer` is not as above.
    pub fn new(device: D, buffer: &'static mut [u8], vendor: &str, product: &str) -> Self {
        assert!(
            !buffer.is_empty()
                && buffer.len().is_multiple_of(BLOCK_SIZE)
                && (buffer.as_ptr() as usize).is_multiple_of(crate::mmu::DCACHE_LINE),
            "mass storage buffer: whole blocks on a cache line"
        );
        let mut identity = [b' '; 28];
        for (slot, byte) in identity[..8].iter_mut().zip(vendor.bytes()) {
            *slot = byte;
        }
        for (slot, byte) in identity[8..24].iter_mut().zip(product.bytes()) {
            *slot = byte;
        }
        identity[24..].copy_from_slice(b"0100");
        Self {
            device,
            buffer,
            state: State::Off,
            command: Command::default(),
            sense: Sense::NONE,
            ejected: false,
            packet: 512,
            identity,
            stats: Stats::default(),
        }
    }

    /// The block device.
    pub fn device(&mut self) -> &mut D {
        &mut self.device
    }

    /// The counts so far.
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// Whether the host ejected the medium.
    pub const fn ejected(&self) -> bool {
        self.ejected
    }

    /// Whether the host is in the middle of a command: not a good time to unplug.
    pub fn busy(&self) -> bool {
        !matches!(self.state, State::Off | State::Command)
    }

    /// Takes one event from [`Otg::poll`].
    pub fn handle(&mut self, otg: &mut Otg, event: Event) {
        match event {
            Event::Reset | Event::Disconnected => {
                self.state = State::Off;
                self.ejected = false;
            }
            Event::Enumerated(speed) => {
                self.packet = if speed == Speed::High { 512 } else { 64 };
            }
            Event::Configured(0) => self.state = State::Off,
            Event::Configured(_) => {
                let enabled = otg.enable_bulk(EP_IN, self.packet).and_then(|()| otg.enable_bulk(EP_OUT, self.packet));
                if enabled.is_ok() {
                    self.receive_command(otg);
                } else {
                    self.state = State::Off;
                }
            }
            Event::ClassRequest(setup) => {
                let ours = setup.kind() == Kind::Class && setup.recipient() == Recipient::Interface && setup.index == 0;
                match setup.request {
                    REQUEST_GET_MAX_LUN if ours && setup.is_in() => otg.control_in(&[0]),
                    REQUEST_RESET if ours && !setup.is_in() && setup.length == 0 => {
                        otg.control_ack();
                        self.stats.resets += 1;
                        self.bot_reset(otg);
                    }
                    _ => otg.control_stall(),
                }
            }
            Event::HaltCleared(address) => {
                if self.state == State::Halted(address) {
                    self.send_status(otg);
                }
            }
            Event::OutDone(EP, received) => match self.state {
                State::Command => self.take_command(otg, received),
                State::WriteBlocks => self.received_blocks(otg, received),
                _ => {}
            },
            Event::InDone(EP) => match self.state {
                State::Reply => self.data_sent(otg),
                State::ReadBlocks => {
                    self.command.moved += self.command.chunk * BLOCK_SIZE as u32;
                    self.command.lba += self.command.chunk;
                    self.command.left -= self.command.chunk;
                    if self.command.left == 0 {
                        self.data_sent(otg);
                    } else {
                        self.send_blocks(otg);
                    }
                }
                State::Status => self.receive_command(otg),
                _ => {}
            },
            _ => {}
        }
    }

    fn bot_reset(&mut self, otg: &mut Otg) {
        // Bulk-Only Transport 3.1: ready for the next command; the host clears the halts.
        otg.abort(EP_IN);
        otg.abort(EP_OUT);
        let _ = otg.set_halt(EP_IN, false, false);
        let _ = otg.set_halt(EP_OUT, false, false);
        if self.state != State::Off {
            self.receive_command(otg);
        }
    }

    fn receive_command(&mut self, otg: &mut Otg) {
        self.state = State::Command;
        let packet = usize::from(self.packet).max(BLOCK_SIZE).min(self.buffer.len());
        // SAFETY: the buffer is ours for good, and left alone until the transfer ends or the
        // controller stops it (reset, unplug, configuration), each of which comes through here.
        if unsafe { otg.start_out(EP, &mut self.buffer[..packet]) }.is_err() {
            self.state = State::Off;
        }
    }

    fn take_command(&mut self, otg: &mut Otg, received: usize) {
        let cbw = &self.buffer[..CBW_LEN];
        let signature = u32::from_le_bytes([cbw[0], cbw[1], cbw[2], cbw[3]]);
        let length = cbw[14] & 0x1F;
        if received != CBW_LEN || signature != CBW_SIGNATURE || cbw[13] != 0 || length == 0 || length > 16 {
            // 6.6.1: not valid, or not meaningful: halt both until a reset recovery.
            self.stats.invalid += 1;
            let _ = otg.set_halt(EP_IN, true, true);
            let _ = otg.set_halt(EP_OUT, true, true);
            self.state = State::NeedReset;
            return;
        }
        let mut block = [0u8; 16];
        block.copy_from_slice(&cbw[15..31]);
        self.command = Command {
            tag: u32::from_le_bytes([cbw[4], cbw[5], cbw[6], cbw[7]]),
            host_len: u32::from_le_bytes([cbw[8], cbw[9], cbw[10], cbw[11]]),
            host_in: cbw[12] & 0x80 != 0,
            opcode: block[0],
            ..Command::default()
        };
        self.stats.commands += 1;
        let action = self.decode(&block);
        self.start(otg, action);
    }

    fn fail(&mut self, sense: Sense) -> Action {
        self.sense = sense;
        self.command.status = STATUS_FAILED;
        self.stats.failed += 1;
        self.stats.last_failure = Some((self.command.opcode, sense));
        Action::None
    }

    /// Puts `bytes` in the buffer and sends at most `allocation` of them.
    fn reply(&mut self, bytes: &[u8], allocation: usize) -> Action {
        let len = bytes.len().min(allocation);
        self.buffer[..len].copy_from_slice(&bytes[..len]);
        Action::Reply(len)
    }

    fn blocks_in_range(&mut self, lba: u32, count: u32) -> bool {
        u64::from(lba) + u64::from(count) <= u64::from(self.device.blocks())
    }

    fn decode(&mut self, block: &[u8; 16]) -> Action {
        let be16 = |at: usize| u16::from_be_bytes([block[at], block[at + 1]]);
        let be32 = |at: usize| u32::from_be_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
        let opcode = block[0];
        // The commands that work without a medium.
        match opcode {
            INQUIRY => {
                if block[1] & 1 != 0 {
                    return self.fail(Sense::INVALID_FIELD);
                }
                // SPC-2, 7.3.2: a direct access device, removable, SPC-2, 36 bytes.
                let mut data = [0u8; 36];
                data[1] = 0x80;
                data[2] = 0x04;
                data[3] = 0x02;
                data[4] = 31;
                data[8..36].copy_from_slice(&self.identity);
                return self.reply(&data, usize::from(be16(3)));
            }
            REQUEST_SENSE => {
                // SPC-2, 7.20.2: fixed format, current errors.
                let Sense(key, code, qualifier) = self.sense;
                let data = [0x70, 0, key, 0, 0, 0, 0, 10, 0, 0, 0, 0, code, qualifier, 0, 0, 0, 0];
                self.sense = Sense::NONE;
                return self.reply(&data, usize::from(block[4]));
            }
            START_STOP_UNIT => {
                // SBC-2, 5.19: LOEJ with START clear ejects, with START set loads.
                if block[4] & 2 != 0 {
                    self.ejected = block[4] & 1 == 0;
                }
                return Action::None;
            }
            PREVENT_ALLOW => return Action::None,
            _ => {}
        }
        if self.ejected {
            return match opcode {
                TEST_UNIT_READY | READ_CAPACITY_10 | READ_FORMAT_CAPACITIES | MODE_SENSE_6 | MODE_SENSE_10
                | READ_10 | READ_12 | WRITE_10 | WRITE_12 | VERIFY_10 | SYNCHRONIZE_CACHE_10 => {
                    self.fail(Sense::MEDIUM_NOT_PRESENT)
                }
                _ => self.fail(Sense::INVALID_COMMAND),
            };
        }
        let blocks = self.device.blocks();
        let protect = if self.device.read_only() { 0x80 } else { 0 };
        match opcode {
            TEST_UNIT_READY | SYNCHRONIZE_CACHE_10 | VERIFY_10 => Action::None,
            READ_CAPACITY_10 => {
                let mut data = [0u8; 8];
                data[..4].copy_from_slice(&blocks.saturating_sub(1).to_be_bytes());
                data[4..].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
                self.reply(&data, 8)
            }
            READ_FORMAT_CAPACITIES => {
                // UFI 4.10: a list of one, the current capacity, formatted media (2).
                let mut data = [0u8; 12];
                data[3] = 8;
                data[4..8].copy_from_slice(&blocks.to_be_bytes());
                data[8] = 2;
                data[9..12].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes()[1..]);
                self.reply(&data, usize::from(be16(7)))
            }
            // SPC-2, 8.3: the header alone, its write protect bit, no block descriptor.
            MODE_SENSE_6 => self.reply(&[3, 0, protect, 0], usize::from(block[4])),
            MODE_SENSE_10 => self.reply(&[0, 6, 0, protect, 0, 0, 0, 0], usize::from(be16(7))),
            READ_10 | READ_12 | WRITE_10 | WRITE_12 => {
                let lba = be32(2);
                let count = if matches!(opcode, READ_10 | WRITE_10) { u32::from(be16(7)) } else { be32(6) };
                if !self.blocks_in_range(lba, count) {
                    return self.fail(Sense::LBA_OUT_OF_RANGE);
                }
                if matches!(opcode, WRITE_10 | WRITE_12) {
                    if protect != 0 {
                        return self.fail(Sense::WRITE_PROTECTED);
                    }
                    Action::Write(lba, count)
                } else {
                    Action::Read(lba, count)
                }
            }
            _ => self.fail(Sense::INVALID_COMMAND),
        }
    }

    /// Runs the data stage the command wants against the one the host means, as the
    /// Bulk-Only Transport's thirteen cases (6.7) work out.
    fn start(&mut self, otg: &mut Otg, action: Action) {
        let host_len = self.command.host_len;
        match action {
            Action::None => self.end_without_data(otg),
            Action::Reply(len) => {
                if len == 0 && self.command.host_in {
                    return self.end_without_data(otg);
                }
                if host_len == 0 || !self.command.host_in {
                    // Cases 2 and 10: the host does not mean to take data.
                    self.command.status = STATUS_PHASE_ERROR;
                    return self.end_without_data(otg);
                }
                let len = len.min(host_len as usize);
                self.command.moved = len as u32;
                self.state = State::Reply;
                // SAFETY: as in `receive_command`.
                if unsafe { otg.start_in(EP, &self.buffer[..len]) }.is_err() {
                    self.state = State::Off;
                }
            }
            Action::Read(_, 0) | Action::Write(_, 0) => self.end_without_data(otg),
            Action::Read(lba, count) => {
                if !self.command.host_in || u64::from(count) * BLOCK_SIZE as u64 > u64::from(host_len) {
                    // Cases 7, 8 and 10.
                    self.command.status = STATUS_PHASE_ERROR;
                    return self.end_without_data(otg);
                }
                self.command.lba = lba;
                self.command.left = count;
                self.send_blocks(otg);
            }
            Action::Write(lba, count) => {
                if self.command.host_in || host_len == 0 || u64::from(count) * BLOCK_SIZE as u64 > u64::from(host_len) {
                    // Cases 3, 8 and 13.
                    self.command.status = STATUS_PHASE_ERROR;
                    return self.end_without_data(otg);
                }
                self.command.lba = lba;
                self.command.left = count;
                self.receive_blocks(otg);
            }
        }
    }

    /// No data from the device: if the host meant to move some, halt its endpoint first.
    fn end_without_data(&mut self, otg: &mut Otg) {
        if self.command.host_len == 0 {
            return self.send_status(otg);
        }
        let address = if self.command.host_in { EP_IN } else { EP_OUT };
        let _ = otg.set_halt(address, true, false);
        self.state = State::Halted(address);
    }

    fn send_blocks(&mut self, otg: &mut Otg) {
        let chunk = self.command.left.min((self.buffer.len() / BLOCK_SIZE) as u32);
        let bytes = chunk as usize * BLOCK_SIZE;
        if self.device.read(self.command.lba, &mut self.buffer[..bytes]).is_err() {
            self.fail(Sense::READ_ERROR);
            return self.data_sent(otg);
        }
        self.stats.blocks_read += u64::from(chunk);
        self.command.chunk = chunk;
        self.state = State::ReadBlocks;
        // SAFETY: as in `receive_command`.
        if unsafe { otg.start_in(EP, &self.buffer[..bytes]) }.is_err() {
            self.state = State::Off;
        }
    }

    /// The data to the host is over, whole or cut short: the status, after halting IN when the
    /// host still waits for more and the last packet was not short (6.7.2, case 5).
    fn data_sent(&mut self, otg: &mut Otg) {
        let moved = self.command.moved;
        if moved < self.command.host_len && moved.is_multiple_of(u32::from(self.packet)) {
            let _ = otg.set_halt(EP_IN, true, false);
            self.state = State::Halted(EP_IN);
        } else {
            self.send_status(otg);
        }
    }

    fn receive_blocks(&mut self, otg: &mut Otg) {
        let chunk = self.command.left.min((self.buffer.len() / BLOCK_SIZE) as u32);
        self.command.chunk = chunk;
        self.state = State::WriteBlocks;
        let bytes = chunk as usize * BLOCK_SIZE;
        // SAFETY: as in `receive_command`.
        if unsafe { otg.start_out(EP, &mut self.buffer[..bytes]) }.is_err() {
            self.state = State::Off;
        }
    }

    fn received_blocks(&mut self, otg: &mut Otg, received: usize) {
        self.command.moved += received as u32;
        let whole = (received / BLOCK_SIZE) as u32;
        if whole != 0 {
            let bytes = whole as usize * BLOCK_SIZE;
            if self.device.write(self.command.lba, &self.buffer[..bytes]).is_err() {
                self.fail(Sense::WRITE_ERROR);
                return self.data_received(otg);
            }
            self.stats.blocks_written += u64::from(whole);
        }
        if whole < self.command.chunk {
            // The host stopped early.
            self.command.status = STATUS_PHASE_ERROR;
            return self.data_received(otg);
        }
        self.command.lba += whole;
        self.command.left -= whole;
        if self.command.left == 0 {
            self.data_received(otg);
        } else {
            self.receive_blocks(otg);
        }
    }

    /// The data from the host is over: halt OUT if the host still means to send (6.7.3, case 9).
    fn data_received(&mut self, otg: &mut Otg) {
        if self.command.moved < self.command.host_len {
            let _ = otg.set_halt(EP_OUT, true, false);
            self.state = State::Halted(EP_OUT);
        } else {
            self.send_status(otg);
        }
    }

    fn send_status(&mut self, otg: &mut Otg) {
        let command = self.command;
        let residue = command.host_len.saturating_sub(command.moved);
        let csw = &mut self.buffer[..CSW_LEN];
        csw[..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        csw[4..8].copy_from_slice(&command.tag.to_le_bytes());
        csw[8..12].copy_from_slice(&residue.to_le_bytes());
        csw[12] = command.status;
        if command.status == STATUS_PASSED {
            self.sense = Sense::NONE;
        }
        self.state = State::Status;
        // SAFETY: as in `receive_command`.
        if unsafe { otg.start_in(EP, &self.buffer[..CSW_LEN]) }.is_err() {
            self.state = State::Off;
        }
    }
}
