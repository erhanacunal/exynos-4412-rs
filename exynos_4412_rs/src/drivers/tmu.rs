//! Thermal management unit: the chip's temperature sensor.
//!
//! The Exynos 4412 has one sensor on the die. It is the only on-chip temperature: the CPU
//! cores, the GPU and the rest share it, as the one `cpu-thermal` zone of
//! `exynos4412-mehmet.dts:101-119` does.
//!
//! Register offsets and settings are verified against Linux 5.15.198
//! `drivers/thermal/samsung/exynos_tmu.c`, cited as `exynos_tmu.c:<line>`, relative to
//! [`soc::TMU_BASE`]. The node is `exynos4412.dtsi:811-819`; its supply, `vtmu-supply`, is LDO10
//! (`exynos4412-mehmet.dts:1577-1580`), which is `always-on` and on after U-Boot.
//!
//! # Calibration
//!
//! The sensor gives an 8-bit code. `TRIMINFO` holds the code that was measured at 25 °C in the
//! factory. Linux converts with that one point, at 1 °C a code (`exynos_tmu.c:227-236`; it sets
//! one-point trimming for every SoC but the 5433, `:976`), and so does [`Tmu::celsius`]. A
//! fused value outside 40..=100 is taken as missing and replaced with 55, as Linux does
//! (`:238-254`, `:928-936`).
//!
//! # Interrupts and the hardware trip
//!
//! Nothing here enables an interrupt or sets a threshold. `THERM_TRIP_EN`, which lets the TMU
//! cut the power at a threshold, is left as U-Boot left it: see [`Registers`].
//!
//! # Concurrency
//!
//! [`Tmu::new`] writes the control registers: call it once, from one core. After that the
//! readings are single register reads, from any core.

use core::ptr;

use super::cmu::Cmu;
use super::timer::Mct;
use crate::soc;

/// exynos_tmu.c:29.
const TRIMINFO: usize = 0x00;
/// exynos_tmu.c:52, `EXYNOS_TMU_TRIMINFO_CON2`.
const TRIMINFO_CON2: usize = 0x14;
/// exynos_tmu.c:30.
const CONTROL: usize = 0x20;
/// exynos_tmu.c:31.
const STATUS: usize = 0x28;
/// exynos_tmu.c:32.
const CURRENT_TEMP: usize = 0x40;
/// exynos_tmu.c:33.
const INTEN: usize = 0x70;

/// exynos_tmu.c:57.
const TRIMINFO_RELOAD: u32 = 1;
/// exynos_tmu.c:37, the code fields of `TRIMINFO` and `CURRENT_TEMP`.
const CODE_MASK: u32 = 0xFF;
/// exynos_tmu.c:59, the 85 °C code in `TRIMINFO`.
const TRIMINFO_85_SHIFT: u32 = 8;

/// exynos_tmu.c:42, `EXYNOS_TMU_CORE_EN_SHIFT`.
const CORE_EN: u32 = 1 << 0;
/// exynos_tmu.c:62, `EXYNOS_TMU_THERM_TRIP_EN_SHIFT`; Linux sets it with a critical trip, `:405`.
const THERM_TRIP_EN: u32 = 1 << 12;
/// exynos_tmu.c:40-41 and the 4412 value, `:928`.
const BUF_SLOPE_SHIFT: u32 = 8;
const BUF_SLOPE_MASK: u32 = 0xF;
const BUF_SLOPE: u32 = 8;
/// exynos_tmu.c:60-61 and `EXYNOS_NOISE_CANCEL_MODE`, `:123`.
const TRIP_MODE_SHIFT: u32 = 13;
const TRIP_MODE_MASK: u32 = 0x7;
const TRIP_MODE_NOISE_CANCEL: u32 = 4;
/// exynos_tmu.c:81-82, the 4412 sensor mux.
const MUX_ADDR_SHIFT: u32 = 20;
const MUX_ADDR: u32 = 6;
/// exynos_tmu.c:38-39 and the 4412 value, `:929`.
const REF_VOLTAGE_SHIFT: u32 = 24;
const REF_VOLTAGE_MASK: u32 = 0x1F;
const REF_VOLTAGE: u32 = 16;

/// exynos_tmu.c:120, the temperature of the fused code.
const FIRST_POINT_C: i16 = 25;
/// exynos_tmu.c:930-936: the fallback code and the range a fused one must be in.
const FALLBACK_CODE: u8 = 55;
const MIN_FUSED_CODE: u8 = 40;
const MAX_FUSED_CODE: u8 = 100;

/// How long [`Tmu::new`] waits for the reload of `TRIMINFO`. On the board, a read right after
/// the request still gave the value from before it (`0x000def95`, whose 25 °C code is out of
/// range), and a read a few ms later the fused one (`0x00000033`). Not from the manual either.
const RELOAD_TIMEOUT_MS: u64 = 10;
/// How long [`Tmu::new`] waits for the first reading. Not from the manual, which this was not
/// checked against: only far above a sensing cycle.
const FIRST_READING_TIMEOUT_MS: u64 = 100;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

