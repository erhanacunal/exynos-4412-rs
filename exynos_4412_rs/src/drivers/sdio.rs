//! SDIO card protocol, over [`Sdhci`].
//!
//! Command numbers and register addresses are verified against Linux 5.15.198
//! `include/linux/mmc/sdio.h`, cited as `sdio.h:<line>`. The formats of the commands' arguments are
//! the ones listed at `sdio.h:16-34`.
//!
//! # Concurrency
//!
//! One core at a time, like [`Sdhci`].

use super::sdhci::{self, Response, Sdhci, Transfer};
use super::timer::Mct;
use crate::soc;

/// `include/linux/mmc/sd.h:14`.
const CMD_SEND_RELATIVE_ADDR: u8 = 3;
/// sdio.h:12.
const CMD_SEND_OP_COND: u8 = 5;
/// `include/linux/mmc/mmc.h:38`.
const CMD_SELECT_CARD: u8 = 7;
/// sdio.h:13.
const CMD_RW_DIRECT: u8 = 52;
/// sdio.h:14.
const CMD_RW_EXTENDED: u8 = 53;

/// The bit in a response to `CMD5` that says the card has finished starting: bit 31 of the OCR.
const R4_READY: u32 = 1 << 31;
const R4_FUNCTIONS_SHIFT: u32 = 28;
const R4_FUNCTIONS_MASK: u32 = 0x7;
const R4_OCR_MASK: u32 = 0x00FF_FFFF;
/// `include/linux/mmc/host.h:320,321`: 2.9 to 3.1 V, around the 3 V of the card's supply in
/// `exynos4412-mehmet.dts:203-207`.
const OCR_WANTED: u32 = 0x0002_0000 | 0x0004_0000;
/// sdio.h:60 `R5_STATUS`: the error bits of an R5.
const R5_ERRORS: u32 = 0xCB00;

/// sdio.h:67.
const CCCR_REVISION: u32 = 0x00;
/// sdio.h:80.
const CCCR_SD: u32 = 0x01;
/// sdio.h:87.
const CCCR_IO_ENABLE: u32 = 0x02;
/// sdio.h:88.
const CCCR_IO_READY: u32 = 0x03;
/// sdio.h:95.
const CCCR_BUS_INTERFACE: u32 = 0x07;
/// sdio.h:97,100.
const BUS_WIDTH_MASK: u8 = 0x03;
const BUS_WIDTH_4BIT: u8 = 0x02;
/// sdio.h:108.
const CCCR_CAPABILITIES: u32 = 0x08;
/// sdio.h:119: three bytes, low first.
const CCCR_CIS_POINTER: u32 = 0x09;
/// sdio.h:166: where the registers of function `f` start.
const FBR_STRIDE: u32 = 0x100;
/// sdio.h:187: two bytes, low first.
const FBR_BLOCK_SIZE: u32 = 0x10;

/// `include/pcmcia/cistpl.h:43`.
const CISTPL_MANFID: u8 = 0x20;
/// `include/pcmcia/cistpl.h:47`.
const CISTPL_END: u8 = 0xFF;
/// `include/pcmcia/cistpl.h:17`.
const CISTPL_NULL: u8 = 0x00;
/// A CIS with more tuples than this is not one to keep following.
const CIS_MAX_TUPLES: u32 = 64;

/// The bytes one `CMD53` in byte mode moves at most: 0 in its 9-bit count means 512.
const BYTE_MODE_MAX: usize = 512;
/// The blocks one `CMD53` moves at most: 0 in its 9-bit count does not mean 512 in block mode.
const BLOCK_COUNT_MAX: usize = 511;
/// The count field of `CMD53`: sdio.h:34.
const COUNT_MASK: u32 = 0x1FF;
/// How long a card may take to start, as a timeout. Not measured.
const READY_TIMEOUT_MS: u64 = 1000;
/// Wait between asks.
const READY_POLL_MS: u64 = 10;
/// The clock must run for 74 cycles before the first command, 185 us at 400 kHz. Well over that.
const FIRST_CLOCK_MS: u64 = 2;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// Why a call to the card failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller reports a failure.
    Host(sdhci::Error),
    /// The card reports an error: the raw R5.
    Card(u32),
    /// The card did not finish starting, with its last answer to `CMD5`.
    NotReady(u32),
    /// A function did not become ready.
    FunctionNotReady(u8),
    /// A block size the card cannot use.
    BadLength,
}

impl From<sdhci::Error> for Error {
    fn from(error: sdhci::Error) -> Self {
        Self::Host(error)
    }
}

