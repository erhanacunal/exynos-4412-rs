//! Driver for the Samsung UART: polled, and the receive interrupt that the `embassy` feature's
//! async reader uses.
//!
//! Clocks, pins, line settings and the baud rate are inherited from U-Boot. The
//! FIFO may be on or off: each status check follows the current `UFCON` setting.
//!
//! Every offset below is verified against Linux 5.15.198
//! `include/linux/serial_s3c.h`, cited as `serial_s3c.h:<line>`.

use core::fmt;

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite, WriteOnly};
use tock_registers::{register_bitfields, register_structs};

use crate::soc;

register_bitfields![u32,
    UFCON [
        /// serial_s3c.h:128 `S3C2410_UFCON_FIFOMODE`.
        FIFO_ENABLE OFFSET(0) NUMBITS(1) []
    ],
    UTRSTAT [
        /// serial_s3c.h:182 `S3C2410_UTRSTAT_RXDR`.
        RX_DATA_READY OFFSET(0) NUMBITS(1) [],
        /// serial_s3c.h:181 `S3C2410_UTRSTAT_TXFE`.
        TX_BUFFER_EMPTY OFFSET(1) NUMBITS(1) [],
        /// serial_s3c.h:180 `S3C2410_UTRSTAT_TXE`: holding buffer and shifter both empty.
        TX_EMPTY OFFSET(2) NUMBITS(1) []
    ],
    // The Exynos layout: `samsung_tty.c` EXYNOS_COMMON_SERIAL_DRV_DATA_USI selects S5PV210_UFSTAT_*.
    UFSTAT [
        /// serial_s3c.h:238 `S5PV210_UFSTAT_RXMASK`.
        RX_COUNT OFFSET(0) NUMBITS(8) [],
        /// serial_s3c.h:235 `S5PV210_UFSTAT_RXFULL`.
        RX_FULL OFFSET(8) NUMBITS(1) [],
        /// serial_s3c.h:234 `S5PV210_UFSTAT_TXFULL`.
        TX_FULL OFFSET(24) NUMBITS(1) []
    ]
];

register_structs! {
    Registers {
        /// serial_s3c.h:21 `S3C2410_ULCON`.
        (0x00 => ulcon: ReadOnly<u32>),
        /// serial_s3c.h:22 `S3C2410_UCON`.
        (0x04 => ucon: ReadWrite<u32>),
        /// serial_s3c.h:23 `S3C2410_UFCON`.
        (0x08 => ufcon: ReadOnly<u32, UFCON::Register>),
        (0x0C => _reserved0),
        /// serial_s3c.h:26 `S3C2410_UTRSTAT`.
        (0x10 => utrstat: ReadOnly<u32, UTRSTAT::Register>),
        /// serial_s3c.h:27 `S3C2410_UERSTAT`: receive errors, cleared by the read.
        (0x14 => uerstat: ReadOnly<u32>),
        /// serial_s3c.h:28 `S3C2410_UFSTAT`.
        (0x18 => ufstat: ReadOnly<u32, UFSTAT::Register>),
        (0x1C => _reserved2),
        /// serial_s3c.h:20 `S3C2410_UTXH`. Byte access: the Exynos nodes have no
        /// `reg-io-width`, so `samsung_tty.c:wr_reg` uses UPIO_MEM (writeb) on them.
        (0x20 => utxh: WriteOnly<u8>),
        (0x21 => _reserved3),
        /// serial_s3c.h:19 `S3C2410_URXH`. Byte access, see `utxh`.
        (0x24 => urxh: ReadOnly<u8>),
        (0x25 => _reserved4),
        /// serial_s3c.h:25 `S3C2410_UBRDIV`.
        (0x28 => ubrdiv: ReadWrite<u32>),
        /// serial_s3c.h:196 `S3C2443_DIVSLOT`; `samsung_tty.c` sets `has_fracval` on
        /// Exynos, which makes this the UFRACVAL register.
        (0x2C => ufracval: ReadWrite<u32>),
        /// serial_s3c.h:199 `S3C64XX_UINTP`: pending interrupts, cleared by writing 1.
        (0x30 => uintp: ReadWrite<u32>),
        /// serial_s3c.h:200 `S3C64XX_UINTSP`: interrupt sources, before the mask.
        (0x34 => uintsp: ReadOnly<u32>),
        /// serial_s3c.h:201 `S3C64XX_UINTM`: a set bit masks that interrupt.
        (0x38 => uintm: ReadWrite<u32>),
        (0x3C => @END),
    }
}

