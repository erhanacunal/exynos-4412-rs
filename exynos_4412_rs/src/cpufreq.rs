//! The CPU clock, chosen at run time: [`set_arm_mhz`] moves all four cores to one of
//! [`cmu::ARM_POINTS`], from 200 MHz to 1.4 GHz, with `vdd_arm` at the voltage of that point.
//! Nothing here runs at boot: the CPU stays at the rate U-Boot left, 1 GHz on this board, until a
//! program asks for another.
//!
//! `vdd_arm` is BUCK2 of the MAX77686 (`exynos4412-mehmet.dts:278-279,1177-1186`). The voltage
//! goes into the DVS register the pins choose ([`max77686::selected_dvs`]): `DVS1` as U-Boot
//! leaves them, `DVS2` as Linux sets them. Like `cpufreq-dt`, the voltage is raised before a
//! faster clock and lowered after a slower one.
//!
//! ```ignore
//! let i2c = I2c::new(Bus::I2c0);
//! i2c.init(400_000, 100)?;
//! cpufreq::set_arm_mhz(Max77686::new(i2c), mct, 1400)?;
//! ```
//!
//! # What this does not check
//!
//! The voltages are those of mainline Linux's `cpu0_opp_table`, the same for every chip; the
//! vendor kernel chose them per chip from fused ASV groups. The temperature is not watched: no
//! TMU driver throttles the CPU. Call it from one core at a time, and not while another core
//! uses `i2c_0`.

use crate::drivers::cmu::{self, ArmClockError, ArmPoint, Cmu};
use crate::drivers::max77686::{self, Max77686};
use crate::drivers::timer::Mct;
use crate::soc;

/// BUCK2 is `vdd_arm`: `exynos4412-mehmet.dts:1177-1178`.
pub const ARM_BUCK: u8 = 2;
/// `regulator-min-microvolt` of BUCK2: `exynos4412-mehmet.dts:1179`.
pub const ARM_MIN_MICROVOLTS: u32 = 850_000;
/// `regulator-max-microvolt` of BUCK2: `exynos4412-mehmet.dts:1180`.
pub const ARM_MAX_MICROVOLTS: u32 = 1_500_000;
/// Added to the ramp time before the clock rises: the output's settling, ours, not Linux's.
const SETTLE_US: u64 = 100;

/// Why [`set_arm_mhz`] did not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// [`cmu::ARM_POINTS`] has no point at this rate; nothing was changed.
    NoSuchPoint(u32),
    /// The point's voltage is outside the range of BUCK2 in the DTS; nothing was changed.
    VoltageOutOfRange(u32),
    /// BUCK2 is not in its normal mode (3); nothing was changed.
    BuckOff(u8),
    /// The PMIC failed. If the clock had already been lowered, it stays lowered.
    Pmic(max77686::Error),
    /// The clock change failed. If the voltage had already been raised, it stays raised.
    Clock(ArmClockError),
}

impl From<max77686::Error> for Error {
    fn from(error: max77686::Error) -> Self {
        Self::Pmic(error)
    }
}

impl From<ArmClockError> for Error {
    fn from(error: ArmClockError) -> Self {
        Self::Clock(error)
    }
}

/// What [`set_arm_mhz`] changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    /// `armclk` before and after, in Hz.
    pub from_hz: u32,
    /// See `from_hz`.
    pub to_hz: u32,
    /// BUCK2's chosen DVS register before and after, in microvolts.
    pub from_microvolts: u32,
    /// See `from_microvolts`.
    pub to_microvolts: u32,
    /// The index of that register, 0 for `DVS1`.
    pub dvs: usize,
}

/// The CPU clock now, `armclk`, in Hz.
pub fn arm_hz() -> u32 {
    Cmu::new().arm_clocks().armclk_hz()
}

/// The voltage of `vdd_arm` now: the DVS register of BUCK2 the pins choose, in microvolts.
pub fn arm_microvolts(pmic: Max77686) -> Result<u32, Error> {
    Ok(pmic.dvs_buck(ARM_BUCK)?.microvolts(max77686::selected_dvs()))
}

/// Runs the CPU at `mhz`, one of [`cmu::ARM_POINTS`], with `vdd_arm` at that point's voltage.
/// The bus of `pmic` must be initialized. See the [module documentation](self).
pub fn set_arm_mhz(pmic: Max77686, mct: Mct, mhz: u32) -> Result<Change, Error> {
    let point = cmu::arm_point(mhz).ok_or(Error::NoSuchPoint(mhz))?;
    set_arm_point(pmic, mct, point)
}

/// [`set_arm_mhz`] with the point itself.
pub fn set_arm_point(pmic: Max77686, mct: Mct, point: ArmPoint) -> Result<Change, Error> {
    if !(ARM_MIN_MICROVOLTS..=ARM_MAX_MICROVOLTS).contains(&point.microvolts) {
        return Err(Error::VoltageOutOfRange(point.microvolts));
    }
    let buck = pmic.dvs_buck(ARM_BUCK)?;
    if buck.mode() != 3 {
        return Err(Error::BuckOff(buck.mode()));
    }
    let dvs = max77686::selected_dvs();
    let from_microvolts = buck.microvolts(dvs);
    let from_hz = arm_hz();
    let cmu = Cmu::new();

    let to_hz = if point.microvolts > from_microvolts {
        pmic.set_dvs_microvolts(ARM_BUCK, dvs, point.microvolts)?;
        let ramp_us = u64::from((point.microvolts - from_microvolts).div_ceil(buck.ramp_microvolts_per_us()));
        wait_us(mct, ramp_us + SETTLE_US);
        cmu.set_arm_clock(point, mct)?
    } else {
        let hz = cmu.set_arm_clock(point, mct)?;
        if point.microvolts < from_microvolts {
            pmic.set_dvs_microvolts(ARM_BUCK, dvs, point.microvolts)?;
        }
        hz
    };
    Ok(Change { from_hz, to_hz, from_microvolts, to_microvolts: point.microvolts, dvs })
}

fn wait_us(mct: Mct, us: u64) {
    let end = mct.count() + us * u64::from(soc::MCT_CLOCK_HZ / 1_000_000);
    while mct.count() < end {
        core::hint::spin_loop();
    }
}
