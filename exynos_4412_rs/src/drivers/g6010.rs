//! The u-blox G6010 GPS receiver of the Mehmet board: its power and its UART.
//!
//! The receiver sends NMEA sentences by itself once it is powered: read them from [`UART`]
//! (with `embassy::uart::UartRx`, or polled) and parse them with [`crate::nmea`]. Nothing has
//! to be sent to it.
//!
//! What the vendor kernel says about it (written from scratch here, nothing of that GPL code
//! is in this crate):
//!
//! - `drivers/misc/g6010.c` only switches two supplies, through an ioctl of `/dev/g6010_ctl`:
//!   on, `vdd_gps_1v8`, 10 ms, then `vdd_gps_2v8`; off, the same order. Android's GPS HAL
//!   talked to the receiver over `/dev/ttySAC1` itself.
//! - `mach-mehmet.c`: `vdd_gps_1v8` is MAX77686 LDO9 at 1.8 V, the receiver's I/O; `vdd_gps_2v8`
//!   is LDO18 at 2.8 V, the antenna's LNA. Both off at boot.
//! - `mach-mehmet.c` `s3c_setup_uart_cfg_gpio`, port 1 and `gpio-mehmet.h`: RXD on GPA0(4) with
//!   pull-up, TXD on GPA0(5) without, function 2.
//! - No reset, EXTINT or time-pulse line is described, and the PMIC's 32 kHz output that was
//!   to feed it (`max77686_enp32khz_consumer`, "gps_32khz") is commented out.
//! - The baud rate is not in the kernel. 9600 is the u-blox 6 default ([`DEFAULT_BAUD`]).
//!
//! The SoC's own GPS block and its power domains (`pd::Domain::Gps`) have nothing to do with it.

use crate::drivers::cmu::Cmu;
use crate::drivers::gpio::{self, Function, Pin, Pull};
use crate::drivers::max77686::{self, Max77686};
use crate::drivers::sdio::delay_ms;
use crate::drivers::timer::Mct;
use crate::drivers::uart::{Uart, UartId};

/// The UART the receiver is on.
pub const UART: UartId = UartId::Uart1;
/// MAX77686 LDO9, `vdd_gps_1v8`: the receiver's I/O.
pub const LDO_IO: (u8, u16) = (9, 1800);
/// MAX77686 LDO18, `vdd_gps_2v8`: the antenna's LNA.
pub const LDO_LNA: (u8, u16) = (18, 2800);
/// The u-blox 6 default.
pub const DEFAULT_BAUD: u32 = 9600;
/// `g6010.c`: between the two supplies.
const SUPPLY_GAP_MS: u64 = 10;
/// `sclk_uart1` asked of the CMU, the rate U-Boot gives the console's.
const UART_CLOCK_HZ: u32 = 100_000_000;
/// GPA0(4), `GPIO_GPS_RXD`, and GPA0(5), `GPIO_GPS_TXD`: UART1's RXD and TXD as function 2.
const RXD: Pin = gpio::GPA0.pin(4);
const TXD: Pin = gpio::GPA0.pin(5);
const UART_FUNCTION: Function = Function::Alt(2);

/// Turns the receiver's supplies on, in the order of the vendor kernel.
pub fn power_on(pmic: &Max77686, mct: Mct) -> Result<(), max77686::Error> {
    pmic.enable_ldo(LDO_IO.0, LDO_IO.1)?;
    delay_ms(mct, SUPPLY_GAP_MS);
    pmic.enable_ldo(LDO_LNA.0, LDO_LNA.1)
}

/// Turns the receiver's supplies off, in the order of the vendor kernel. Its TXD then floats:
/// the pull-up on RXD keeps the UART from reading it as a break.
pub fn power_off(pmic: &Max77686, mct: Mct) -> Result<(), max77686::Error> {
    pmic.disable_ldo(LDO_IO.0)?;
    delay_ms(mct, SUPPLY_GAP_MS);
    pmic.disable_ldo(LDO_LNA.0)
}

/// Whether both supplies are on.
pub fn is_powered(pmic: &Max77686) -> Result<bool, max77686::Error> {
    Ok(pmic.ldo(LDO_IO.0)?.mode() != 0 && pmic.ldo(LDO_LNA.0)?.mode() != 0)
}

/// Sets up the receiver's UART: its clock, its pins, then 8N1 at `baud` with both FIFOs on and
/// every interrupt masked. Returns the baud rate the divider gives. For an async reader, create
/// `embassy::uart::UartRx` after it.
pub fn setup_uart(baud: u32) -> u32 {
    let clock = Cmu::new().set_uart_clock(UART as usize, UART_CLOCK_HZ);
    RXD.set_pull(Pull::Up);
    RXD.set_function(UART_FUNCTION);
    TXD.set_pull(Pull::None);
    TXD.set_function(UART_FUNCTION);
    Uart::new(UART).configure(baud, clock)
}

/// Changes the baud rate of the receiver's UART, set up before by [`setup_uart`]. Returns the
/// baud rate the divider gives. Like [`setup_uart`], it masks the UART's interrupts and empties
/// its FIFOs: turn the receive interrupt on again after it (`Uart::enable_rx_interrupt`).
pub fn set_baud(baud: u32) -> u32 {
    let clock = Cmu::new().sclk_uart_hz(UART as usize).expect("setup_uart ran first");
    Uart::new(UART).configure(baud, clock)
}
