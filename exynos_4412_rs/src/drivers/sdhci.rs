//! SD host controller, for the SDIO WiFi chip on `sdhci_3`.
//!
//! Register offsets and fields are verified against Linux 5.15.198, `drivers/mmc/host/sdhci.h`,
//! cited as `sdhci.h:<line>`, and the Samsung registers against `drivers/mmc/host/sdhci-s3c.c`,
//! cited as `sdhci-s3c.c:<line>`.
//!
//! # Clock
//!
//! `sdhci-s3c.c:764-771` marks this controller as having no usable divider of its own: the card
//! clock is `sclk_mmc3`, set in the CMU.
//!
//! # Transfer
//!
//! The CPU moves data through the buffer port by polling. No DMA and no interrupts, so caches
//! need no maintenance.
//!
//! # Concurrency
//!
//! One core at a time: a command is a sequence of accesses to shared registers.

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::register_structs;
use tock_registers::registers::{ReadOnly, ReadWrite};

use super::cmu::Cmu;
use crate::soc;

register_structs! {
    Registers {
        (0x00 => _reserved0),
        /// sdhci.h:30.
        (0x04 => block_size: ReadWrite<u16>),
        /// sdhci.h:33.
        (0x06 => block_count: ReadWrite<u16>),
        /// sdhci.h:35.
        (0x08 => argument: ReadWrite<u32>),
        /// sdhci.h:37.
        (0x0C => transfer_mode: ReadWrite<u16>),
        /// sdhci.h:46.
        (0x0E => command: ReadWrite<u16>),
        /// sdhci.h:61.
        (0x10 => response: ReadOnly<u32>),
        (0x14 => _reserved1),
        /// sdhci.h:63.
        (0x20 => buffer: ReadWrite<u32>),
        /// sdhci.h:65.
        (0x24 => present_state: ReadOnly<u32>),
        /// sdhci.h:83.
        (0x28 => host_control: ReadWrite<u8>),
        /// sdhci.h:97.
        (0x29 => power_control: ReadWrite<u8>),
        (0x2A => _reserved2),
        /// sdhci.h:110.
        (0x2C => clock_control: ReadWrite<u16>),
        /// sdhci.h:122.
        (0x2E => timeout_control: ReadWrite<u8>),
        /// sdhci.h:124.
        (0x2F => software_reset: ReadWrite<u8>),
        /// sdhci.h:129.
        (0x30 => int_status: ReadWrite<u32>),
        /// sdhci.h:130.
        (0x34 => int_enable: ReadWrite<u32>),
        /// sdhci.h:131.
        (0x38 => signal_enable: ReadWrite<u32>),
        (0x3C => _reserved3),
        /// sdhci.h:202.
        (0x40 => capabilities: ReadOnly<u32>),
        (0x44 => _reserved4),
        /// sdhci-s3c.c:34.
        (0x80 => control2: ReadWrite<u32>),
        /// sdhci-s3c.c:35.
        (0x84 => control3: ReadWrite<u32>),
        (0x88 => _reserved5),
        /// sdhci-s3c.c:36.
        (0x8C => control4: ReadWrite<u32>),
        (0x90 => _reserved6),
        /// sdhci.h:271.
        (0xFE => host_version: ReadOnly<u16>),
        (0x100 => @END),
    }
}

/// The most one block can hold: the 12 bits of the size field, `sdhci.h:31`, are more than the
/// SDIO functions here use.
pub const MAX_BLOCK_SIZE: usize = 2048;

/// sdhci.h:39,43,44.
const TRNS_BLK_CNT_EN: u16 = 0x02;
const TRNS_READ: u16 = 0x10;
const TRNS_MULTI: u16 = 0x20;
/// sdhci.h:48-50,53,55.
const CMD_RESP_NONE: u16 = 0x00;
const CMD_RESP_SHORT: u16 = 0x02;
const CMD_CRC: u16 = 0x08;
const CMD_INDEX: u16 = 0x10;
const CMD_DATA: u16 = 0x20;
/// sdhci.h:58: where the index goes in the command register.
const CMD_INDEX_SHIFT: u32 = 8;
/// sdhci.h:31: where the boundary of an SDMA transfer goes. It has no meaning here without DMA.
/// The value 7 is its largest.
const BLKSZ_BOUNDARY: u16 = 7 << 12;

/// sdhci.h:66-67.
const STATE_CMD_INHIBIT: u32 = 0x1;
const STATE_DATA_INHIBIT: u32 = 0x2;

