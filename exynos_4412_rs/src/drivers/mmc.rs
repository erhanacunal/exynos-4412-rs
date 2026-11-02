//! Read-only access to the board's eMMC, through the DW MMC controller `mshc_0`.
//!
//! # Read-only by construction
//!
//! This driver can send only these commands, all listed in its private `Command` type: `CMD8` (read
//! `EXT_CSD`), `CMD13` (read status), `CMD17` and `CMD18` (read blocks, the latter with the
//! controller's automatic `CMD12` to stop). There is no write, erase or switch command anywhere
//! in it, and the controller's write-direction bit has no constant here.
//!
//! # U-Boot's setup
//!
//! The card is not initialized here: U-Boot has just read this image from it, so the card is
//! selected and in its transfer state, with clock, bus width and timing configured. This driver
//! keeps all of that. It does not reset the card (`CMD0`, or its reset pin `gpk1-2`), touch its
//! rails, or change its bus settings, since `CMD6` writes to the card's `EXT_CSD`. It follows
//! that a card left in another state, such as after a failed multi-block read, stays unusable
//! until the board restarts.
//!
//! # Transfer
//!
//! The CPU reads the controller's FIFO by polling. No DMA and no interrupts, so caches need no
//! maintenance.
//!
//! Register offsets and fields are verified against Linux 5.15.198
//! `drivers/mmc/host/dw_mmc.h`, cited as `dw_mmc.h:<line>`, and command numbers against
//! `include/linux/mmc/mmc.h`, cited as `mmc.h:<line>`.
//!
//! # Concurrency
//!
//! One core at a time: a transfer is a sequence of accesses to shared registers.

use core::arch::asm;

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::register_structs;
use tock_registers::registers::{ReadOnly, ReadWrite};

use super::cmu::Cmu;
use crate::soc;

register_structs! {
    Registers {
        /// dw_mmc.h:280.
        (0x000 => ctrl: ReadWrite<u32>),
        (0x004 => _reserved0),
        /// dw_mmc.h:282.
        (0x008 => clkdiv: ReadOnly<u32>),
        (0x00C => _reserved1),
        /// dw_mmc.h:284.
        (0x010 => clkena: ReadOnly<u32>),
        (0x014 => _reserved2),
        /// dw_mmc.h:286.
        (0x018 => ctype: ReadOnly<u32>),
        /// dw_mmc.h:287.
        (0x01C => blksiz: ReadWrite<u32>),
        /// dw_mmc.h:288.
        (0x020 => bytcnt: ReadWrite<u32>),
        /// dw_mmc.h:289.
        (0x024 => intmask: ReadWrite<u32>),
        /// dw_mmc.h:290.
        (0x028 => cmdarg: ReadWrite<u32>),
        /// dw_mmc.h:291.
        (0x02C => cmd: ReadWrite<u32>),
        /// dw_mmc.h:292.
        (0x030 => resp0: ReadOnly<u32>),
        (0x034 => _reserved3),
        /// dw_mmc.h:297.
        (0x044 => rintsts: ReadWrite<u32>),
        /// dw_mmc.h:298.
        (0x048 => status: ReadOnly<u32>),
        /// dw_mmc.h:299.
        (0x04C => fifoth: ReadOnly<u32>),
        (0x050 => _reserved4),
        /// dw_mmc.h:307.
        (0x06C => verid: ReadOnly<u32>),
        /// dw_mmc.h:308.
        (0x070 => hcon: ReadOnly<u32>),
        (0x074 => _reserved5),
        /// dw_mmc.h:311.
        (0x080 => bmod: ReadWrite<u32>),
        (0x084 => _reserved6),
        /// `dw_mmc-exynos.h:11`.
        (0x09C => clksel: ReadOnly<u32>),
        (0x0A0 => @END),
    }
}

/// Bytes in one block.
pub const BLOCK_SIZE: usize = 512;

/// dw_mmc.h:346.
const CTRL_USE_IDMAC: u32 = 1 << 25;
/// dw_mmc.h:353.
const CTRL_DMA_ENABLE: u32 = 1 << 5;
/// dw_mmc.h:354.
const CTRL_INT_ENABLE: u32 = 1 << 4;
/// dw_mmc.h:356.
const CTRL_FIFO_RESET: u32 = 1 << 1;
/// dw_mmc.h:360.
const CLKENA_ENABLE: u32 = 1 << 0;
/// dw_mmc.h:434.
const BMOD_IDMAC_ENABLE: u32 = 1 << 7;

