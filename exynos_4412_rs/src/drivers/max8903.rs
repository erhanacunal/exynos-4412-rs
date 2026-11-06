//! Maxim MAX8903 battery charger: which inputs have power, whether it charges, and its fault line.
//!
//! The chip has no bus: five GPIOs connect it, all active low. They are in
//! `exynos4412-mehmet.dts:186-201` of the Linux 5.15.198 tree, cited as
//! `exynos4412-mehmet.dts:<line>`, and in the vendor kernel, `/home/developer/willow`,
//! `arch/arm/mach-exynos/include/mach/gpio-mehmet.h:101-105` and `mach-mehmet.c:879-928`.
//!
//! | Pin | Signal | Direction | Meaning when low |
//! |---|---|---|---|
//! | GPM0[2] | `nCHG_EN` (`CEN`) | output | charging enabled |
//! | GPX2[5] | `nCHARGING` (`CHG`) | input | the battery is being charged |
//! | GPX2[4] | `nDC_OK` (`DOK`) | input | the DC input has valid power |
//! | GPX2[6] | `nUSB_OK` (`UOK`) | input | the USB input has valid power |
//! | GPX3[0] | `nBAT_FLT` (`FLT`) | input | the charge timer ran out: a fault |
//!
//! The current limit mode (`DCM`) and USB suspend (`USUS`) inputs are not connected to GPIOs on
//! this board: neither the mainline DTS nor the vendor kernel has them.
//!
//! # Charging
//!
//! `CHG` is high whenever the chip is not charging: with no input power, with charging disabled,
//! after the charge is done, or after a fault. [`Status::state`] tells these apart only as far as
//! the pins do. Both Linux drivers enable charging whenever an input has power
//! (`drivers/power/supply/max8903_charger.c` in the mainline tree, `max8903g_charger_online` in
//! the vendor one); [`Max8903::new`] leaves `CEN` as it finds it, and [`Max8903::set_charging`]
//! decides.
//!
//! On the board, U-Boot leaves `CEN` an output, driven low. Driving it high did not stop
//! charging: `CHG` stayed low, and charging started again on a new DC connection
//! (`charger_test`). The vendor kernel never drives it high either: nothing calls its
//! `max8903g_charger_disable`. So either `CEN` does not reach the chip as the DTS says, or it is
//! held low on the board; [`Max8903::set_charging`]`(false)` cannot be relied on.
//!
//! A timer fault stays until charging restarts: [`Max8903::restart_charging`] disables charging
//! and enables it again, which for the same reason may have no effect. A fault has not been seen.
//!
//! # Concurrency
//!
//! The pins are only read, apart from `CEN`, which only [`Max8903::set_charging`] writes. Each
//! access is one register access, so the type is `Copy` and can be used from any core.

use super::gpio::{self, Function, Pin, Pull};
use super::timer::Mct;
use crate::soc;

/// `cen-gpios`, `exynos4412-mehmet.dts:192`; an output with no pull, `:799-803`.
const CEN: Pin = gpio::GPM0.pin(2);
/// `chg-gpios`, `exynos4412-mehmet.dts:193`.
const CHG: Pin = gpio::GPX2.pin(5);
/// `dok-gpios`, `exynos4412-mehmet.dts:194`.
const DOK: Pin = gpio::GPX2.pin(4);
/// `uok-gpios`, `exynos4412-mehmet.dts:195`.
const UOK: Pin = gpio::GPX2.pin(6);
/// `flt-gpios`, `exynos4412-mehmet.dts:196`.
const FLT: Pin = gpio::GPX3.pin(0);

/// How long [`Max8903::restart_charging`] keeps charging disabled. Not from the datasheet: only
/// well above the time the pin takes to change.
const RESTART_OFF_MS: u64 = 10;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// What the charger is doing, as far as its pins tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// `CHG` low: the battery is being charged.
    Charging,
    /// An input has power, but the chip is not charging: the charge is done, charging is
    /// disabled, or there is a fault ([`Status::fault`]).
    NotCharging,
    /// No input has power: the board runs from the battery.
    Discharging,
}

/// The charger's pins, read at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// The DC input has valid power.
    pub dc: bool,
    /// The USB input has valid power. A USB cable to a PC counts.
    pub usb: bool,
    /// The battery is being charged.
    pub charging: bool,
    /// The charge timer ran out.
    pub fault: bool,
    /// Whether `CEN` enables charging; `None` while `CEN` is not an output, as before the first
    /// [`Max8903::set_charging`] if U-Boot left it so.
    pub enabled: Option<bool>,
}

impl Status {
    /// Whether either input has power.
    pub fn powered(&self) -> bool {
        self.dc || self.usb
    }

    /// What the charger is doing.
    pub fn state(&self) -> State {
        if self.charging {
            State::Charging
        } else if self.powered() {
            State::NotCharging
        } else {
            State::Discharging
        }
    }
}

/// The MAX8903 charger. Holds nothing: the state is in the pins.
#[derive(Clone, Copy, Debug)]
pub struct Max8903 {
    _private: (),
}

impl Max8903 {
    /// Configures the four status pins as inputs with pull-up, as `exynos4412-mehmet.dts:806-810`
    /// does. Leaves `CEN` as it is, so that charging does not change until
    /// [`Max8903::set_charging`].
    pub fn new() -> Self {
        for pin in [CHG, DOK, UOK, FLT] {
            pin.set_input(Pull::Up);
        }
        Self { _private: () }
    }

    /// Reads all the pins.
    pub fn status(&self) -> Status {
        Status {
            dc: !DOK.read(),
            usb: !UOK.read(),
            charging: !CHG.read(),
            fault: !FLT.read(),
            enabled: self.charging_enabled(),
        }
    }

    /// Whether `CEN` enables charging; `None` while it is not an output.
    pub fn charging_enabled(&self) -> Option<bool> {
        match CEN.function() {
            Function::Output => Some(!CEN.read()),
            _ => None,
        }
    }

    /// Enables or disables charging through `CEN`, making it an output if it is not yet one.
    /// With charging disabled the chip still powers the board from its inputs.
    ///
    /// Disabling had no effect on the board: see the module documentation.
    pub fn set_charging(&self, enable: bool) {
        // Active low; `set_output` sets the level before the direction.
        CEN.set_output(!enable);
    }

    /// Disables charging for a moment, then enables it: this starts a new charge cycle and its
    /// timer, which clears a timer fault. Needs the MCT running.
    pub fn restart_charging(&self, mct: Mct) {
        self.set_charging(false);
        let start = mct.count();
        while mct.count() - start < RESTART_OFF_MS * COUNTS_PER_MS {}
        self.set_charging(true);
    }
}

impl Default for Max8903 {
    fn default() -> Self {
        Self::new()
    }
}