/// serial_s3c.h:206-208 `S3C64XX_UINTM_RXD_MSK`, `_ERR_MSK`, `_TXD_MSK`: the receive, error and
/// transmit bits of `UINTP`, `UINTSP` and `UINTM`. Bit 3 is the modem interrupt.
const INT_RXD: u32 = 1 << 0;
const INT_ERROR: u32 = 1 << 1;
const INT_TXD: u32 = 1 << 2;
const INT_ALL: u32 = 0xF;
/// serial_s3c.h:92 `S3C2410_UCON_RXIRQMODE`: bits 1:0 of `UCON` select interrupt or polling
/// mode for receiving, which are the same mode here; 0 turns receiving off.
const UCON_RX_MODE_MASK: u32 = 0b11;
const UCON_RX_MODE_IRQ: u32 = 0b01;
/// serial_s3c.h:93 `S3C2410_UCON_RXFIFO_TOI`: an interrupt also when bytes wait in the FIFO
/// below its trigger level and the line has gone quiet.
const UCON_RX_TIMEOUT: u32 = 1 << 7;

/// One of the five UART blocks of the SoC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UartId {
    /// UART0. Wired to the Bluetooth chip on the Mehmet board.
    Uart0 = 0,
    /// UART1.
    Uart1 = 1,
    /// UART2, the board's debug console.
    Uart2 = 2,
    /// UART3.
    Uart3 = 3,
    /// UART4. Not described in the Linux 5.15 device tree, see [`soc::UART4_BASE`].
    Uart4 = 4,
}

impl UartId {
    /// Physical base address of this UART's registers.
    pub const fn base(self) -> usize {
        match self {
            Self::Uart0 => soc::UART0_BASE,
            Self::Uart1 => soc::UART1_BASE,
            Self::Uart2 => soc::UART2_BASE,
            Self::Uart3 => soc::UART3_BASE,
            Self::Uart4 => soc::UART4_BASE,
        }
    }

    /// The GIC interrupt ID: `exynos4.dtsi`, `GIC_SPI 52` to `55` for `serial_0` to `serial_3`.
    /// `None` for UART4, which the device tree does not describe.
    pub const fn irq(self) -> Option<u16> {
        match self {
            Self::Uart4 => None,
            other => Some(32 + 52 + other as u16),
        }
    }

    /// Converts a raw index, returning `None` when it is out of range.
    pub const fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::Uart0),
            1 => Some(Self::Uart1),
            2 => Some(Self::Uart2),
            3 => Some(Self::Uart3),
            4 => Some(Self::Uart4),
            _ => None,
        }
    }
}

/// Raw line configuration registers, as left behind by the bootloader.
#[derive(Clone, Copy, Debug)]
pub struct UartConfig {
    /// Line control: word length, parity, stop bits.
    pub ulcon: u32,
    /// Control: TX/RX modes and clock selection.
    pub ucon: u32,
    /// FIFO control.
    pub ufcon: u32,
    /// Integer part of the baud divisor.
    pub ubrdiv: u32,
    /// Fractional part of the baud divisor.
    pub ufracval: u32,
}

/// Handle to one UART.
///
/// A handle holds no state, so several may refer to the same UART. Concurrent
/// users can only interleave their bytes.
#[derive(Clone, Copy, Debug)]
pub struct Uart {
    id: UartId,
}

impl Uart {
    /// Creates a handle. Accesses are valid only once the UART is clocked, which
    /// U-Boot has done for its console UART.
    pub const fn new(id: UartId) -> Self {
        Self { id }
    }

    /// The UART this handle refers to.
    pub const fn id(&self) -> UartId {
        self.id
    }

    fn regs(&self) -> &Registers {
        // SAFETY: `base()` is the MMIO block of a UART, device memory that stays
        // mapped for the whole program, and `Registers` is `repr(C)` with interior
        // mutable cells only, so shared references to it are sound.
        unsafe { &*(self.id.base() as *const Registers) }
    }

    fn fifo_enabled(&self) -> bool {
        self.regs().ufcon.is_set(UFCON::FIFO_ENABLE)
    }

    /// Returns whether [`try_write_byte`](Self::try_write_byte) would accept a byte.
    pub fn tx_ready(&self) -> bool {
        if self.fifo_enabled() {
            !self.regs().ufstat.is_set(UFSTAT::TX_FULL)
        } else {
            self.regs().utrstat.is_set(UTRSTAT::TX_BUFFER_EMPTY)
        }
    }

    /// Returns whether [`try_read_byte`](Self::try_read_byte) would return a byte.
    pub fn rx_ready(&self) -> bool {
        if self.fifo_enabled() {
            let ufstat = self.regs().ufstat.extract();
            ufstat.read(UFSTAT::RX_COUNT) != 0 || ufstat.is_set(UFSTAT::RX_FULL)
        } else {
            self.regs().utrstat.is_set(UTRSTAT::RX_DATA_READY)
        }
    }

    /// Sends `byte` if there is room, returning whether it was accepted.
    pub fn try_write_byte(&self, byte: u8) -> bool {
        let ready = self.tx_ready();
        if ready {
            self.regs().utxh.set(byte);
        }
        ready
    }

