//! Access to the board's eMMC, through the DW MMC controller `mshc_0`: reads, and writes only
//! through [`MmcWriter`], which keeps them away from what boots the board.
//!
//! # Commands
//!
//! This driver can send only these commands, all listed in its private `Command` type: `CMD8` (read
//! `EXT_CSD`), `CMD13` (read status), `CMD17` and `CMD18` (read blocks), `CMD24` and `CMD25`
//! (write blocks). The multiple block ones end with the controller's automatic `CMD12`. There
//! is no erase command, and no switch command (`CMD6`), so the user area is the only hardware
//! partition this driver can reach.
//!
//! # What writes cannot reach
//!
//! The board boots from the eMMC's first boot partition: `PARTITION_CONFIG` reads `0x48`, boot
//! enabled from boot partition 1, with acknowledge. That is where the boot ROM finds BL1, BL2 and
//! U-Boot, and without `CMD6` this driver cannot select it. In the user area, writes are refused,
//! by checks that do not depend on the caller:
//!
//! - in the first [`PROTECTED_BLOCKS`] blocks, before the first partition: the MBR, and room that
//!   Samsung's layouts keep for copies of the boot loaders;
//! - outside the one partition an [`MmcWriter`] was made for, from the table it reads itself;
//! - on any block of the partition table: the MBR and every extended boot record;
//! - in the extended partition itself, which only holds the logical ones and their records.
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
//! Two ways, both polled, without interrupts:
//!
//! - [`Mmc::read_blocks`]: the CPU reads the controller's FIFO. Caches need no maintenance.
//! - [`Mmc::read_blocks_dma`]: the controller's own DMA engine, the IDMAC, writes DRAM from a
//!   chain of descriptors. The buffer is dropped from the caches before and after
//!   ([`mmu::discard_dcache_range`], [`mmu::invalidate_dcache_range`]), so it must be whole cache
//!   lines in DRAM. Every return, error or not, stops the engine first, so nothing writes the
//!   buffer once the borrow ends.
//! - [`MmcWriter::write`]: the IDMAC reads DRAM, cleaned from the caches first
//!   ([`mmu::clean_dcache_range`]). Each write waits until the card has finished programming
//!   and is back in its transfer state, so data is on the card when it returns: the card's own
//!   write cache stays off, as nothing here turns it on.
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

use core::sync::atomic::{AtomicU32, Ordering};

use super::cmu::Cmu;
use crate::partition::{self, BlockRead, Partition};
use crate::{mmu, soc};

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
        (0x04C => fifoth: ReadWrite<u32>),
        (0x050 => _reserved4),
        /// dw_mmc.h:307.
        (0x06C => verid: ReadOnly<u32>),
        /// dw_mmc.h:308.
        (0x070 => hcon: ReadOnly<u32>),
        (0x074 => _reserved5),
        /// dw_mmc.h:311.
        (0x080 => bmod: ReadWrite<u32>),
        /// dw_mmc.h:312: any write makes the IDMAC read the descriptors again.
        (0x084 => pldmnd: ReadWrite<u32>),
        /// dw_mmc.h:313: the first descriptor.
        (0x088 => dbaddr: ReadWrite<u32>),
        /// dw_mmc.h:314.
        (0x08C => idsts: ReadWrite<u32>),
        /// dw_mmc.h:315.
        (0x090 => idinten: ReadWrite<u32>),
        /// dw_mmc.h:316: the descriptor the IDMAC is at.
        (0x094 => dscaddr: ReadOnly<u32>),
        /// dw_mmc.h:317: the buffer address it is at.
        (0x098 => bufaddr: ReadOnly<u32>),
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
/// dw_mmc.h:355.
const CTRL_DMA_RESET: u32 = 1 << 2;
/// dw_mmc.h:356.
const CTRL_FIFO_RESET: u32 = 1 << 1;
/// dw_mmc.h:360.
const CLKENA_ENABLE: u32 = 1 << 0;
/// dw_mmc.h:434.
const BMOD_IDMAC_ENABLE: u32 = 1 << 7;
/// dw_mmc.h:435: fixed bursts, as Linux sets.
const BMOD_FIXED_BURST: u32 = 1 << 1;
/// dw_mmc.h:436.
const BMOD_SWRESET: u32 = 1 << 0;