/// dw_mmc.h:372.
const INT_EBE: u32 = 1 << 15;
/// dw_mmc.h:373.
const INT_ACD: u32 = 1 << 14;
/// dw_mmc.h:374.
const INT_SBE: u32 = 1 << 13;
/// dw_mmc.h:375.
const INT_HLE: u32 = 1 << 12;
/// dw_mmc.h:376.
const INT_FRUN: u32 = 1 << 11;
/// dw_mmc.h:377 `SDMMC_INT_HTO`.
const INT_HTO: u32 = 1 << 10;
/// dw_mmc.h:379.
const INT_DRTO: u32 = 1 << 9;
/// dw_mmc.h:380.
const INT_RTO: u32 = 1 << 8;
/// dw_mmc.h:381.
const INT_DCRC: u32 = 1 << 7;
/// dw_mmc.h:382.
const INT_RCRC: u32 = 1 << 6;
/// dw_mmc.h:385.
const INT_DATA_OVER: u32 = 1 << 3;
/// dw_mmc.h:386.
const INT_CMD_DONE: u32 = 1 << 2;
/// dw_mmc.h:387.
const INT_RESP_ERR: u32 = 1 << 1;
/// `dw_mmc.c:47-48` `DW_MCI_CMD_ERROR_FLAGS`.
const CMD_ERRORS: u32 = INT_RTO | INT_RCRC | INT_RESP_ERR | INT_HLE;
/// `dw_mmc.c:44-46` `DW_MCI_DATA_ERROR_FLAGS`, plus FIFO overrun, which polling could cause.
const DATA_ERRORS: u32 = INT_DRTO | INT_DCRC | INT_HTO | INT_SBE | INT_EBE | INT_HLE | INT_FRUN;

/// dw_mmc.h:391.
const CMD_START: u32 = 1 << 31;
/// dw_mmc.h:392.
const CMD_USE_HOLD_REG: u32 = 1 << 29;
/// dw_mmc.h:399.
const CMD_PRV_DAT_WAIT: u32 = 1 << 13;
/// dw_mmc.h:400: the controller sends `CMD12` itself after the last block.
const CMD_SEND_STOP: u32 = 1 << 12;
/// dw_mmc.h:403.
const CMD_DAT_EXP: u32 = 1 << 9;
/// dw_mmc.h:404.
const CMD_RESP_CRC: u32 = 1 << 8;
/// dw_mmc.h:406.
const CMD_RESP_EXP: u32 = 1 << 6;

/// dw_mmc.h:409 `SDMMC_GET_FCNT`.
const STATUS_FCNT_SHIFT: u32 = 17;
const STATUS_FCNT_MASK: u32 = 0x1FFF;
/// dw_mmc.h:411.
const STATUS_BUSY: u32 = 1 << 9;

/// dw_mmc.h:423 `SDMMC_GET_HDATA_WIDTH`.
const HCON_HDATA_SHIFT: u32 = 7;
const HCON_HDATA_MASK: u32 = 0x7;
/// `dw_mmc.c:3328` is 64 bits for the value 2, and `dw_mmc.c:3335` leaves 32 bits for the value 1.
const HDATA_32: u32 = 1;
const HDATA_64: u32 = 2;
/// dw_mmc.h:277 `DW_MMC_240A`: from this version on the FIFO is at dw_mmc.h:340 `DATA_240A_OFFSET`,
/// and before it at dw_mmc.h:339 `DATA_OFFSET`, by `dw_mmc.c:3394-3397`.
const VERID_240A: u32 = 0x240A;
const FIFO_OFFSET: usize = 0x100;
const FIFO_240A_OFFSET: usize = 0x200;

/// mmc.h:148 `R1_CURRENT_STATE`.
const R1_STATE_SHIFT: u32 = 9;
const R1_STATE_MASK: u32 = 0xF;
/// mmc.h:128-143: the error bits of an R1 response, 31 `OUT_OF_RANGE` to 16 `CID_CSD_OVERWRITE`,
/// leaving out 25 `CARD_IS_LOCKED`, which reports a state.
const R1_ERRORS: u32 = 0xFDFF_0000;