/// sdhci.h:85.
const CTRL_4BITBUS: u8 = 0x02;
/// sdhci.h:98-101.
const POWER_ON: u8 = 0x01;
const POWER_180: u8 = 0x0A;
const POWER_300: u8 = 0x0C;
const POWER_330: u8 = 0x0E;
/// sdhci.h:217-219.
const CAN_VDD_330: u32 = 0x0100_0000;
const CAN_VDD_300: u32 = 0x0200_0000;
const CAN_VDD_180: u32 = 0x0400_0000;

/// sdhci.h:117-120.
const CLOCK_CARD_EN: u16 = 0x0004;
const CLOCK_INT_STABLE: u16 = 0x0002;
const CLOCK_INT_EN: u16 = 0x0001;
/// The longest timeout in the 4 bits of the register's field. The largest value, 0xF, is
/// reserved in the SD host controller specification.
const TIMEOUT_MAX: u8 = 0xE;
/// sdhci.h:125-127.
const RESET_ALL: u8 = 0x01;
const RESET_CMD: u8 = 0x02;
const RESET_DATA: u8 = 0x04;

/// sdhci.h:132-137.
const INT_RESPONSE: u32 = 0x0000_0001;
const INT_DATA_END: u32 = 0x0000_0002;
const INT_SPACE_AVAIL: u32 = 0x0000_0010;
const INT_DATA_AVAIL: u32 = 0x0000_0020;
/// sdhci.h:156 `SDHCI_INT_ERROR_MASK`, with sdhci.h:143 `SDHCI_INT_ERROR` its summary bit.
const INT_ERRORS: u32 = 0xFFFF_8000;
/// sdhci.h:144.
const INT_TIMEOUT: u32 = 0x0001_0000;
/// What this driver watches. The SDIO card interrupt stays out until something handles it.
const INT_WATCHED: u32 = INT_RESPONSE | INT_DATA_END | INT_SPACE_AVAIL | INT_DATA_AVAIL | INT_ERRORS;

/// sdhci-s3c.c:38,39,52,64: what `sdhci-s3c.c:281-285` sets for each clock rate.
const CTRL2_DEFAULTS: u32 = (1 << 31) | (1 << 30) | (1 << 14) | (1 << 8);
/// sdhci-s3c.c:68-69.
const CTRL2_SELBASECLK_MASK: u32 = 0x3 << 4;
const CTRL2_SELBASECLK_SHIFT: u32 = 4;
/// `exynos4.dtsi:351` names its clock `mmc_busclk.2`, and `sdhci-s3c.c:537,273` selects a source
/// by the number in that name.
const BASE_CLOCK_SOURCE: u32 = 2;
/// sdhci-s3c.c:74-77 `S3C_SDHCI_CTRL3_FCSEL3`..`0`: `sdhci-s3c.c:289` sets the first two always.
const CTRL3_FCSEL_ALWAYS: u32 = (1 << 15) | (1 << 7);
/// `sdhci-s3c.c:290-291`: and the last two below this rate.
const CTRL3_FCSEL_LOW: u32 = (1 << 31) | (1 << 23);
const CTRL3_LOW_BELOW_HZ: u32 = 25_000_000;
/// sdhci-s3c.c:100: the value `sdhci-s3c.c:277` writes.
const CTRL4_DRIVE_9MA: u32 = 0x3 << 16;

/// Turns a stuck controller or card into an error instead of a hang. Not measured: at the lowest
/// clock a command takes well under a millisecond, and this is many times longer.
const POLL_LIMIT: u32 = 5_000_000;

/// The shape of a command's response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// None.
    None,
    /// 48 bits, with neither a CRC nor the command's index in them: R4.
    ShortRaw,
    /// 48 bits, with both: R1, R5, R6.
    Short,
}

impl Response {
    const fn flags(self) -> u16 {
        match self {
            Self::None => CMD_RESP_NONE,
            Self::ShortRaw => CMD_RESP_SHORT,
            Self::Short => CMD_RESP_SHORT | CMD_CRC | CMD_INDEX,
        }
    }
}

/// A transfer, for [`Sdhci::write_data`] and [`Sdhci::read_data`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transfer {
    /// The command's index.
    pub index: u8,
    /// The command's argument.
    pub argument: u32,
    /// Bytes in one block.
    pub block_size: usize,
    /// Whether the transfer is of several blocks, counted by the controller, and not of one.
    pub multiple: bool,
}