/// dw_mmc.h:426-432, the IDMAC's status: abnormal summary, normal summary, card error summary,
/// descriptor unavailable, fatal bus error, receive done, transmit done.
const IDSTS_AI: u32 = 1 << 9;
const IDSTS_NI: u32 = 1 << 8;
const IDSTS_CES: u32 = 1 << 5;
const IDSTS_DU: u32 = 1 << 4;
const IDSTS_FBE: u32 = 1 << 2;
const IDSTS_RI: u32 = 1 << 1;
const IDSTS_TI: u32 = 1 << 0;
/// `dw_mmc.c:58-61` `IDMAC_INT_CLR`.
const IDSTS_ALL: u32 = IDSTS_AI | IDSTS_NI | IDSTS_CES | IDSTS_DU | IDSTS_FBE | IDSTS_RI | IDSTS_TI;
const IDSTS_ERRORS: u32 = IDSTS_CES | IDSTS_DU | IDSTS_FBE;

/// `dw_mmc.c:86-103` `struct idmac_desc`, 32-bit addresses: control word bits.
const DES0_DIC: u32 = 1 << 1;
const DES0_LD: u32 = 1 << 2;
const DES0_FD: u32 = 1 << 3;
const DES0_CH: u32 = 1 << 4;
const DES0_OWN: u32 = 1 << 31;
/// `dw_mmc.c:106` `DW_MCI_DESC_DATA_LENGTH`: what one chained descriptor moves.
const DESCRIPTOR_BYTES: usize = 0x1000;
/// Descriptors in the chain. Linux has one page of them (`dw_mmc.c:63`), 256.
const DESCRIPTORS: usize = 256;
/// The most one command moves; longer reads take several.
const DMA_CHUNK_BYTES: usize = DESCRIPTORS * DESCRIPTOR_BYTES;
const DESCRIPTOR_WORDS: usize = 4;

/// dw_mmc.h:421 `SDMMC_GET_TRANS_MODE`, 0 being dw_mmc.h:417 `DMA_INTERFACE_IDMA`.
const HCON_TRANS_MODE_SHIFT: u32 = 16;
const HCON_TRANS_MODE_MASK: u32 = 0x3;
/// dw_mmc.h:424 `SDMMC_GET_ADDR_CONFIG`: 1 for 64-bit descriptors, which this does not build.
const HCON_ADDR_64: u32 = 1 << 27;

/// `exynos4412.dtsi`, `mshc_0`: `fifo-depth = <0x80>`, in FIFO words.
const FIFO_DEPTH: u32 = 0x80;

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
/// dw_mmc.h:402: the data goes to the card.
const CMD_DAT_WR: u32 = 1 << 10;
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
/// mmc.h:158 `R1_STATE_TRAN`.
const R1_STATE_TRAN: u32 = 4;
/// mmc.h:149 `R1_READY_FOR_DATA`.
const R1_READY_FOR_DATA: u32 = 1 << 8;

/// Blocks at the start of the user area that are never written: 10 MiB, up to where the first
/// partition, PARAM, starts on this board. [`MmcWriter::new`] also refuses a table whose first
/// partition starts before this.
pub const PROTECTED_BLOCKS: u32 = 20480;
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
    /// mmc.h:63 `MMC_WRITE_BLOCK`.
    WriteBlock,
    /// mmc.h:64 `MMC_WRITE_MULTIPLE_BLOCK`.
    WriteMultipleBlock,
}