/// `EXT_CSD` byte offsets: mmc.h:273,275,277,279,289,295.
const EXT_CSD_PART_CONFIG: usize = 179;
const EXT_CSD_BUS_WIDTH: usize = 183;
const EXT_CSD_HS_TIMING: usize = 185;
const EXT_CSD_REV: usize = 192;
const EXT_CSD_SEC_CNT: usize = 212;
const EXT_CSD_BOOT_MULT: usize = 226;
/// mmc.h:335 `EXT_CSD_PART_CONFIG_ACC_MASK`: which hardware partition reads come from. 0 is the
/// user area, where the partition table is.
const PART_CONFIG_ACCESS_MASK: u8 = 0x7;

/// Turns a stuck controller or card into an error instead of a hang.
const POLL_LIMIT: u32 = 50_000_000;

/// Every command this driver can send. None of them can change the card's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    /// mmc.h:39 `MMC_SEND_EXT_CSD`.
    SendExtCsd,
    /// mmc.h:44 `MMC_SEND_STATUS`.
    SendStatus,
    /// mmc.h:53 `MMC_READ_SINGLE_BLOCK`.
    ReadSingleBlock,
    /// mmc.h:54 `MMC_READ_MULTIPLE_BLOCK`.
    ReadMultipleBlock,
}

impl Command {
    const fn flags(self) -> u32 {
        let (index, extra) = match self {
            Self::SendExtCsd => (8, CMD_DAT_EXP),
            Self::SendStatus => (13, 0),
            Self::ReadSingleBlock => (17, CMD_DAT_EXP),
            Self::ReadMultipleBlock => (18, CMD_DAT_EXP | CMD_SEND_STOP),
        };
        index | extra | CMD_START | CMD_USE_HOLD_REG | CMD_PRV_DAT_WAIT | CMD_RESP_EXP | CMD_RESP_CRC
    }
}

/// Why a read failed. Hex values are the controller's raw interrupt status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller's clock gate or card clock is off: U-Boot did not leave it usable.
    ClockOff,
    /// This FIFO width is not supported.
    FifoWidth(u32),
    /// A buffer that is not a whole number of blocks, or too large for one transfer.
    BadLength,
    /// The controller did not take the command.
    CommandNotTaken,
    /// The command failed, or got no answer.
    Command(u32),
    /// The card reported an error in its response.
    Card(u32),
    /// The transfer failed.
    Data(u32),
    /// Nothing happened within the time limit. Holds the raw interrupt status.
    Timeout(u32),
}

/// What [`Mmc::read_ext_csd`] finds.
#[derive(Clone, Copy, Debug)]
pub struct ExtCsd {
    /// Blocks in the user area.
    pub sectors: u32,
    /// `EXT_CSD_REV`.
    pub revision: u8,
    /// `PARTITION_CONFIG` byte.
    pub part_config: u8,
    /// `BUS_WIDTH` byte.
    pub bus_width: u8,
    /// `HS_TIMING` byte.
    pub hs_timing: u8,
    /// Size of each boot partition, in units of 128 KiB.
    pub boot_mult: u8,
}

impl ExtCsd {
    /// Whether reads come from the user area, as opposed to a boot or RPMB partition.
    pub const fn reads_user_area(&self) -> bool {
        self.part_config & PART_CONFIG_ACCESS_MASK == 0
    }
}

/// The controller's setup, as raw registers, for reports.
#[derive(Clone, Copy, Debug)]
pub struct ControllerState {
    /// `VERID`.
    pub verid: u32,
    /// `HCON`.
    pub hcon: u32,
    /// `CTRL`.
    pub ctrl: u32,
    /// `BMOD`.
    pub bmod: u32,
    /// `CLKENA`.
    pub clkena: u32,
    /// `CLKDIV`.
    pub clkdiv: u32,
    /// `CLKSEL`.
    pub clksel: u32,
    /// `CTYPE`.
    pub ctype: u32,
    /// `FIFOTH`.
    pub fifoth: u32,
    /// `STATUS`.
    pub status: u32,
}

/// Handle to the eMMC controller. Holds what it read of the FIFO's layout.
#[derive(Clone, Copy, Debug)]
pub struct Mmc {
    fifo: usize,
    fifo_64: bool,
}

