//! Maxim MAX17040 fuel gauge, on `i2c_3` at [`ADDRESS`]: the battery's voltage and charge.
//!
//! Paths are relative to the Linux 5.15.198 tree: the node is `exynos4412-mehmet.dts:1272-1276`,
//! the mainline driver `drivers/power/supply/max17040_battery.c`, cited as
//! `max17040_battery.c:<line>`. The vendor kernel, `/home/developer/willow`, has its own
//! `drivers/power/max17040_battery.c`, cited as `vendor max17040_battery.c:<line>`.
//!
//! The registers are 16 bits, most significant byte first, read from their even address
//! (`max17040_battery.c:409-414`). The chip has no current sense and no temperature: the charge
//! is its own estimate from the voltage. It has no alert line on this board (`exynos4412-mehmet.dts:1275`), so a low
//! battery shows only when read.
//!
//! # Charge scale
//!
//! The two Linux drivers read `SOC` differently. Mainline takes the high byte as the percentage
//! (`max17040_battery.c:199-206`, no `maxim,double-soc` in the DTS). The vendor driver, built
//! as `FUEL_GAUGE_ONLY` because the MAX8903 is configured (`vendor max17040_battery.c:40-42`),
//! halves it (`:178`), as for a gauge with a custom model, whose unit is 1/512 %. The board
//! settles it: `SOC` read `0xb3c8` while charging at 4153 mV (`battery_test`), whose high byte,
//! 179, cannot be a percentage, and which is 89.9 % at 1/512 %. [`Reading::percent`] follows
//! the vendor kernel.
//!
//! `CONFIG` read `0x8200` on the board: `RCOMP` `0x82`, not the chip's default `0x97`
//! (`max17040_battery.c:33`), so the gauge has been set up for its battery by something before.
//!
//! # Writes
//!
//! None. `CONFIG` (`RCOMP`) stays as the chip has it: the vendor driver writes it only outside
//! `FUEL_GAUGE_ONLY` (`vendor max17040_battery.c:334-336`), and mainline only from the DTS, which has none. Reset and
//! quick-start are left out: each throws away the chip's estimate.

use crate::drivers::i2c::{self, I2c};

/// `exynos4412-mehmet.dts:1272-1274`, `fuel-gauge@36` on `i2c_3`.
pub const ADDRESS: u8 = 0x36;
/// `samsung,i2c-max-bus-freq` of `i2c_3`, `exynos4412-mehmet.dts:1269`.
pub const BUS_HZ: u32 = 100_000;
/// `samsung,i2c-sda-delay` of `i2c_3`, `exynos4412-mehmet.dts:1267`.
pub const BUS_SDA_DELAY_NS: u32 = 100;

/// max17040_battery.c:22.
const VCELL: u8 = 0x02;
/// max17040_battery.c:23.
const SOC: u8 = 0x04;
/// max17040_battery.c:25.
const VERSION: u8 = 0x08;
/// max17040_battery.c:26, `RCOMP` in the high byte.
const CONFIG: u8 = 0x0C;

/// max17040_battery.c:67-69,186: 12 bits at the top of `VCELL`, 1250 uV a step.
const VCELL_SHIFT: u32 = 4;
const VCELL_STEP_UV: u32 = 1250;

/// One reading of the gauge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The battery's voltage in mV.
    pub millivolts: u16,
    /// `SOC` as read.
    pub raw_soc: u16,
}

impl Reading {
    /// The charge in whole percent, as the vendor kernel computes it: half the high byte of
    /// `SOC`, at most 100 (`vendor max17040_battery.c:175-178`).
    pub fn percent(&self) -> u8 {
        ((self.raw_soc >> 9) as u8).min(100)
    }

    /// The charge in tenths of a percent, rounded, from all of `SOC` at 1/512 %; at most 1000.
    pub fn permille(&self) -> u16 {
        ((u32::from(self.raw_soc) * 10 + 256) / 512).min(1000) as u16
    }
}

/// Handle to the gauge. Holds no state. The bus must be initialized, at [`BUS_HZ`].
#[derive(Clone, Copy, Debug)]
pub struct Max17040 {
    i2c: I2c,
}

impl Max17040 {
    /// The gauge on `i2c`, which must be `i2c_3`.
    pub const fn new(i2c: I2c) -> Self {
        Self { i2c }
    }

    fn read(&self, register: u8) -> Result<u16, i2c::Error> {
        let mut value = [0; 2];
        self.i2c.write_read(ADDRESS, &[register], &mut value)?;
        Ok(u16::from_be_bytes(value))
    }

    /// The battery's voltage and charge. The two are separate reads.
    pub fn reading(&self) -> Result<Reading, i2c::Error> {
        let vcell = self.read(VCELL)?;
        let raw_soc = self.read(SOC)?;
        let microvolts = u32::from(vcell >> VCELL_SHIFT) * VCELL_STEP_UV;
        Ok(Reading { millivolts: (microvolts / 1000) as u16, raw_soc })
    }

    /// `VERSION`, the chip's revision.
    pub fn version(&self) -> Result<u16, i2c::Error> {
        self.read(VERSION)
    }

    /// `CONFIG`: `RCOMP` in the high byte, the alert settings in the low one.
    pub fn config(&self) -> Result<u16, i2c::Error> {
        self.read(CONFIG)
    }
}