impl Command {
    const fn flags(self) -> u32 {
        let (index, extra) = match self {
            Self::SendExtCsd => (8, CMD_DAT_EXP),
            Self::SendStatus => (13, 0),
            Self::ReadSingleBlock => (17, CMD_DAT_EXP),
            Self::ReadMultipleBlock => (18, CMD_DAT_EXP | CMD_SEND_STOP),
            Self::WriteBlock => (24, CMD_DAT_EXP | CMD_DAT_WR),
            Self::WriteMultipleBlock => (25, CMD_DAT_EXP | CMD_DAT_WR | CMD_SEND_STOP),
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
    /// The controller has no IDMAC, or one with 64-bit descriptors. Holds `HCON`.
    NoIdmac(u32),
    /// A DMA buffer that does not start on a cache line or is not in DRAM.
    BadBuffer,
    /// The IDMAC stopped on an error, or did not finish in time.
    Dma(DmaState),
    /// A write that would reach a protected block. Holds the block.
    Protected(u32),
    /// After a write, the card did not come back to its transfer state. Holds its last status.
    NotReady(u32),
}

/// Why an [`MmcWriter`] was not made, or refused a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The card or the controller failed.
    Card(Error),
    /// Reading the partition table failed.
    Table(partition::Error<Error>),
    /// The table has no partition of this number.
    NoPartition(u32),
    /// The extended partition holds only the logical ones and their records.
    Extended,
    /// The table puts a partition, or this one, before [`PROTECTED_BLOCKS`]. Holds the start.
    TableTooLow(u32),
    /// A table record lies inside the partition. Holds the record's block.
    RecordInside(u32),
    /// The card is not in its transfer state. Holds its status.
    NotTransfer(u32),
    /// A write past the end of the partition.
    OutOfRange,
}

/// Where a DMA read stood when it failed, as raw registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaState {
    /// `RINTSTS`.
    pub rintsts: u32,
    /// `IDSTS`.
    pub idsts: u32,
    /// `STATUS`: bits 29:17 count the words left in the FIFO.
    pub status: u32,
    /// `DSCADDR`: the descriptor the IDMAC was at.
    pub dscaddr: u32,
    /// `BUFADDR`: the address it was writing.
    pub bufaddr: u32,
    /// The descriptor chain's start, to compare with `dscaddr`.
    pub dbaddr: u32,
}

fn dma_state() -> DmaState {
    let regs = regs();
    DmaState {
        rintsts: regs.rintsts.get(),
        idsts: regs.idsts.get(),
        status: regs.status.get(),
        dscaddr: regs.dscaddr.get(),
        bufaddr: regs.bufaddr.get(),
        dbaddr: regs.dbaddr.get(),
    }
}

/// The descriptor chain. Only [`Mmc::read_blocks_dma`] touches it, one call at a time.
#[repr(C, align(32))]
struct Descriptors([AtomicU32; DESCRIPTORS * DESCRIPTOR_WORDS]);

static DESCRIPTORS_RING: Descriptors =
    Descriptors([const { AtomicU32::new(0) }; DESCRIPTORS * DESCRIPTOR_WORDS]);

