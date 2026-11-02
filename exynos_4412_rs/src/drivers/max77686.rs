//! MAX77686 power management IC: the state of its LDOs, and turning them on and off.
//!
//! Paths are relative to the Linux 5.15.198 tree. `max77686-regulator.c` is in
//! `drivers/regulator/`, `max77686-private.h` in `include/linux/mfd/`.
//!
//! U-Boot leaves most peripheral rails of this board off, whatever `regulator-boot-on` says
//! in the DTS. A device on one of them does not answer on I2C until its LDO is on. The LDOs
//! with a device at each I2C address are in `exynos4412-mehmet.dts`.
//!
//! ```ignore
//! let i2c = I2c::new(Bus::I2c0);
//! i2c.init(400_000, 100)?;
//! Max77686::new(i2c).enable_ldo(24, 3000)?;
//! ```
//!
//! # What this does not check
//!
//! A rail at the wrong voltage can damage what it feeds, and turning off the wrong one can
//! stop the board: some LDOs feed the CPU, the memory and the eMMC. This driver checks only
//! that the voltage is one the LDO can produce. The caller must take the voltage from the
//! DTS. Of the bucks, only the voltage of BUCK5 to BUCK9 is covered, and the GPIO control of
//! BUCK8, BUCK9 and LDO20 to LDO22 (`max77686-regulator.c:103-107`) is not.

use crate::drivers::i2c::{self, I2c};

/// `exynos4412-mehmet.dts:962`, `max77686: pmic@9`, on `i2c_0`.
pub const ADDRESS: u8 = 0x09;
/// max77686-private.h:20.
const REG_DEVICE_ID: u8 = 0x00;
/// max77686-private.h:80. The other LDOs follow in order: `.enable_reg` and `.vsel_reg` at
/// max77686-regulator.c:322-324.
const REG_LDO1_CTRL1: u8 = 0x40;
/// max77686-private.h:69, `MAX77686_REG_BUCK5OUT`. Each later buck is two registers on:
/// `.vsel_reg` in `regulator_desc_buck`, max77686-regulator.c.
const REG_BUCK5_OUT: u8 = 0x31;
/// The bucks with one plain voltage register: `regulator_desc_buck`, max77686-regulator.c:475-479.
const BUCKS_WITH_OUT: core::ops::RangeInclusive<u8> = 5..=9;
/// max77686-regulator.c:27.
const BUCK_MIN_MILLIVOLTS: u16 = 750;
/// max77686-regulator.c:28.
const BUCK_STEP_MILLIVOLTS: u16 = 50;
/// max77686-regulator.c:62, which `regulator_desc_buck` uses as `.vsel_mask`.
const BUCK_VSEL_MASK: u8 = VSEL_MASK;
/// Number of LDOs: max77686-regulator.c:68.
const LDO_COUNT: u8 = 26;

/// max77686-regulator.c:58.
const OPMODE_SHIFT: u32 = 6;
/// max77686-regulator.c:56 `MAX77686_NORMAL`.
const OPMODE_NORMAL: u8 = 3;
/// max77686-regulator.c:62.
const VSEL_MASK: u8 = 0x3F;
/// max77686-regulator.c:23,25: lowest voltage of both kinds of LDO.
const MIN_MILLIVOLTS: u16 = 800;
/// max77686-regulator.c:24.
const STEP_MILLIVOLTS: u16 = 50;
/// max77686-regulator.c:26.
const LOW_STEP_MILLIVOLTS: u16 = 25;
/// The LDOs with the finer step: `regulator_desc_ldo1_low` and `regulator_desc_ldo_low` at
/// max77686-regulator.c:445-459.
const LOW_STEP_LDOS: [u8; 6] = [1, 2, 6, 7, 8, 15];

/// Why a call to the PMIC failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The I2C transfer failed.
    Bus(i2c::Error),
    /// There is no LDO with this number.
    NoSuchLdo,
    /// This is not one of BUCK5 to BUCK9.
    NoSuchBuck,
    /// The regulator cannot produce this voltage.
    BadVoltage,
    /// The register holds something else after the write.
    ReadBack {
        /// The value written.
        wrote: u8,
        /// The value read back.
        read: u8,
    },
}

impl From<i2c::Error> for Error {
    fn from(error: i2c::Error) -> Self {
        Self::Bus(error)
    }
}

/// Contents of one LDO's control register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LdoState {
    number: u8,
    raw: u8,
}

impl LdoState {
    /// The register value.
    pub const fn raw(&self) -> u8 {
        self.raw
    }

    /// The operating mode. 0 is off: `max77686-regulator.c:264` turns an LDO off with
    /// `regulator_disable_regmap`, and no `disable_val` is set. 3 is on.
    /// `max77686-regulator.c:45-53` gives the meanings of 1 and 2, which depend on the LDO.
    pub const fn mode(&self) -> u8 {
        self.raw >> OPMODE_SHIFT
    }

