//! Polled master driver for the I2C controllers 0 to 7.
//!
//! Register map and bits are verified against Linux 5.15.198
//! `drivers/i2c/busses/i2c-s3c2410.c`, cited as `i2c-s3c2410.c:<line>`. The nodes
//! `i2c_0`..`i2c_7` of `exynos4.dtsi` use `samsung,s3c2440-i2c`, so this layout with `IICLC`.
//!
//! ```ignore
//! let bus = I2c::new(Bus::I2c3);
//! bus.init(100_000, 100)?;
//! let mut value = [0; 2];
//! bus.write_read(0x36, &[0x02], &mut value)?;
//! ```
//!
//! # Limits
//!
//! - The controller's interrupt is on while a transfer runs, because `i2c-s3c2410.c:708`
//!   turns it on for its polled mode too. Do not enable the bus's interrupt in the GIC.
//! - A device holding a line low stays that way: there is no bus recovery. The call returns
//!   an error.
//! - Each timeout is a count of register reads, not a time: see [`POLL_LIMIT`].
//! - `i2c_8` is left out. It is the HDMI PHY's bus, with different stop behaviour
//!   (`i2c-s3c2410.c:328`) and `samsung,s3c2440-hdmiphy-i2c` in `exynos4.dtsi:600`.
//!
//! # Concurrency
//!
//! Use each bus from one context at a time, including interrupt handlers and other cores.
//! [`init`](I2c::init) also changes pins and a clock gate: see their modules.

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::register_structs;
use tock_registers::registers::ReadWrite;

use crate::drivers::cmu::Cmu;
use crate::drivers::gpio::{self, Function, Pin, Pull};
use crate::soc;

register_structs! {
    Registers {
        /// i2c-s3c2410.c:39 `S3C2410_IICCON`.
        (0x00 => con: ReadWrite<u32>),
        /// i2c-s3c2410.c:40 `S3C2410_IICSTAT`.
        (0x04 => stat: ReadWrite<u32>),
        /// i2c-s3c2410.c:41 `S3C2410_IICADD`, written as a byte at i2c-s3c2410.c:989.
        (0x08 => add: ReadWrite<u8>),
        (0x09 => _reserved0),
        /// i2c-s3c2410.c:42 `S3C2410_IICDS`, accessed as a byte by i2c-s3c2410.c:273.
        (0x0C => ds: ReadWrite<u8>),
        (0x0D => _reserved1),
        /// i2c-s3c2410.c:43 `S3C2440_IICLC`.
        (0x10 => lc: ReadWrite<u32>),
        (0x14 => @END),
    }
}

/// i2c-s3c2410.c:45.
const CON_ACK: u32 = 1 << 7;
/// i2c-s3c2410.c:47: the prescaler divides by 512 when set, by 16 otherwise.
const CON_DIV_512: u32 = 1 << 6;
/// i2c-s3c2410.c:48.
const CON_IRQ_ENABLE: u32 = 1 << 5;
/// i2c-s3c2410.c:49. Set: a byte is done and the bus is held. Writing 0 releases it.
const CON_PENDING: u32 = 1 << 4;
/// i2c-s3c2410.c:51: largest prescaler field.
const CON_SCALE_MAX: u32 = 0xF;

/// i2c-s3c2410.c:53.
const STAT_MASTER_RX: u32 = 2 << 6;
/// i2c-s3c2410.c:54.
const STAT_MASTER_TX: u32 = 3 << 6;
/// i2c-s3c2410.c:57.
const STAT_MODE_MASK: u32 = 3 << 6;
/// i2c-s3c2410.c:59-60: written, starts or (cleared) stops; read, the bus is busy.
const STAT_START_BUSY: u32 = 1 << 5;
/// i2c-s3c2410.c:61.
const STAT_ENABLE: u32 = 1 << 4;
/// i2c-s3c2410.c:62.
const STAT_ARBITRATION_LOST: u32 = 1 << 3;
/// i2c-s3c2410.c:65. Set after a byte: no acknowledge came.
const STAT_NO_ACK: u32 = 1 << 0;

/// i2c-s3c2410.c:73.
const LC_FILTER: u32 = 1 << 2;
/// Bits 1:0 of `IICLC` count units of this many clocks: i2c-s3c2410.c:67-70,874-876.
const LC_CLOCKS_PER_UNIT: u64 = 5;
/// i2c-s3c2410.c:877-878.
const LC_UNITS_MAX: u64 = 3;

/// Own address as a target, never used: `samsung,i2c-slave-addr` of every bus in
/// `exynos4412-mehmet.dts`.
const OWN_ADDRESS: u8 = 0x10;
/// Register reads to wait for a byte or a stop, far longer than any byte at a rate this
/// driver sets (the reads take at least tens of nanoseconds each), to allow for a device
/// stretching the clock. Not measured.
pub const POLL_LIMIT: u32 = 1_000_000;
/// Loop rounds for the data to reach the bus before it is released, at least the 50 ns of
/// i2c-s3c2410.c:1086 at the fastest clock of this chip.
const SETUP_ROUNDS: u32 = 200;
/// Highest 7-bit address.
const ADDRESS_MAX: u8 = 0x7F;