/// `FIFOTH` for 512-byte blocks: `dw_mmc.c` `dw_mci_adjust_fifoth`. The burst is the largest
/// of 1 to 256 words that divides both a block and the free half of the FIFO, and the receive
/// watermark is one less than it.
const fn fifoth_for_blocks(fifo_width: u32) -> u32 {
    const SIZES: [u32; 8] = [1, 4, 8, 16, 32, 64, 128, 256];
    let block_words = BLOCK_SIZE as u32 / fifo_width;
    let tx_wmark = FIFO_DEPTH / 2;
    let tx_free = FIFO_DEPTH - tx_wmark;
    let mut index = SIZES.len() - 1;
    let (mut msize, mut rx_wmark) = (0, 1);
    while index > 0 {
        if block_words.is_multiple_of(SIZES[index]) && tx_free.is_multiple_of(SIZES[index]) {
            msize = index as u32;
            rx_wmark = SIZES[index] - 1;
            break;
        }
        index -= 1;
    }
    // dw_mmc.h:413 `SDMMC_SET_FIFOTH`.
    (msize & 0x7) << 28 | (rx_wmark & 0xFFF) << 16 | (tx_wmark & 0xFFF)
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
    dma_fifoth: u32,
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
        let fifo_64 = hdata == HDATA_64;
        Ok(Self {
            fifo: soc::MSHC_BASE + offset,
            fifo_64,
            dma_fifoth: fifoth_for_blocks(if fifo_64 { 8 } else { 4 }),
        })
    }

    /// The `FIFOTH` value DMA reads set: burst size and watermarks.
    pub const fn dma_fifoth(&self) -> u32 {
        self.dma_fifoth
    }

    /// Sets another `FIFOTH` for DMA reads, to compare settings. `mmc_test` uses it.
    #[doc(hidden)]
    pub const fn set_dma_fifoth(&mut self, value: u32) {
        self.dma_fifoth = value;
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
        wait_not_busy()?;
        self.reset_fifo()?;
        regs.blksiz.set(BLOCK_SIZE as u32);
        regs.bytcnt.set(buffer.len() as u32);

        self.command(command, argument)?;

        let mut done = 0;
        let mut polls = 0;
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

        if command == Command::ReadMultipleBlock {
            wait_auto_stop()?;
        }
        Ok(())
    }

    fn reset_fifo(&self) -> Result<(), Error> {
        reset_ctrl(CTRL_FIFO_RESET)
    }

    /// Reads whole blocks into `buffer`, from block `lba` of the user area, as
    /// [`read_blocks`](Self::read_blocks) does, but through the IDMAC. `buffer` must start on a
    /// cache line ([`mmu::DCACHE_LINE`]) and lie in DRAM, where addresses are physical ones.
    /// Reads of more than 1 MiB take one command per MiB.
    pub fn read_blocks_dma(&self, lba: u32, buffer: &mut [u8]) -> Result<(), Error> {
        check_dma_buffer(buffer)?;
        let mut lba = lba;
        for chunk in buffer.chunks_mut(DMA_CHUNK_BYTES) {
            // Before the IDMAC writes: no dirty line of the buffer may be written over it.
            mmu::discard_dcache_range(chunk.as_ptr() as usize, chunk.len());
            let result = self.transfer_dma(lba, chunk.as_ptr() as usize, chunk.len(), Direction::Read);
            // Whatever happened, the engine is stopped by now, and the CPU must read DRAM.
            mmu::invalidate_dcache_range(chunk.as_ptr() as usize, chunk.len());
            result?;
            lba += (chunk.len() / BLOCK_SIZE) as u32;
        }
        Ok(())
    }

    /// Writes whole blocks from `data` from block `lba` on. Only [`MmcWriter`] calls it, after
    /// its own checks; this one refuses the protected start of the user area by itself too.
    fn write_blocks_dma(&self, lba: u32, data: &[u8], rca: u16) -> Result<(), Error> {
        check_dma_buffer(data)?;
        if lba < PROTECTED_BLOCKS {
            return Err(Error::Protected(lba));
        }
        let mut lba = lba;
        for chunk in data.chunks(DMA_CHUNK_BYTES) {
            // The IDMAC reads DRAM: what the CPU wrote must be there.
            mmu::clean_dcache_range(chunk.as_ptr() as usize, chunk.len());
            self.transfer_dma(lba, chunk.as_ptr() as usize, chunk.len(), Direction::Write)?;
            self.wait_programmed(rca)?;
            lba += (chunk.len() / BLOCK_SIZE) as u32;
        }
        Ok(())
    }

    /// After a write: the card holds `DAT0` low while it programs, then must be back in its
    /// transfer state, ready for data, as Linux's `mmc_blk_card_busy` waits for.
    fn wait_programmed(&self, rca: u16) -> Result<(), Error> {
        wait_not_busy()?;
        let mut status = 0;
        for _ in 0..POLL_LIMIT / 1000 {
            status = self.read_status(rca)?;
            if status & R1_READY_FOR_DATA != 0 && card_state(status) == R1_STATE_TRAN {
                return Ok(());
            }
        }
        Err(Error::NotReady(status))
    }

    /// One command's worth: at most [`DMA_CHUNK_BYTES`] at `start`, cache maintenance done by
    /// the caller. In the order of Linux 5.15.198 `dw_mci_submit_data_dma` and
    /// `dw_mci_idmac_start_dma`: the data registers, `FIFOTH`, the DMA interface, the
    /// descriptors, the IDMAC, then the command. Stops the IDMAC on every return.
    fn transfer_dma(&self, lba: u32, start: usize, length: usize, direction: Direction) -> Result<(), Error> {
        let regs = regs();
        wait_not_busy()?;
        self.reset_fifo()?;
        regs.blksiz.set(BLOCK_SIZE as u32);
        regs.bytcnt.set(length as u32);
        regs.fifoth.set(self.dma_fifoth);

        let count = length.div_ceil(DESCRIPTOR_BYTES);
        for index in 0..count {
            let bytes = (length - index * DESCRIPTOR_BYTES).min(DESCRIPTOR_BYTES);
            let last = index == count - 1;
            let mut control = DES0_OWN;
            if index == 0 {
                control |= DES0_FD;
            }
            // `dw_mci_prepare_desc32`: all but the last are chained, without their interrupt.
            control |= if last { DES0_LD } else { DES0_CH | DES0_DIC };
            let words = &DESCRIPTORS_RING.0[index * DESCRIPTOR_WORDS..][..DESCRIPTOR_WORDS];
            words[0].store(control, Ordering::Relaxed);
            words[1].store(bytes as u32, Ordering::Relaxed);
            words[2].store((start + index * DESCRIPTOR_BYTES) as u32, Ordering::Relaxed);
            words[3].store(words[DESCRIPTOR_WORDS..].as_ptr() as u32, Ordering::Relaxed);
        }
        let ring = DESCRIPTORS_RING.0.as_ptr() as usize;
        let ring_bytes = (count * DESCRIPTOR_WORDS * 4).next_multiple_of(mmu::DCACHE_LINE);
        mmu::clean_dcache_range(ring, ring_bytes);

        let result = self.run_dma(lba, length, ring, direction);
        stop_dma();
        result
    }

    fn run_dma(&self, lba: u32, length: usize, ring: usize, direction: Direction) -> Result<(), Error> {
        let regs = regs();
        regs.ctrl.set(regs.ctrl.get() | CTRL_DMA_ENABLE);
        reset_ctrl(CTRL_DMA_RESET)?;
        regs.bmod.set(regs.bmod.get() | BMOD_SWRESET);
        // As Linux enables them (`dw_mci_idmac_init`). With `CTRL_INT_ENABLE` off, no interrupt
        // leaves the controller.
        regs.idinten.set(IDSTS_NI | IDSTS_RI | IDSTS_TI);
        regs.idsts.set(IDSTS_ALL);
        regs.dbaddr.set(ring as u32);
        regs.ctrl.set(regs.ctrl.get() | CTRL_USE_IDMAC);
        regs.bmod.set(regs.bmod.get() | BMOD_IDMAC_ENABLE | BMOD_FIXED_BURST);
        regs.pldmnd.set(1);

        let single = length == BLOCK_SIZE;
        let (command, dma_done) = match direction {
            Direction::Read if single => (Command::ReadSingleBlock, IDSTS_RI),
            Direction::Read => (Command::ReadMultipleBlock, IDSTS_RI),
            Direction::Write if single => (Command::WriteBlock, IDSTS_TI),
            Direction::Write => (Command::WriteMultipleBlock, IDSTS_TI),
        };
        self.command(command, lba)?;

        // Done when the data has crossed: the controller's transfer is over, and the IDMAC has
        // finished its side of it.
        let mut polls = 0;
        loop {
            let status = regs.rintsts.get();
            let dma = regs.idsts.get();
            if status & DATA_ERRORS != 0 {
                return Err(Error::Data(status));
            }
            if dma & IDSTS_ERRORS != 0 {
                return Err(Error::Dma(dma_state()));
            }
            if status & INT_DATA_OVER != 0 && dma & dma_done != 0 {
                break;
            }
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::Dma(dma_state()));
            }
        }
        if !single {
            wait_auto_stop()?;
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

/// Which way a DMA transfer goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// From the card to DRAM.
    Read,
    /// From DRAM to the card.
    Write,
}