/// The controller's state, or `None` if its gate is closed, when reading it could hang.
pub fn controller_state() -> Option<ControllerState> {
    if !Cmu::new().mmc4_bus_clock_on() {
        return None;
    }
    let regs = regs();
    Some(ControllerState {
        verid: regs.verid.get(),
        hcon: regs.hcon.get(),
        ctrl: regs.ctrl.get(),
        bmod: regs.bmod.get(),
        clkena: regs.clkena.get(),
        clkdiv: regs.clkdiv.get(),
        clksel: regs.clksel.get(),
        ctype: regs.ctype.get(),
        fifoth: regs.fifoth.get(),
        status: regs.status.get(),
    })
}

fn regs() -> &'static Registers {
    // SAFETY: MSHC_BASE is this MMIO block, device memory that stays mapped for the whole
    // program, and the struct has interior mutable cells only.
    unsafe { &*(soc::MSHC_BASE as *const Registers) }
}

impl Mmc {
    /// Takes over the controller from U-Boot for polled transfers: turns off its DMA and
    /// interrupts, leaving clock, bus width and timing as they are. Sends nothing to the card.
    pub fn new() -> Result<Self, Error> {
        if !Cmu::new().mmc4_bus_clock_on() {
            return Err(Error::ClockOff);
        }
        let regs = regs();
        if regs.clkena.get() & CLKENA_ENABLE == 0 {
            return Err(Error::ClockOff);
        }
        let hdata = (regs.hcon.get() >> HCON_HDATA_SHIFT) & HCON_HDATA_MASK;
        if hdata != HDATA_32 && hdata != HDATA_64 {
            return Err(Error::FifoWidth(hdata));
        }

        regs.intmask.set(0);
        regs.ctrl.set(regs.ctrl.get() & !(CTRL_USE_IDMAC | CTRL_DMA_ENABLE | CTRL_INT_ENABLE));
        regs.bmod.set(regs.bmod.get() & !BMOD_IDMAC_ENABLE);

        let offset = if regs.verid.get() & 0xFFFF < VERID_240A { FIFO_OFFSET } else { FIFO_240A_OFFSET };
        Ok(Self { fifo: soc::MSHC_BASE + offset, fifo_64: hdata == HDATA_64 })
    }

    /// Whether the FIFO is 64 bits wide, rather than 32.
    pub const fn fifo_is_64_bit(&self) -> bool {
        self.fifo_64
    }

    /// Reads the card's status with `CMD13`, addressed to `rca`. The state is in bits 12:9: see
    /// [`card_state`]. U-Boot picks the card's address and does not say which it chose, so a
    /// wrong guess gets [`Error::Command`], which does no harm.
    pub fn read_status(&self, rca: u16) -> Result<u32, Error> {
        self.command(Command::SendStatus, u32::from(rca) << 16)?;
        Ok(regs().resp0.get())
    }

    /// Reads the card's `EXT_CSD` with `CMD8`.
    pub fn read_ext_csd(&self) -> Result<ExtCsd, Error> {
        let mut raw = [0; BLOCK_SIZE];
        self.transfer(Command::SendExtCsd, 0, &mut raw)?;
        Ok(ExtCsd {
            sectors: u32::from_le_bytes([
                raw[EXT_CSD_SEC_CNT],
                raw[EXT_CSD_SEC_CNT + 1],
                raw[EXT_CSD_SEC_CNT + 2],
                raw[EXT_CSD_SEC_CNT + 3],
            ]),
            revision: raw[EXT_CSD_REV],
            part_config: raw[EXT_CSD_PART_CONFIG],
            bus_width: raw[EXT_CSD_BUS_WIDTH],
            hs_timing: raw[EXT_CSD_HS_TIMING],
            boot_mult: raw[EXT_CSD_BOOT_MULT],
        })
    }

    /// Reads whole blocks into `buffer`, from block `lba` of the user area. A card over 2 GiB
    /// addresses by block, which is what this assumes: check [`ExtCsd::sectors`].
    pub fn read_blocks(&self, lba: u32, buffer: &mut [u8]) -> Result<(), Error> {
        if buffer.is_empty() || !buffer.len().is_multiple_of(BLOCK_SIZE) || buffer.len() > u32::MAX as usize {
            return Err(Error::BadLength);
        }
        let command = if buffer.len() == BLOCK_SIZE { Command::ReadSingleBlock } else { Command::ReadMultipleBlock };
        self.transfer(command, lba, buffer)
    }