/// An I2C controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    /// `i2c_0`, `exynos4.dtsi:493`, pins `exynos4412-pinctrl.dtsi:335`.
    I2c0,
    /// `i2c_1`, `exynos4.dtsi:506`, pins `exynos4412-pinctrl.dtsi:349`.
    I2c1,
    /// `i2c_2`, `exynos4.dtsi:519`, pins `exynos4412-pinctrl.dtsi:155`.
    I2c2,
    /// `i2c_3`, `exynos4.dtsi:532`, pins `exynos4412-pinctrl.dtsi:183`.
    I2c3,
    /// `i2c_4`, `exynos4.dtsi:545`, pins `exynos4412-pinctrl.dtsi:211`.
    I2c4,
    /// `i2c_5`, `exynos4.dtsi:558`, pins `exynos4412-pinctrl.dtsi:225`.
    I2c5,
    /// `i2c_6`, `exynos4.dtsi:571`, pins `exynos4412-pinctrl.dtsi:279`.
    I2c6,
    /// `i2c_7`, `exynos4.dtsi:584`, pins `exynos4412-pinctrl.dtsi:314`.
    I2c7,
}

impl Bus {
    const fn index(self) -> usize {
        self as usize
    }

    /// The two pins of the `samsung,pins` line, and their function.
    const fn pins(self) -> ([Pin; 2], u8) {
        match self {
            Self::I2c0 => ([gpio::GPD1.pin(0), gpio::GPD1.pin(1)], 2),
            Self::I2c1 => ([gpio::GPD1.pin(2), gpio::GPD1.pin(3)], 2),
            Self::I2c2 => ([gpio::GPA0.pin(6), gpio::GPA0.pin(7)], 3),
            Self::I2c3 => ([gpio::GPA1.pin(2), gpio::GPA1.pin(3)], 3),
            Self::I2c4 => ([gpio::GPB.pin(0), gpio::GPB.pin(1)], 3),
            Self::I2c5 => ([gpio::GPB.pin(2), gpio::GPB.pin(3)], 3),
            Self::I2c6 => ([gpio::GPC1.pin(3), gpio::GPC1.pin(4)], 4),
            Self::I2c7 => ([gpio::GPD0.pin(2), gpio::GPD0.pin(3)], 3),
        }
    }
}

/// Why an I2C call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No device answered its address.
    AddressNack,
    /// The device refused a data byte.
    DataNack,
    /// The bus was busy before the transfer.
    BusBusy,
    /// A byte or a stop did not finish within [`POLL_LIMIT`].
    Timeout,
    /// Another master took the bus.
    ArbitrationLost,
    /// The rate of `aclk100` is unknown: see [`Cmu::aclk100_hz`].
    UnknownClock,
    /// The requested rate is zero, or lower than the prescaler can reach.
    BadRate,
}

/// Handle to one I2C controller. Holds no state.
#[derive(Clone, Copy, Debug)]
pub struct I2c {
    bus: Bus,
}

impl I2c {
    /// Creates a handle.
    pub const fn new(bus: Bus) -> Self {
        Self { bus }
    }

    fn regs(&self) -> &Registers {
        // SAFETY: `soc::I2C_BASES` holds this controller's register block, device memory
        // that stays mapped for the whole program, and the struct has interior mutable
        // cells only.
        unsafe { &*(soc::I2C_BASES[self.bus.index()] as *const Registers) }
    }

    /// Opens the controller's clock gate, gives it its pins with their pull-ups, and sets the
    /// bus rate to the highest not above `hz`, which it returns. `sda_delay_ns` is the
    /// `samsung,i2c-sda-delay` of the bus in the DTS.
    pub fn init(&self, hz: u32, sda_delay_ns: u32) -> Result<u32, Error> {
        let cmu = Cmu::new();
        let clock = cmu.aclk100_hz().ok_or(Error::UnknownClock)?;
        let (con, rate) = prescaler(clock, hz).ok_or(Error::BadRate)?;

        cmu.enable_i2c(self.bus.index());
        let (pins, function) = self.bus.pins();
        for pin in pins {
            pin.set_pull(Pull::Up);
            pin.set_function(Function::Alt(function));
        }

        let regs = self.regs();
        regs.stat.set(0);
        regs.add.set(OWN_ADDRESS);
        regs.con.set(con | CON_IRQ_ENABLE | CON_ACK);
        regs.lc.set(line_control(clock, sda_delay_ns));
        Ok(rate)
    }

    /// Whether a device answers `address`. Sends its address for writing and a stop, nothing
    /// else, which changes no device's registers.
    pub fn probe(&self, address: u8) -> Result<(), Error> {
        self.write_read(address, &[], &mut [])
    }

    /// Sends `data` to `address`.
    pub fn write(&self, address: u8, data: &[u8]) -> Result<(), Error> {
        self.write_read(address, data, &mut [])
    }