/// A DMA buffer must be whole blocks, start on a cache line and lie in DRAM, where addresses
/// are physical ones, and the controller must have a 32-bit IDMAC.
fn check_dma_buffer(buffer: &[u8]) -> Result<(), Error> {
    if buffer.is_empty() || !buffer.len().is_multiple_of(BLOCK_SIZE) || buffer.len() > u32::MAX as usize {
        return Err(Error::BadLength);
    }
    let start = buffer.as_ptr() as usize;
    let in_dram = start >= soc::DRAM_BASE && start + buffer.len() <= soc::DRAM_BASE + soc::DRAM_SIZE;
    if !start.is_multiple_of(mmu::DCACHE_LINE) || !in_dram {
        return Err(Error::BadBuffer);
    }
    let hcon = regs().hcon.get();
    if (hcon >> HCON_TRANS_MODE_SHIFT) & HCON_TRANS_MODE_MASK != 0 || hcon & HCON_ADDR_64 != 0 {
        return Err(Error::NoIdmac(hcon));
    }
    Ok(())
}

/// The partition table reader for [`MmcWriter::new`]: single blocks, polled.
struct TableReader(Mmc);

impl BlockRead for TableReader {
    type Error = Error;

    fn read_block(&self, lba: u32, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), Error> {
        self.0.read_blocks(lba, buffer)
    }
}

/// Partitions [`MmcWriter::new`] reads at most: this board has 10.
const MAX_PARTITIONS: usize = 32;

/// Writes to one partition of the user area, and nowhere else.
///
/// It reads the partition table from the card itself, so the bounds do not come from the
/// caller. See the module documentation for what it refuses.
#[derive(Clone, Copy, Debug)]
pub struct MmcWriter {
    card: Mmc,
    rca: u16,
    partition: Partition,
}