/// Why a command failed. Hex values are the controller's raw interrupt status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller's internal clock did not stabilize.
    ClockNotStable,
    /// A reset did not finish.
    ResetStuck,
    /// The controller stayed busy with the previous command.
    Inhibited(u32),
    /// The card did not answer. This is the expected result of asking a card that is not there.
    Timeout(u32),
    /// The command failed.
    Command(u32),
    /// The transfer failed.
    Data(u32),
    /// The controller did not signal the next step.
    Stuck(u32),
    /// A buffer that does not fit the transfer asked for.
    BadLength,
}

/// Raw registers, for reports.
#[derive(Clone, Copy, Debug)]
pub struct State {
    /// `HOST_VERSION`.
    pub version: u16,
    /// `CAPABILITIES`.
    pub capabilities: u32,
    /// `PRESENT_STATE`.
    pub present_state: u32,
    /// `CONTROL2`.
    pub control2: u32,
    /// `CLOCK_CONTROL`.
    pub clock_control: u16,
    /// `POWER_CONTROL`.
    pub power_control: u8,
}

fn regs() -> &'static Registers {
    // SAFETY: `SDHCI3_BASE` is the address of this block, device memory that stays mapped for the
    // whole program, and `Registers` matches its layout. The caller has opened its clock gate.
    unsafe { &*(soc::SDHCI3_BASE as *const Registers) }
}

/// Handle to the controller. Holds no state.
#[derive(Clone, Copy, Debug)]
pub struct Sdhci {
    _private: (),
}

impl Sdhci {
    /// Opens the clock gate, resets the controller and turns on its bus power. The card clock
    /// stays off until [`set_clock`](Self::set_clock). Commands are polled: no signal is enabled.
    pub fn new() -> Result<Self, Error> {
        Cmu::new().open_sdmmc3_gate();
        let host = Self { _private: () };
        host.reset(RESET_ALL)?;
        let r = regs();
        r.signal_enable.set(0);
        r.int_enable.set(INT_WATCHED);
        r.int_status.set(u32::MAX);
        r.timeout_control.set(TIMEOUT_MAX);
        // `exynos4412-mehmet.dts:203-207` has 3 V on the card's supply. This field only states the
        // voltage to the controller: the board's rails do not depend on it.
        let caps = r.capabilities.get();
        let voltage = if caps & CAN_VDD_300 != 0 {
            POWER_300
        } else if caps & CAN_VDD_330 != 0 {
            POWER_330
        } else if caps & CAN_VDD_180 != 0 {
            POWER_180
        } else {
            POWER_330
        };
        r.power_control.set(voltage | POWER_ON);
        Ok(host)
    }

    /// The controller's registers as they are now.
    pub fn state(&self) -> State {
        let r = regs();
        State {
            version: r.host_version.get(),
            capabilities: r.capabilities.get(),
            present_state: r.present_state.get(),
            control2: r.control2.get(),
            clock_control: r.clock_control.get(),
            power_control: r.power_control.get(),
        }
    }

    fn reset(&self, mask: u8) -> Result<(), Error> {
        let r = regs();
        r.software_reset.set(mask);
        for _ in 0..POLL_LIMIT {
            if r.software_reset.get() & mask == 0 {
                return Ok(());
            }
        }
        Err(Error::ResetStuck)
    }