fn read(offset: usize) -> u32 {
    // SAFETY: every offset above is a register of the TMU, device memory that stays mapped for
    // the whole program. Its clock gate is open before the first access: see `Tmu::new` and
    // `Registers::read`.
    unsafe { ptr::read_volatile((soc::TMU_BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as in `read`. Only `Tmu::new` writes, the TMU's own settings.
    unsafe { ptr::write_volatile((soc::TMU_BASE + offset) as *mut u32, value) }
}

/// The TMU's registers as they are, for reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registers {
    /// Whether the clock gate was open. With it closed the others are not read and are 0.
    pub gate_open: bool,
    /// `TRIMINFO`: the fused codes.
    pub triminfo: u32,
    /// `TRIMINFO_CON2`: the reload request.
    pub triminfo_con2: u32,
    /// `CONTROL`.
    pub control: u32,
    /// `STATUS`.
    pub status: u32,
    /// `CURRENT_TEMP`: the latest code.
    pub current_temp: u32,
    /// `INTEN`: the interrupt enables.
    pub inten: u32,
}

impl Registers {
    /// Reads them without changing anything, the gate included.
    pub fn read() -> Self {
        if !Cmu::new().tmu_gate_open() {
            return Self { gate_open: false, triminfo: 0, triminfo_con2: 0, control: 0, status: 0, current_temp: 0, inten: 0 };
        }
        Self {
            gate_open: true,
            triminfo: read(TRIMINFO),
            triminfo_con2: read(TRIMINFO_CON2),
            control: read(CONTROL),
            status: read(STATUS),
            current_temp: read(CURRENT_TEMP),
            inten: read(INTEN),
        }
    }

    /// Whether the sensor is on.
    pub const fn enabled(&self) -> bool {
        self.control & CORE_EN != 0
    }

    /// Whether the TMU may cut the power at a threshold.
    pub const fn trip_enabled(&self) -> bool {
        self.control & THERM_TRIP_EN != 0
    }
}

/// The factory calibration in `TRIMINFO`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    /// The code at 25 °C, as used: the fused one, or [`FALLBACK_CODE`].
    pub code_25: u8,
    /// The code fused for 85 °C. Not used: see the module documentation.
    pub code_85: u8,
    /// Whether `code_25` came from the fuses.
    pub fused: bool,
    /// How long the reload took until its request bit cleared, in µs; `None` if the bit was
    /// still set after the timeout, which was then waited for in full.
    pub reload_us: Option<u32>,
}

/// The sensor, on and calibrated.
#[derive(Clone, Copy, Debug)]
pub struct Tmu {
    calibration: Calibration,
}

/// Why [`Tmu::new`] has no reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// `CURRENT_TEMP` stayed 0 for [`FIRST_READING_TIMEOUT_MS`].
    NoReading,
}

impl Tmu {
    /// Opens the clock gate, reloads the calibration from the fuses, turns the sensor on with
    /// the 4412 settings of `exynos_tmu.c:332-347` and waits for the first reading. Needs the
    /// MCT running.
    pub fn new(mct: Mct) -> Result<Self, Error> {
        Cmu::new().open_tmu_gate();

        // exynos_tmu.c:428-438. Linux reads `TRIMINFO` straight after; here that was too soon.
        write(TRIMINFO_CON2, read(TRIMINFO_CON2) | TRIMINFO_RELOAD);
        let start = mct.count();
        let reload_us = loop {
            let elapsed = mct.count() - start;
            if read(TRIMINFO_CON2) & TRIMINFO_RELOAD == 0 {
                break Some((elapsed / (COUNTS_PER_MS / 1000)) as u32);
            }
            if elapsed > RELOAD_TIMEOUT_MS * COUNTS_PER_MS {
                break None;
            }
        };
        let calibration = calibration(read(TRIMINFO), reload_us);

        let mut control = read(CONTROL) | (MUX_ADDR << MUX_ADDR_SHIFT);
        control &= !(REF_VOLTAGE_MASK << REF_VOLTAGE_SHIFT);
        control |= REF_VOLTAGE << REF_VOLTAGE_SHIFT;
        control &= !(BUF_SLOPE_MASK << BUF_SLOPE_SHIFT);
        control |= BUF_SLOPE << BUF_SLOPE_SHIFT;
        control &= !(TRIP_MODE_MASK << TRIP_MODE_SHIFT);
        control |= TRIP_MODE_NOISE_CANCEL << TRIP_MODE_SHIFT;
        write(INTEN, 0);
        write(CONTROL, control | CORE_EN);

        let start = mct.count();
        while read(CURRENT_TEMP) & CODE_MASK == 0 {
            if mct.count() - start > FIRST_READING_TIMEOUT_MS * COUNTS_PER_MS {
                return Err(Error::NoReading);
            }
        }
        Ok(Self { calibration })
    }

    /// The calibration in use.
    pub fn calibration(&self) -> Calibration {
        self.calibration
    }

    /// The latest code from the sensor.
    pub fn code(&self) -> u8 {
        (read(CURRENT_TEMP) & CODE_MASK) as u8
    }

    /// The chip's temperature in °C, from the latest code.
    pub fn celsius(&self) -> i16 {
        code_to_celsius(self.calibration, self.code())
    }
}

/// exynos_tmu.c:238-254.
fn calibration(triminfo: u32, reload_us: Option<u32>) -> Calibration {
    let fused_25 = (triminfo & CODE_MASK) as u8;
    let code_85 = ((triminfo >> TRIMINFO_85_SHIFT) & CODE_MASK) as u8;
    let fused = (MIN_FUSED_CODE..=MAX_FUSED_CODE).contains(&fused_25);
    Calibration { code_25: if fused { fused_25 } else { FALLBACK_CODE }, code_85, fused, reload_us }
}

/// exynos_tmu.c:229-230, one-point trimming.
fn code_to_celsius(calibration: Calibration, code: u8) -> i16 {
    i16::from(code) - i16::from(calibration.code_25) + FIRST_POINT_C
}