    /// The voltage it is set to, whether or not it is on.
    pub fn millivolts(&self) -> u16 {
        MIN_MILLIVOLTS + u16::from(self.raw & VSEL_MASK) * step_millivolts(self.number)
    }
}

fn step_millivolts(ldo: u8) -> u16 {
    if LOW_STEP_LDOS.contains(&ldo) { LOW_STEP_MILLIVOLTS } else { STEP_MILLIVOLTS }
}

/// Handle to the PMIC. Holds no state. The bus must be initialized.
#[derive(Clone, Copy, Debug)]
pub struct Max77686 {
    i2c: I2c,
}

impl Max77686 {
    /// Creates a handle on the bus the PMIC is on.
    pub const fn new(i2c: I2c) -> Self {
        Self { i2c }
    }

    fn read(&self, register: u8) -> Result<u8, Error> {
        let mut value = [0];
        self.i2c.write_read(ADDRESS, &[register], &mut value)?;
        Ok(value[0])
    }

    /// Reads the device ID register.
    pub fn device_id(&self) -> Result<u8, Error> {
        self.read(REG_DEVICE_ID)
    }

    /// Reads the state of LDO `number`, from 1 to 26.
    pub fn ldo(&self, number: u8) -> Result<LdoState, Error> {
        Ok(LdoState { number, raw: self.read(ldo_register(number)?)? })
    }

    /// Turns on LDO `number` at `millivolts`, both set by one write. See the [module
    /// documentation](self) for what the caller must check. The rail still has to settle.
    pub fn enable_ldo(&self, number: u8, millivolts: u16) -> Result<(), Error> {
        ldo_register(number)?;
        let vsel = voltage_select(number, millivolts).ok_or(Error::BadVoltage)?;
        self.restore_ldo(LdoState { number, raw: (OPMODE_NORMAL << OPMODE_SHIFT) | vsel })
    }

    /// Writes back a state from [`ldo`](Self::ldo), and checks that it took.
    pub fn restore_ldo(&self, state: LdoState) -> Result<(), Error> {
        let register = ldo_register(state.number)?;
        self.i2c.write(ADDRESS, &[register, state.raw])?;
        let read = self.read(register)?;
        if read == state.raw { Ok(()) } else { Err(Error::ReadBack { wrote: state.raw, read }) }
    }
}

impl Max77686 {
    /// The voltage BUCK `number`, from 5 to 9, is set to, whether or not it is on.
    pub fn buck_millivolts(&self, number: u8) -> Result<u16, Error> {
        let raw = self.read(buck_register(number)?)?;
        Ok(BUCK_MIN_MILLIVOLTS + u16::from(raw & BUCK_VSEL_MASK) * BUCK_STEP_MILLIVOLTS)
    }

    /// Sets the voltage of BUCK `number`, from 5 to 9, and checks that it took. See the [module
    /// documentation](self): other rails may be fed from it. The rail still has to settle.
    pub fn set_buck_millivolts(&self, number: u8, millivolts: u16) -> Result<(), Error> {
        let register = buck_register(number)?;
        let above_min = millivolts.checked_sub(BUCK_MIN_MILLIVOLTS).ok_or(Error::BadVoltage)?;
        let vsel = above_min / BUCK_STEP_MILLIVOLTS;
        if above_min % BUCK_STEP_MILLIVOLTS != 0 || vsel > u16::from(BUCK_VSEL_MASK) {
            return Err(Error::BadVoltage);
        }
        let current = self.read(register)?;
        let wrote = (current & !BUCK_VSEL_MASK) | vsel as u8;
        self.i2c.write(ADDRESS, &[register, wrote])?;
        let read = self.read(register)?;
        if read == wrote { Ok(()) } else { Err(Error::ReadBack { wrote, read }) }
    }
}

fn buck_register(number: u8) -> Result<u8, Error> {
    if BUCKS_WITH_OUT.contains(&number) {
        Ok(REG_BUCK5_OUT + (number - 5) * 2)
    } else {
        Err(Error::NoSuchBuck)
    }
}

fn ldo_register(number: u8) -> Result<u8, Error> {
    if (1..=LDO_COUNT).contains(&number) {
        Ok(REG_LDO1_CTRL1 + number - 1)
    } else {
        Err(Error::NoSuchLdo)
    }
}

fn voltage_select(ldo: u8, millivolts: u16) -> Option<u8> {
    let step = step_millivolts(ldo);
    let above_min = millivolts.checked_sub(MIN_MILLIVOLTS)?;
    let vsel = above_min / step;
    (above_min % step == 0 && vsel <= u16::from(VSEL_MASK)).then_some(vsel as u8)
}