    /// Fills `data` from `address`. An empty `data` does nothing.
    pub fn read(&self, address: u8, data: &mut [u8]) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }
        self.write_read(address, &[], data)
    }

    /// Sends `out` to `address` and then, after a repeated start, fills `input`. With both
    /// empty, this is [`probe`](Self::probe).
    pub fn write_read(&self, address: u8, out: &[u8], input: &mut [u8]) -> Result<(), Error> {
        assert!(address <= ADDRESS_MAX);
        let regs = self.regs();
        if regs.stat.get() & STAT_START_BUSY != 0 {
            return Err(Error::BusBusy);
        }
        // Left over from the stop of the previous transfer.
        self.release();

        let result = self.transfer(address, out, input);
        let stop = self.stop();
        result.and(stop)
    }

    fn transfer(&self, address: u8, out: &[u8], input: &mut [u8]) -> Result<(), Error> {
        let writing = !out.is_empty() || input.is_empty();
        if writing {
            self.start(address << 1, STAT_MASTER_TX)?;
            for byte in out {
                self.send(*byte)?;
            }
        }
        if !input.is_empty() {
            self.start((address << 1) | 1, STAT_MASTER_RX)?;
            self.receive(input)?;
        }
        Ok(())
    }

    /// A start, or a repeated start if the bus is held after a byte. In that case the
    /// release below is what lets the start go out (i2c-s3c2410.c:491,545-548).
    fn start(&self, address_byte: u8, mode: u32) -> Result<(), Error> {
        let regs = self.regs();
        regs.stat.set(mode | STAT_ENABLE);
        regs.ds.set(address_byte);
        setup_delay();
        regs.stat.set(mode | STAT_ENABLE | STAT_START_BUSY);
        self.release();
        self.wait_byte()?;
        if regs.stat.get() & STAT_NO_ACK != 0 {
            return Err(Error::AddressNack);
        }
        Ok(())
    }

    fn send(&self, byte: u8) -> Result<(), Error> {
        let regs = self.regs();
        regs.ds.set(byte);
        setup_delay();
        self.release();
        self.wait_byte()?;
        if regs.stat.get() & STAT_NO_ACK != 0 {
            return Err(Error::DataNack);
        }
        Ok(())
    }

    /// Acknowledges every byte but the last, which tells the device to stop sending.
    fn receive(&self, data: &mut [u8]) -> Result<(), Error> {
        let regs = self.regs();
        let last = data.len() - 1;
        for (index, byte) in data.iter_mut().enumerate() {
            // Keep CON_PENDING set in this write, so that this does not release the bus yet.
            let con = regs.con.get();
            regs.con.set(if index == last { con & !CON_ACK } else { con | CON_ACK });
            self.release();
            self.wait_byte()?;
            *byte = regs.ds.get();
        }
        regs.con.set(regs.con.get() | CON_ACK);
        Ok(())
    }

    /// Clearing the start bit in the current mode, then releasing the bus, makes the stop:
    /// i2c-s3c2410.c:333.
    fn stop(&self) -> Result<(), Error> {
        let regs = self.regs();
        regs.stat.set((regs.stat.get() & STAT_MODE_MASK) | STAT_ENABLE);
        self.release();
        for _ in 0..POLL_LIMIT {
            if regs.stat.get() & STAT_START_BUSY == 0 {
                return Ok(());
            }
        }
        Err(Error::Timeout)
    }

    fn release(&self) {
        let regs = self.regs();
        regs.con.set(regs.con.get() & !CON_PENDING);
    }

    fn wait_byte(&self) -> Result<(), Error> {
        let regs = self.regs();
        for _ in 0..POLL_LIMIT {
            if regs.con.get() & CON_PENDING != 0 {
                if regs.stat.get() & STAT_ARBITRATION_LOST != 0 {
                    return Err(Error::ArbitrationLost);
                }
                return Ok(());
            }
        }
        Err(Error::Timeout)
    }
}

fn setup_delay() {
    for _ in 0..SETUP_ROUNDS {
        core::hint::spin_loop();
    }
}

/// `IICCON` bits and the rate they give: `clock / (16 or 512) / (field + 1)`, as in
/// i2c-s3c2410.c:45-51,821. The smaller prescaler is finer, so it comes first.
fn prescaler(clock: u32, hz: u32) -> Option<(u32, u32)> {
    if hz == 0 {
        return None;
    }
    for (divider, bit) in [(16, 0), (512, CON_DIV_512)] {
        let scale = clock.div_ceil(divider * hz).max(1);
        if scale <= CON_SCALE_MAX + 1 {
            return Some((bit | (scale - 1), clock / (divider * scale)));
        }
    }
    None
}

fn line_control(clock: u32, sda_delay_ns: u32) -> u32 {
    if sda_delay_ns == 0 {
        return 0;
    }
    let clocks = (u64::from(clock) * u64::from(sda_delay_ns)).div_ceil(1_000_000_000);
    clocks.div_ceil(LC_CLOCKS_PER_UNIT).min(LC_UNITS_MAX) as u32 | LC_FILTER
}