impl MmcWriter {
    /// A writer for partition `number`, on the card at address `rca`, which must be in its
    /// transfer state ([`Mmc::read_status`] finds the address).
    pub fn new(card: Mmc, rca: u16, number: u32) -> Result<Self, WriteError> {
        let status = card.read_status(rca).map_err(WriteError::Card)?;
        if card_state(status) != R1_STATE_TRAN {
            return Err(WriteError::NotTransfer(status));
        }
        let mut table = [Partition::default(); MAX_PARTITIONS];
        let found = partition::scan(&TableReader(card), &mut table).map_err(WriteError::Table)?;
        let table = &table[..found];
        if let Some(low) = table.iter().map(|partition| partition.start).find(|start| *start < PROTECTED_BLOCKS) {
            return Err(WriteError::TableTooLow(low));
        }
        let partition =
            *table.iter().find(|partition| partition.number == number).ok_or(WriteError::NoPartition(number))?;
        if partition.is_extended() {
            return Err(WriteError::Extended);
        }
        let end = u64::from(partition.start) + u64::from(partition.count);
        // Every record, primary or logical, must be outside: block 0 is below the protected
        // limit already, and each extended boot record sits before its logical partition.
        if let Some(record) = table
            .iter()
            .map(|other| other.record)
            .find(|record| u64::from(*record) >= u64::from(partition.start) && u64::from(*record) < end)
        {
            return Err(WriteError::RecordInside(record));
        }
        Ok(Self { card, rca, partition })
    }

    /// The partition it writes, as the table on the card gives it.
    pub const fn partition(&self) -> Partition {
        self.partition
    }

    /// Writes whole blocks from `data`, from block `block` of the partition on, counted from
    /// its start. `data` must start on a cache line ([`mmu::DCACHE_LINE`]) and lie in DRAM.
    /// Returns once the card has programmed them.
    pub fn write(&self, block: u32, data: &[u8]) -> Result<(), WriteError> {
        let blocks = (data.len() / BLOCK_SIZE) as u64;
        if u64::from(block) + blocks > u64::from(self.partition.count) {
            return Err(WriteError::OutOfRange);
        }
        self.card.write_blocks_dma(self.partition.start + block, data, self.rca).map_err(WriteError::Card)
    }
}

fn wait_not_busy() -> Result<(), Error> {
    let regs = regs();
    let mut polls = 0;
    while regs.status.get() & STATUS_BUSY != 0 {
        polls += 1;
        if polls == POLL_LIMIT {
            return Err(Error::Timeout(regs.rintsts.get()));
        }
    }
    Ok(())
}

/// The automatic stop after a multiple block read has its own completion.
fn wait_auto_stop() -> Result<(), Error> {
    let regs = regs();
    let mut polls = 0;
    while regs.rintsts.get() & INT_ACD == 0 {
        polls += 1;
        if polls == POLL_LIMIT {
            return Err(Error::Timeout(regs.rintsts.get()));
        }
    }
    Ok(())
}

/// Sets reset bits in `CTRL` and waits for them to clear: `dw_mci_ctrl_reset`.
fn reset_ctrl(bits: u32) -> Result<(), Error> {
    let regs = regs();
    regs.ctrl.set(regs.ctrl.get() | bits);
    let mut polls = 0;
    while regs.ctrl.get() & bits != 0 {
        polls += 1;
        if polls == POLL_LIMIT {
            return Err(Error::Timeout(regs.rintsts.get()));
        }
    }
    Ok(())
}

/// Takes the controller back from the IDMAC: `dw_mci_idmac_stop_dma`, then the DMA interface
/// off, as [`Mmc::new`] leaves it for polled reads.
fn stop_dma() {
    let regs = regs();
    regs.ctrl.set((regs.ctrl.get() & !(CTRL_USE_IDMAC | CTRL_DMA_ENABLE)) | CTRL_DMA_RESET);
    regs.bmod.set((regs.bmod.get() & !(BMOD_IDMAC_ENABLE | BMOD_FIXED_BURST)) | BMOD_SWRESET);
    // A reset that does not finish leaves nothing better to do: the next transfer's own reset
    // reports it.
    let mut polls = 0;
    while regs.ctrl.get() & CTRL_DMA_RESET != 0 && polls < POLL_LIMIT {
        polls += 1;
    }
}

/// The card's state from a status word: 4 is transfer, the state reads need. mmc.h:148,158.
pub const fn card_state(status: u32) -> u32 {
    (status >> R1_STATE_SHIFT) & R1_STATE_MASK
}