    /// Sends `byte`, waiting for room. No newline translation is applied.
    pub fn write_byte(&self, byte: u8) {
        while !self.try_write_byte(byte) {
            core::hint::spin_loop();
        }
    }

    /// Returns a received byte, if any.
    pub fn try_read_byte(&self) -> Option<u8> {
        self.rx_ready().then(|| self.regs().urxh.get())
    }

    /// Waits for a received byte.
    pub fn read_byte(&self) -> u8 {
        loop {
            if let Some(byte) = self.try_read_byte() {
                return byte;
            }
            core::hint::spin_loop();
        }
    }

    /// Waits until every queued byte has left the shift register.
    pub fn flush(&self) {
        while !self.regs().utrstat.is_set(UTRSTAT::TX_EMPTY) {
            core::hint::spin_loop();
        }
    }

    /// Turns on the receive interrupt, at the FIFO's trigger level and after a quiet line with
    /// bytes below it, and masks the UART's others. Keeps the other `UCON` bits; clears what
    /// was pending.
    ///
    /// U-Boot leaves `UINTM` at 0, everything unmasked: the transmit interrupt is then pending
    /// whenever the transmit FIFO is low, and would come back at once after every return.
    pub fn enable_rx_interrupt(&self) {
        let regs = self.regs();
        regs.uintm.set(INT_ALL);
        let ucon = (regs.ucon.get() & !UCON_RX_MODE_MASK) | UCON_RX_MODE_IRQ | UCON_RX_TIMEOUT;
        regs.ucon.set(ucon);
        regs.uintp.set(INT_ALL);
        regs.uintm.set(INT_ALL & !INT_RXD);
    }

    /// Masks every interrupt of this UART, the transmit one too, and clears what was pending.
    /// What U-Boot leaves, `UINTM` at 0, would raise the transmit interrupt at once.
    pub fn mask_interrupts(&self) {
        let regs = self.regs();
        regs.uintm.set(INT_ALL);
        regs.uintp.set(INT_ALL);
    }

    /// Unmasks or masks the transmit interrupt, leaving the others. It is pending whenever the
    /// transmit FIFO holds no more than its trigger level, `UFCON` bits 10:8: unmasked with
    /// nothing to send, it comes back after every return.
    pub fn set_tx_interrupt(&self, on: bool) {
        let regs = self.regs();
        let mask = regs.uintm.get();
        regs.uintm.set(if on { mask & !INT_TXD } else { mask | INT_TXD });
    }

    /// Whether the transmit interrupt is pending.
    pub fn tx_interrupt_pending(&self) -> bool {
        self.regs().uintp.get() & INT_TXD != 0
    }

    /// Clears a pending transmit interrupt, after filling the FIFO: with the FIFO still low it
    /// becomes pending again.
    pub fn clear_tx_interrupt(&self) {
        self.regs().uintp.set(INT_TXD);
    }

    /// Masks the receive interrupt again, with the others.
    pub fn disable_rx_interrupt(&self) {
        let regs = self.regs();
        regs.uintm.set(INT_ALL);
    }

    /// Clears a pending receive or error interrupt, before reading the FIFO: a byte that comes
    /// after raises it again.
    pub fn clear_rx_interrupt(&self) {
        self.regs().uintp.set(INT_RXD | INT_ERROR);
    }

    /// The receive errors since the last call, as `UERSTAT` bits: 0 overrun, 1 parity,
    /// 2 frame, 3 break (serial_s3c.h:184-187). Reading clears them.
    pub fn take_errors(&self) -> u32 {
        self.regs().uerstat.get() & 0xF
    }

    /// Raw `UINTP`, `UINTSP` and `UINTM`.
    pub fn interrupt_registers(&self) -> (u32, u32, u32) {
        let regs = self.regs();
        (regs.uintp.get(), regs.uintsp.get(), regs.uintm.get())
    }

    /// Reads the line configuration inherited from the bootloader.
    pub fn config(&self) -> UartConfig {
        let regs = self.regs();
        UartConfig {
            ulcon: regs.ulcon.get(),
            ucon: regs.ucon.get(),
            ufcon: regs.ufcon.get(),
            ubrdiv: regs.ubrdiv.get(),
            ufracval: regs.ufracval.get(),
        }
    }

    /// Programs the baud rate from the given UART source clock.
    ///
    /// # Panics
    ///
    /// Always: this is not implemented, U-Boot's settings are inherited instead.
    pub fn init_baudrate(&self, _baud: u32, _source_clock_hz: u32) {
        // TODO: derive UBRDIV and UFRACVAL once the CMU driver can report the source clock.
        todo!("baud rate programming");
    }
}

/// Writes text, translating `\n` to `\r\n`.
impl fmt::Write for Uart {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}