    /// Sends a command and waits for its response. On return, the raw status is clean of errors.
    fn command(&self, command: Command, argument: u32) -> Result<(), Error> {
        let regs = regs();
        regs.rintsts.set(u32::MAX);
        regs.cmdarg.set(argument);
        regs.cmd.set(command.flags());

        let mut polls = 0;
        while regs.cmd.get() & CMD_START != 0 {
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::CommandNotTaken);
            }
        }
        polls = 0;
        loop {
            let status = regs.rintsts.get();
            if status & CMD_ERRORS != 0 {
                return Err(Error::Command(status));
            }
            if status & INT_CMD_DONE != 0 {
                break;
            }
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::Timeout(status));
            }
        }
        let response = regs.resp0.get();
        if response & R1_ERRORS != 0 {
            return Err(Error::Card(response));
        }
        Ok(())
    }

    fn transfer(&self, command: Command, argument: u32, buffer: &mut [u8]) -> Result<(), Error> {
        let regs = regs();
        let mut polls = 0;
        while regs.status.get() & STATUS_BUSY != 0 {
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::Timeout(regs.rintsts.get()));
            }
        }
        self.reset_fifo()?;
        regs.blksiz.set(BLOCK_SIZE as u32);
        regs.bytcnt.set(buffer.len() as u32);

        self.command(command, argument)?;

        let mut done = 0;
        polls = 0;
        loop {
            let status = regs.rintsts.get();
            if status & DATA_ERRORS != 0 {
                return Err(Error::Data(status));
            }
            let words = (regs.status.get() >> STATUS_FCNT_SHIFT) & STATUS_FCNT_MASK;
            if words > 0 {
                polls = 0;
                for _ in 0..words {
                    done = self.pull_word(buffer, done);
                }
            } else if done >= buffer.len() && status & INT_DATA_OVER != 0 {
                break;
            } else {
                polls += 1;
                if polls == POLL_LIMIT {
                    return Err(Error::Timeout(status));
                }
            }
        }

        // The automatic stop has its own completion.
        if command == Command::ReadMultipleBlock {
            polls = 0;
            while regs.rintsts.get() & INT_ACD == 0 {
                polls += 1;
                if polls == POLL_LIMIT {
                    return Err(Error::Timeout(regs.rintsts.get()));
                }
            }
        }
        Ok(())
    }

    fn reset_fifo(&self) -> Result<(), Error> {
        let regs = regs();
        regs.ctrl.set(regs.ctrl.get() | CTRL_FIFO_RESET);
        let mut polls = 0;
        while regs.ctrl.get() & CTRL_FIFO_RESET != 0 {
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::Timeout(regs.rintsts.get()));
            }
        }
        Ok(())
    }

    /// Moves one FIFO word to `buffer` at `done`, dropping what does not fit. Returns the new
    /// position.
    fn pull_word(&self, buffer: &mut [u8], done: usize) -> usize {
        let mut word = [0; 8];
        let width = if self.fifo_64 {
            let (low, high): (u32, u32);
            // SAFETY: `fifo` is the data register of this block, device memory that stays
            // mapped. This is one 64-bit access, as the FIFO needs. `ldrd` needs the pair r2,
            // r3 here because it must be an even and the following odd register.
            unsafe {
                asm!(
                    "ldrd r2, r3, [{address}]",
                    address = in(reg) self.fifo,
                    out("r2") low,
                    out("r3") high,
                    options(nostack, preserves_flags),
                );
            }
            word[..4].copy_from_slice(&low.to_le_bytes());
            word[4..].copy_from_slice(&high.to_le_bytes());
            8
        } else {
            // SAFETY: as above, one 32-bit access.
            let value = unsafe { core::ptr::read_volatile(self.fifo as *const u32) };
            word[..4].copy_from_slice(&value.to_le_bytes());
            4
        };
        let left = buffer.len().saturating_sub(done);
        let count = left.min(width);
        buffer[done..done + count].copy_from_slice(&word[..count]);
        done + width
    }
}

/// The card's state from a status word: 4 is transfer, the state reads need. mmc.h:148,158.
pub const fn card_state(status: u32) -> u32 {
    (status >> R1_STATE_SHIFT) & R1_STATE_MASK
}