    /// Stops the card clock, sets its rate, and starts it again. Returns the rate, which is at
    /// most `hz`.
    pub fn set_clock(&self, hz: u32) -> Result<u32, Error> {
        let r = regs();
        r.clock_control.set(0);

        r.control4.set(CTRL4_DRIVE_9MA);
        let control2 = r.control2.get() & !CTRL2_SELBASECLK_MASK;
        r.control2.set(control2 | CTRL2_DEFAULTS | (BASE_CLOCK_SOURCE << CTRL2_SELBASECLK_SHIFT));
        let low = if hz < CTRL3_LOW_BELOW_HZ { CTRL3_FCSEL_LOW } else { 0 };
        r.control3.set(CTRL3_FCSEL_ALWAYS | low);

        let rate = Cmu::new().set_mmc3_clock(hz);

        r.clock_control.set(CLOCK_INT_EN);
        let mut polls = 0;
        while r.clock_control.get() & CLOCK_INT_STABLE == 0 {
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::ClockNotStable);
            }
        }
        r.clock_control.set(CLOCK_INT_EN | CLOCK_CARD_EN);
        Ok(rate)
    }

    /// Sets the width of the data bus in the controller, to 4 bits or to 1.
    pub fn set_bus_4bit(&self, wide: bool) {
        let r = regs();
        let control = r.host_control.get() & !CTRL_4BITBUS;
        r.host_control.set(control | if wide { CTRL_4BITBUS } else { 0 });
    }

    fn wait_idle(&self, mask: u32) -> Result<(), Error> {
        let r = regs();
        for _ in 0..POLL_LIMIT {
            if r.present_state.get() & mask == 0 {
                return Ok(());
            }
        }
        Err(Error::Inhibited(r.present_state.get()))
    }

    /// Starts a command, and waits for its response. An error also resets what it left behind.
    fn start(&self, index: u8, argument: u32, flags: u16, mode: u16) -> Result<u32, Error> {
        let r = regs();
        r.argument.set(argument);
        r.transfer_mode.set(mode);
        r.command.set((u16::from(index) << CMD_INDEX_SHIFT) | flags);
        let mut polls = 0;
        loop {
            let status = r.int_status.get();
            if status & INT_ERRORS != 0 {
                r.int_status.set(status);
                self.reset(RESET_CMD | RESET_DATA)?;
                return Err(if status & INT_TIMEOUT != 0 {
                    Error::Timeout(status)
                } else {
                    Error::Command(status)
                });
            }
            if status & INT_RESPONSE != 0 {
                r.int_status.set(INT_RESPONSE);
                return Ok(r.response.get());
            }
            polls += 1;
            if polls == POLL_LIMIT {
                self.reset(RESET_CMD | RESET_DATA)?;
                return Err(Error::Stuck(status));
            }
        }
    }

    /// Sends a command with no data, and returns its response, or 0 if it has none.
    pub fn command(&self, index: u8, argument: u32, response: Response) -> Result<u32, Error> {
        self.wait_idle(STATE_CMD_INHIBIT)?;
        regs().int_status.set(u32::MAX);
        self.start(index, argument, response.flags(), 0)
    }

    fn start_transfer(&self, transfer: Transfer, length: usize, read: bool) -> Result<u32, Error> {
        let block = transfer.block_size;
        if block == 0
            || block > MAX_BLOCK_SIZE
            || length == 0
            || !length.is_multiple_of(block)
            || length / block > usize::from(u16::MAX)
            || (!transfer.multiple && length != block)
        {
            return Err(Error::BadLength);
        }
        self.wait_idle(STATE_CMD_INHIBIT | STATE_DATA_INHIBIT)?;
        let r = regs();
        r.int_status.set(u32::MAX);
        r.block_size.set(BLKSZ_BOUNDARY | block as u16);
        r.block_count.set((length / block) as u16);
        let mut mode = if read { TRNS_READ } else { 0 };
        if transfer.multiple {
            mode |= TRNS_BLK_CNT_EN | TRNS_MULTI;
        }
        let flags = Response::Short.flags() | CMD_DATA;
        self.start(transfer.index, transfer.argument, flags, mode)
    }

    fn wait_for(&self, flag: u32) -> Result<(), Error> {
        let r = regs();
        for _ in 0..POLL_LIMIT {
            let status = r.int_status.get();
            if status & INT_ERRORS != 0 {
                r.int_status.set(status);
                self.reset(RESET_CMD | RESET_DATA)?;
                return Err(Error::Data(status));
            }
            if status & flag != 0 {
                r.int_status.set(flag);
                return Ok(());
            }
        }
        let status = r.int_status.get();
        self.reset(RESET_CMD | RESET_DATA)?;
        Err(Error::Stuck(status))
    }

    /// Runs a command that reads `buffer`, and returns its response.
    pub fn read_data(&self, transfer: Transfer, buffer: &mut [u8]) -> Result<u32, Error> {
        let response = self.start_transfer(transfer, buffer.len(), true)?;
        let r = regs();
        for block in buffer.chunks_mut(transfer.block_size) {
            self.wait_for(INT_DATA_AVAIL)?;
            for bytes in block.chunks_mut(4) {
                let word = r.buffer.get().to_le_bytes();
                bytes.copy_from_slice(&word[..bytes.len()]);
            }
        }
        self.wait_for(INT_DATA_END)?;
        Ok(response)
    }

    /// Runs a command that writes `buffer`, and returns its response.
    pub fn write_data(&self, transfer: Transfer, buffer: &[u8]) -> Result<u32, Error> {
        let response = self.start_transfer(transfer, buffer.len(), false)?;
        let r = regs();
        for block in buffer.chunks(transfer.block_size) {
            self.wait_for(INT_SPACE_AVAIL)?;
            for bytes in block.chunks(4) {
                let mut word = [0; 4];
                word[..bytes.len()].copy_from_slice(bytes);
                r.buffer.set(u32::from_le_bytes(word));
            }
        }
        self.wait_for(INT_DATA_END)?;
        Ok(response)
    }
}
