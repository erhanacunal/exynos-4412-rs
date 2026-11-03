//! MAX77686 power management IC: the state of its LDOs, and turning them on and off; and its
//! real-time clock, [`Rtc`], read only.
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
use crate::drivers::timer::Mct;
use crate::soc;
use crate::time::DateTime;

/// `exynos4412-mehmet.dts:962`, `max77686: pmic@9`, on `i2c_0`.
pub const ADDRESS: u8 = 0x09;
/// max77686-private.h:20.
const REG_DEVICE_ID: u8 = 0x00;
/// max77686-private.h:136: the enables of the 32 kHz outputs.
const REG_32KHZ: u8 = 0x7F;
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

/// One of the PMIC's 32 kHz outputs. The value is the bit in its enable register:
/// `clk-max77686.c:59,64,69` with `dt-bindings/clock/maxim,max77686.h:13-15`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock32k {
    /// `MAX77686_CLK_AP`.
    Ap = 0,
    /// `MAX77686_CLK_CP`.
    Cp = 1,
    /// `MAX77686_CLK_PMIC`.
    Pmic = 2,
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

    /// Reads the register with the enables of the 32 kHz outputs.
    pub fn clocks_32k(&self) -> Result<u8, Error> {
        self.read(REG_32KHZ)
    }

    /// Turns on a 32 kHz output, leaving the others as they are, and checks that it took.
    pub fn enable_clock_32k(&self, clock: Clock32k) -> Result<(), Error> {
        let wrote = self.read(REG_32KHZ)? | (1 << clock as u8);
        self.i2c.write(ADDRESS, &[REG_32KHZ, wrote])?;
        let read = self.read(REG_32KHZ)?;
        if read & wrote == wrote { Ok(()) } else { Err(Error::ReadBack { wrote, read }) }
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

/// `drivers/rtc/rtc-max77686.c:20` `MAX77686_I2C_ADDR_RTC`: the RTC answers at an address of its
/// own on the PMIC's bus, so none of its registers can reach the regulators.
pub const RTC_ADDRESS: u8 = 0x0C >> 1;
/// max77686-private.h:145 `MAX77686_RTC_CONTROL`.
const RTC_CONTROL: u8 = 0x03;
/// max77686-private.h:146 `MAX77686_RTC_UPDATE0`.
const RTC_UPDATE0: u8 = 0x04;
/// max77686-private.h:149 `MAX77686_RTC_SEC`, the first of seven: seconds, minutes, hours,
/// weekday, month, year, date.
const RTC_SEC: u8 = 0x07;
/// rtc-max77686.c:36 `RTC_RBUDR_MASK`: copies the counters to the registers that are read.
const RTC_RBUDR: u8 = 1 << 4;
/// rtc-max77686.c:193 `.delay`: how long that copy takes, in microseconds.
const RTC_UPDATE_US: u64 = 16_000;
/// rtc-max77686.c:27,29 `BCD_EN_MASK`, `MODEL24_MASK`, in `RTC_CONTROL`.
const RTC_BCD: u8 = 1 << 0;
const RTC_24_HOUR: u8 = 1 << 1;
/// rtc-max77686.c:37 `HOUR_PM_MASK`.
const RTC_PM: u8 = 1 << 6;
/// rtc-max77686.c:194 `.mask`: bit 7 of each time register enables an alarm field instead.
const RTC_MASK: u8 = 0x7F;

/// The RTC registers as one read gave them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcReading {
    /// `RTC_CONTROL`: bit 0 set for BCD, bit 1 set for 24-hour mode. Linux sets binary and
    /// 24-hour mode (`max77686_rtc_init_reg`).
    pub control: u8,
    /// Seconds, minutes, hours, weekday, month, year, date: the order of the registers.
    pub raw: [u8; 7],
}

impl RtcReading {
    /// The time it holds, decoded as `max77686_rtc_data_to_tm` does, with years from 2000.
    /// `None` if a field is out of range, as after the RTC lost its supply.
    pub fn time(&self) -> Option<DateTime> {
        let bcd = self.control & RTC_BCD != 0;
        let field = |value: u8| {
            let value = value & RTC_MASK;
            if bcd { (value >> 4) * 10 + (value & 0xF) } else { value }
        };
        let [second, minute, hour, _weekday, month, year, date] = self.raw;
        let hour = if self.control & RTC_24_HOUR != 0 {
            field(hour & 0x3F)
        } else {
            field(hour & 0x1F) % 12 + if hour & RTC_PM != 0 { 12 } else { 0 }
        };
        let time = DateTime {
            year: 2000 + u16::from(field(year)),
            month: field(month & 0x1F),
            day: field(date & 0x3F),
            hour,
            minute: field(minute),
            second: field(second),
        };
        time.is_valid().then_some(time)
    }
}

/// The MAX77686's real-time clock, which runs from the battery while the board is off.
#[derive(Clone, Copy, Debug)]
pub struct Rtc {
    i2c: I2c,
}

impl Rtc {
    /// Creates a handle on the bus the PMIC is on.
    pub const fn new(i2c: I2c) -> Self {
        Self { i2c }
    }

    fn read_register(&self, register: u8) -> Result<u8, Error> {
        let mut value = [0];
        self.i2c.write_read(RTC_ADDRESS, &[register], &mut value)?;
        Ok(value[0])
    }

    /// Reads the time. The one write is to `RTC_UPDATE0`, which copies the counters to the
    /// registers read after it (`max77686_rtc_update`); then waits 16 ms on `mct`, which must
    /// be running.
    pub fn read(&self, mct: Mct) -> Result<RtcReading, Error> {
        let update = self.read_register(RTC_UPDATE0)?;
        self.i2c.write(RTC_ADDRESS, &[RTC_UPDATE0, update | RTC_RBUDR])?;
        let wait = RTC_UPDATE_US * u64::from(soc::MCT_CLOCK_HZ / 1_000_000);
        let begin = mct.count();
        while mct.count() - begin < wait {}
        let control = self.read_register(RTC_CONTROL)?;
        let mut raw = [0; 7];
        self.i2c.write_read(RTC_ADDRESS, &[RTC_SEC], &mut raw)?;
        Ok(RtcReading { control, raw })
    }
}