/// What the card says about itself as it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardInfo {
    /// The voltage windows the card offers, from its first answer to `CMD5`.
    pub ocr: u32,
    /// Number of I/O functions, not counting function 0.
    pub functions: u8,
    /// The address it gave itself.
    pub rca: u16,
}

/// Waits, from the global counter of the MCT, which must be running.
pub fn delay_ms(mct: Mct, ms: u64) {
    let start = mct.count();
    while mct.count() - start < ms * COUNTS_PER_MS {
        core::hint::spin_loop();
    }
}

/// An SDIO card that has been started and selected.
#[derive(Clone, Copy, Debug)]
pub struct Sdio {
    host: Sdhci,
    info: CardInfo,
}

impl Sdio {
    /// Starts a card that has just come out of reset. The clock in `host` must be at a card
    /// identification rate, at most 400 kHz, with the bus at 1 bit.
    pub fn start(host: Sdhci, mct: Mct) -> Result<Self, Error> {
        delay_ms(mct, FIRST_CLOCK_MS);

        let first = host.command(CMD_SEND_OP_COND, 0, Response::ShortRaw)?;
        let ocr = first & R4_OCR_MASK;
        // A window it does not offer would be refused. The rails do not move either way, so with
        // none in common this asks for what the card offers.
        let asked = if ocr & OCR_WANTED != 0 { ocr & OCR_WANTED } else { ocr };

        let begin = mct.count();
        let mut answer;
        loop {
            answer = host.command(CMD_SEND_OP_COND, asked, Response::ShortRaw)?;
            if answer & R4_READY != 0 {
                break;
            }
            if mct.count() - begin > READY_TIMEOUT_MS * COUNTS_PER_MS {
                return Err(Error::NotReady(answer));
            }
            delay_ms(mct, READY_POLL_MS);
        }
        let functions = ((answer >> R4_FUNCTIONS_SHIFT) & R4_FUNCTIONS_MASK) as u8;

        let rca = (host.command(CMD_SEND_RELATIVE_ADDR, 0, Response::Short)? >> 16) as u16;
        host.command(CMD_SELECT_CARD, u32::from(rca) << 16, Response::Short)?;

        Ok(Self { host, info: CardInfo { ocr, functions, rca } })
    }

    /// What the card said as it started.
    pub const fn info(&self) -> CardInfo {
        self.info
    }

    /// Reads one byte with `CMD52`.
    pub fn read_byte(&self, function: u8, address: u32) -> Result<u8, Error> {
        self.direct(false, function, address, 0)
    }

    /// Writes one byte with `CMD52`.
    pub fn write_byte(&self, function: u8, address: u32, value: u8) -> Result<(), Error> {
        self.direct(true, function, address, value).map(|_| ())
    }

    fn direct(&self, write: bool, function: u8, address: u32, value: u8) -> Result<u8, Error> {
        let argument = (u32::from(write) << 31)
            | (u32::from(function) << 28)
            | (address << 9)
            | u32::from(value);
        let response = self.host.command(CMD_RW_DIRECT, argument, Response::Short)?;
        if response & R5_ERRORS != 0 {
            return Err(Error::Card(response));
        }
        Ok(response as u8)
    }

    /// Reads `buffer` with `CMD53`, at successive addresses if `increment`. Whole blocks go in
    /// block mode whenever more is left than a byte-mode transfer can take.
    pub fn read(
        &self,
        function: u8,
        address: u32,
        increment: bool,
        block_size: usize,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        let mut done = 0;
        while done < buffer.len() {
            let at = if increment { address + done as u32 } else { address };
            let (length, argument, transfer) =
                Self::plan(false, function, at, increment, block_size, buffer.len() - done)?;
            let transfer = Transfer { argument, ..transfer };
            let response = self.host.read_data(transfer, &mut buffer[done..done + length])?;
            if response & R5_ERRORS != 0 {
                return Err(Error::Card(response));
            }
            done += length;
        }
        Ok(())
    }

    /// Writes `buffer` with `CMD53`, at successive addresses if `increment`. Whole blocks go in
    /// block mode whenever more is left than a byte-mode transfer can take.
    pub fn write(
        &self,
        function: u8,
        address: u32,
        increment: bool,
        block_size: usize,
        buffer: &[u8],
    ) -> Result<(), Error> {
        let mut done = 0;
        while done < buffer.len() {
            let at = if increment { address + done as u32 } else { address };
            let (length, argument, transfer) =
                Self::plan(true, function, at, increment, block_size, buffer.len() - done)?;
            let transfer = Transfer { argument, ..transfer };
            let response = self.host.write_data(transfer, &buffer[done..done + length])?;
            if response & R5_ERRORS != 0 {
                return Err(Error::Card(response));
            }
            done += length;
        }
        Ok(())
    }

