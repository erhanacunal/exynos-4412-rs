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
//!   turns it on for its polled mode too. Do not enable the bus's interrupt in the GIC: the
//!   async transfers enable it while they wait, and disable it again.
//! - A device holding a line low stays that way: there is no bus recovery. The call returns
//!   an error.
//! - Each timeout is a count of register reads, not a time: see [`POLL_LIMIT`].
//! - `i2c_8` is left out. It is the HDMI PHY's bus, with different stop behaviour
//!   (`i2c-s3c2410.c:328`) and `samsung,s3c2440-hdmiphy-i2c` in `exynos4.dtsi:600`.
//!
//! # Async transfers
//!
//! With the `embassy` feature, [`I2c::write_read_async`] and the calls built on it run the
//! same transfer, but await the controller's interrupt for each byte instead of polling, so
//! the core runs other tasks meanwhile: a byte takes 90 us at 100 kHz. The handler,
//! [`on_interrupt`], disables the interrupt at the GIC and wakes the transfer. An async mutex
//! per bus keeps one async transfer at a time on it. A byte that does not finish within
//! [`BYTE_TIMEOUT`] fails the transfer with [`Error::Timeout`]. A transfer dropped part way
//! finishes its byte, polled; a read takes one more byte, not acknowledged, so that the device
//! stops sending; then a stop goes out, and the bus is let go.
//!
//! # Concurrency
//!
//! Use each bus from one context at a time, including interrupt handlers and other cores.
//! The async mutex covers async transfers only: a polled call on a bus during an async
//! transfer on it, from another task of the same core for one, breaks both.
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

    /// The controller's GIC interrupt ID: `GIC_SPI 58` to `65` of `i2c_0`..`i2c_7` in
    /// `exynos4.dtsi:498-589`.
    pub const fn irq(self) -> u16 {
        32 + 58 + self as u16
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
            self.begin_start(address << 1, STAT_MASTER_TX);
            self.wait_byte()?;
            self.check_ack(Error::AddressNack)?;
            for byte in out {
                self.begin_send(*byte);
                self.wait_byte()?;
                self.check_ack(Error::DataNack)?;
            }
        }
        if !input.is_empty() {
            self.begin_start((address << 1) | 1, STAT_MASTER_RX);
            self.wait_byte()?;
            self.check_ack(Error::AddressNack)?;
            let last = input.len() - 1;
            for (index, byte) in input.iter_mut().enumerate() {
                self.begin_receive(index == last);
                self.wait_byte()?;
                *byte = self.regs().ds.get();
            }
            self.end_receive();
        }
        Ok(())
    }

    /// Sends a start, or a repeated start if the bus is held after a byte, with the address.
    /// In the second case the release below is what lets the start go out
    /// (i2c-s3c2410.c:491,545-548). Then wait for the byte and [`check_ack`](Self::check_ack).
    fn begin_start(&self, address_byte: u8, mode: u32) {
        let regs = self.regs();
        regs.stat.set(mode | STAT_ENABLE);
        regs.ds.set(address_byte);
        setup_delay();
        regs.stat.set(mode | STAT_ENABLE | STAT_START_BUSY);
        self.release();
    }

    /// Sends one byte. Then wait for it and [`check_ack`](Self::check_ack).
    fn begin_send(&self, byte: u8) {
        self.regs().ds.set(byte);
        setup_delay();
        self.release();
    }

    /// `error` if the byte just sent was not acknowledged.
    fn check_ack(&self, error: Error) -> Result<(), Error> {
        if self.regs().stat.get() & STAT_NO_ACK != 0 { Err(error) } else { Ok(()) }
    }

    /// Receives one byte, acknowledged unless it is the `last`, which tells the device to stop
    /// sending. Then wait for it and read `ds`.
    fn begin_receive(&self, last: bool) {
        let regs = self.regs();
        // Keep CON_PENDING set in this write, so that this does not release the bus yet.
        let con = regs.con.get();
        regs.con.set(if last { con & !CON_ACK } else { con | CON_ACK });
        self.release();
    }

    /// Acknowledging again, for the next transfer.
    fn end_receive(&self) {
        let regs = self.regs();
        regs.con.set(regs.con.get() | CON_ACK);
    }

    /// The byte is done, or arbitration was lost: `None` while it still runs.
    fn byte_done(&self) -> Option<Result<(), Error>> {
        let regs = self.regs();
        if regs.con.get() & CON_PENDING == 0 {
            return None;
        }
        if regs.stat.get() & STAT_ARBITRATION_LOST != 0 {
            return Some(Err(Error::ArbitrationLost));
        }
        Some(Ok(()))
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
        for _ in 0..POLL_LIMIT {
            if let Some(result) = self.byte_done() {
                return result;
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

#[cfg(feature = "embassy")]
pub use asynch::{BYTE_TIMEOUT, on_interrupt};

#[cfg(feature = "embassy")]
mod asynch {
    use core::future::poll_fn;
    use core::sync::atomic::{AtomicU32, Ordering};
    use core::task::Poll;

    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use embassy_sync::mutex::Mutex;
    use embassy_sync::waitqueue::AtomicWaker;
    use embassy_time::{Duration, with_timeout};
    use tock_registers::interfaces::Readable;

    use super::{ADDRESS_MAX, Bus, Error, I2c, STAT_MASTER_RX, STAT_MODE_MASK, STAT_START_BUSY};
    use crate::drivers::gic::Gic;

    const BUSES: usize = 8;
    /// Longer than any byte at the rates this driver sets, with room for a device stretching
    /// the clock. Not measured.
    pub const BYTE_TIMEOUT: Duration = Duration::from_millis(10);

    static WAKERS: [AtomicWaker; BUSES] = [const { AtomicWaker::new() }; BUSES];
    static LOCKS: [Mutex<CriticalSectionRawMutex, ()>; BUSES] = [const { Mutex::new(()) }; BUSES];
    static INTERRUPTS: [AtomicU32; BUSES] = [const { AtomicU32::new(0) }; BUSES];
    const ALL: [Bus; BUSES] = [Bus::I2c0, Bus::I2c1, Bus::I2c2, Bus::I2c3, Bus::I2c4, Bus::I2c5, Bus::I2c6, Bus::I2c7];

    /// Handles the interrupt of an I2C controller, if `id` is one: disables it at the GIC,
    /// whatever raised it, and wakes the transfer on that bus, which looks at the controller
    /// itself. The application's `handle_irq` reaches it through `embassy::on_interrupt`.
    pub fn on_interrupt(id: u16) -> bool {
        let Some(bus) = ALL.iter().find(|bus| bus.irq() == id) else {
            return false;
        };
        Gic::new().disable(id);
        INTERRUPTS[bus.index()].fetch_add(1, Ordering::Relaxed);
        WAKERS[bus.index()].wake();
        true
    }

    /// Sends a stop if a transfer is dropped part way: see the [module documentation](super).
    struct InFlight {
        i2c: I2c,
        done: bool,
    }

    impl Drop for InFlight {
        fn drop(&mut self) {
            let i2c = self.i2c;
            Gic::new().disable(i2c.bus.irq());
            if self.done {
                return;
            }
            // Whatever these give: the next transfer finds the bus busy if they failed. The
            // byte under way finishes first, polled. A device that is sending keeps sending
            // after an acknowledged byte, and may hold SDA low where the stop needs it high:
            // one more byte, not acknowledged, tells it to stop.
            let _ = i2c.wait_byte();
            if i2c.regs().stat.get() & STAT_MODE_MASK == STAT_MASTER_RX {
                i2c.begin_receive(true);
                let _ = i2c.wait_byte();
                i2c.end_receive();
            }
            let _ = i2c.stop();
        }
    }

    impl I2c {
        /// Interrupts the async transfers on this bus have taken, since the start.
        pub fn interrupts(&self) -> u32 {
            INTERRUPTS[self.bus.index()].load(Ordering::Relaxed)
        }

        /// The current byte, awaited on the controller's interrupt.
        async fn byte_async(&self) -> Result<(), Error> {
            let irq = self.bus.irq();
            let waker = &WAKERS[self.bus.index()];
            let wait = poll_fn(|cx| {
                if let Some(result) = self.byte_done() {
                    return Poll::Ready(result);
                }
                waker.register(cx.waker());
                let gic = Gic::new();
                gic.set_target(irq, 0);
                gic.enable(irq);
                // A byte done since the look above raises the interrupt: the controller's
                // line stays high until the bus is released.
                Poll::Pending
            });
            let result = with_timeout(BYTE_TIMEOUT, wait).await.unwrap_or(Err(Error::Timeout));
            Gic::new().disable(irq);
            result
        }

        /// [`write_read`](Self::write_read), awaiting each byte: see the [module
        /// documentation](super). Waits first for other async transfers on this bus.
        pub async fn write_read_async(&self, address: u8, out: &[u8], input: &mut [u8]) -> Result<(), Error> {
            assert!(address <= ADDRESS_MAX);
            let _lock = LOCKS[self.bus.index()].lock().await;
            if self.regs().stat.get() & STAT_START_BUSY != 0 {
                return Err(Error::BusBusy);
            }
            self.release();
            let mut flight = InFlight { i2c: *self, done: false };
            let result = self.transfer_async(address, out, input).await;
            flight.done = true;
            let stop = self.stop();
            result.and(stop)
        }

        /// [`write`](Self::write), awaiting each byte.
        pub async fn write_async(&self, address: u8, data: &[u8]) -> Result<(), Error> {
            self.write_read_async(address, data, &mut []).await
        }

        /// [`read`](Self::read), awaiting each byte.
        pub async fn read_async(&self, address: u8, data: &mut [u8]) -> Result<(), Error> {
            if data.is_empty() {
                return Ok(());
            }
            self.write_read_async(address, &[], data).await
        }

        /// [`probe`](Self::probe), awaiting each byte.
        pub async fn probe_async(&self, address: u8) -> Result<(), Error> {
            self.write_read_async(address, &[], &mut []).await
        }

        /// The steps of `transfer`, each byte awaited.
        async fn transfer_async(&self, address: u8, out: &[u8], input: &mut [u8]) -> Result<(), Error> {
            let writing = !out.is_empty() || input.is_empty();
            if writing {
                self.begin_start(address << 1, super::STAT_MASTER_TX);
                self.byte_async().await?;
                self.check_ack(Error::AddressNack)?;
                for byte in out {
                    self.begin_send(*byte);
                    self.byte_async().await?;
                    self.check_ack(Error::DataNack)?;
                }
            }
            if !input.is_empty() {
                self.begin_start((address << 1) | 1, super::STAT_MASTER_RX);
                self.byte_async().await?;
                self.check_ack(Error::AddressNack)?;
                let last = input.len() - 1;
                for (index, byte) in input.iter_mut().enumerate() {
                    self.begin_receive(index == last);
                    self.byte_async().await?;
                    *byte = self.regs().ds.get();
                }
                self.end_receive();
            }
            Ok(())
        }
    }
}