    /// How much of `left` bytes the next `CMD53` moves, its argument, and its transfer.
    fn plan(
        write: bool,
        function: u8,
        address: u32,
        increment: bool,
        block_size: usize,
        left: usize,
    ) -> Result<(usize, u32, Transfer), Error> {
        if block_size == 0 || block_size > BYTE_MODE_MAX {
            return Err(Error::BadLength);
        }
        let blocks = left > block_size;
        let (length, count, size) = if blocks {
            let count = (left / block_size).min(BLOCK_COUNT_MAX);
            (count * block_size, count, block_size)
        } else {
            (left, left, left)
        };
        let argument = (u32::from(write) << 31)
            | (u32::from(function) << 28)
            | (u32::from(blocks) << 27)
            | (u32::from(increment) << 26)
            | (address << 9)
            | (count as u32 & COUNT_MASK);
        let transfer = Transfer { index: CMD_RW_EXTENDED, argument, block_size: size, multiple: blocks };
        Ok((length, argument, transfer))
    }

    /// The first two registers of the card's common control block: revisions of the CCCR format
    /// and SDIO specification, and of the SD specification.
    pub fn revisions(&self) -> Result<(u8, u8), Error> {
        Ok((self.read_byte(0, CCCR_REVISION)?, self.read_byte(0, CCCR_SD)?))
    }

    /// The card's capability register.
    pub fn capabilities(&self) -> Result<u8, Error> {
        self.read_byte(0, CCCR_CAPABILITIES)
    }

    /// The bus interface register.
    pub fn bus_interface(&self) -> Result<u8, Error> {
        self.read_byte(0, CCCR_BUS_INTERFACE)
    }

    /// Follows the card's common CIS to its vendor and device numbers.
    pub fn manufacturer(&self) -> Result<Option<(u16, u16)>, Error> {
        let mut address = 0;
        for i in 0..3 {
            address |= u32::from(self.read_byte(0, CCCR_CIS_POINTER + i)?) << (8 * i);
        }
        for _ in 0..CIS_MAX_TUPLES {
            let code = self.read_byte(0, address)?;
            match code {
                CISTPL_END => return Ok(None),
                CISTPL_NULL => {
                    address += 1;
                    continue;
                }
                _ => {}
            }
            let length = self.read_byte(0, address + 1)?;
            if code == CISTPL_MANFID && length >= 4 {
                let mut id = [0; 4];
                for (i, byte) in id.iter_mut().enumerate() {
                    *byte = self.read_byte(0, address + 2 + i as u32)?;
                }
                return Ok(Some((
                    u16::from_le_bytes([id[0], id[1]]),
                    u16::from_le_bytes([id[2], id[3]]),
                )));
            }
            if length == 0xFF {
                return Ok(None);
            }
            address += 2 + u32::from(length);
        }
        Ok(None)
    }

    /// Sets the data bus to 4 bits in the card, then in the controller.
    pub fn set_bus_4bit(&self) -> Result<(), Error> {
        let interface = self.bus_interface()?;
        self.write_byte(0, CCCR_BUS_INTERFACE, (interface & !BUS_WIDTH_MASK) | BUS_WIDTH_4BIT)?;
        self.host.set_bus_4bit(true);
        Ok(())
    }

    /// Sets the block size of a function.
    pub fn set_block_size(&self, function: u8, block_size: u16) -> Result<(), Error> {
        let registers = u32::from(function) * FBR_STRIDE + FBR_BLOCK_SIZE;
        let [low, high] = block_size.to_le_bytes();
        self.write_byte(0, registers, low)?;
        self.write_byte(0, registers + 1, high)
    }

    /// Enables a function and waits up to `timeout_ms` for it to be ready.
    pub fn enable_function(&self, mct: Mct, function: u8, timeout_ms: u64) -> Result<(), Error> {
        let bit = 1 << function;
        let enabled = self.read_byte(0, CCCR_IO_ENABLE)?;
        self.write_byte(0, CCCR_IO_ENABLE, enabled | bit)?;
        let begin = mct.count();
        while self.read_byte(0, CCCR_IO_READY)? & bit == 0 {
            if mct.count() - begin > timeout_ms * COUNTS_PER_MS {
                return Err(Error::FunctionNotReady(function));
            }
            delay_ms(mct, READY_POLL_MS);
        }
        Ok(())
    }
}
