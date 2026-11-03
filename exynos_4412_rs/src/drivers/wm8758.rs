//! Wolfson WM8758 audio codec, on `i2c_1` at [`ADDRESS`]: its power-up and the path from the
//! DAC to the headphones.
//!
//! Paths are relative to the Linux 5.15.198 tree. `exynos4412-mehmet.dts:1245-1262` binds the
//! codec to `sound/soc/codecs/wm8985.c` as a WM8758; its registers are in `wm8985.h` in the
//! same directory, which this cites by line. The vendor kernel uses the same driver.
//!
//! The codec's registers are 9 bits wide and cannot be read back: each write sends the 7-bit
//! register number and the value in two bytes (`regmap` with 7 address bits and 9 value bits,
//! `wm8985.c:1123-1124`). The codec acknowledging is all that can be checked.
//!
//! The codec is a clock slave: I2S0 sends it the master clock on CDCLK, 256 times the frame
//! rate, and BCLK and LRCLK. [`Wm8758::start_headphones`] takes the codec from reset to
//! sound on LOUT1 and ROUT1, the headphone outputs. [`Wm8758::set_speakers`] adds LOUT2 and
//! ROUT2, which feed the board's speaker amplifier, and turns the amplifier on. The inputs
//! stay off.

use crate::drivers::gpio::{self, Pin, Pull};
use crate::drivers::i2c::{self, Bus, I2c};
use crate::drivers::max77686::{self, Max77686};
use crate::drivers::timer::Mct;
use crate::soc;

/// `exynos4412-mehmet.dts:1245-1247`, `codec@1a` on `i2c_1`.
pub const ADDRESS: u8 = 0x1A;
/// `samsung,i2c-max-bus-freq` of `i2c_1`, `exynos4412-mehmet.dts:1237`.
pub const BUS_HZ: u32 = 100_000;
/// `samsung,i2c-sda-delay` of `i2c_1`, `exynos4412-mehmet.dts:1235`.
pub const BUS_SDA_DELAY_NS: u32 = 100;

/// `vdd_audio`, all four supplies of the codec: `exynos4412-mehmet.dts:1144-1147,1249-1252`.
pub const LDO_AUDIO: (u8, u16) = (25, 3300);
/// `exynos4412-mehmet.dts:833-838`, `gpio-mehmet.h:38`: Linux drives it high.
pub const POP_SUPPRESSOR: Pin = gpio::GPM1.pin(3);
/// `exynos4412-mehmet.dts:826-831`, `gpio-mehmet.h:37`: the speaker amplifier, on when high.
pub const SPEAKER_AMP: Pin = gpio::GPM1.pin(2);
/// `exynos4412-mehmet.dts:137,840-844`, `gpio-mehmet.h:17`: low while a jack is in.
pub const JACK_DETECT: Pin = gpio::GPX1.pin(6);

/// Longer than the rail needs; `pmic_test` saw the codec answer after it.
const RAIL_SETTLE_MS: u64 = 10;
/// `wm8985.c:992`: VMID charging with the anti-pop features on.
const VMID_CHARGE_MS: u64 = 500;

/// `wm8985.h:13-25,50,57-63,67`.
const SOFTWARE_RESET: u8 = 0x00;
const POWER_1: u8 = 0x01;
const POWER_2: u8 = 0x02;
const POWER_3: u8 = 0x03;
const AUDIO_INTERFACE: u8 = 0x04;
const CLOCK_GEN: u8 = 0x06;
const ADDITIONAL: u8 = 0x07;
const DAC_CONTROL: u8 = 0x0A;
const LEFT_DAC_VOLUME: u8 = 0x0B;
const RIGHT_DAC_VOLUME: u8 = 0x0C;
const OUT4_TO_ADC: u8 = 0x2A;
const OUTPUT_CTRL0: u8 = 0x31;
const LEFT_MIXER: u8 = 0x32;
const RIGHT_MIXER: u8 = 0x33;
const LOUT1_VOLUME: u8 = 0x34;
const ROUT1_VOLUME: u8 = 0x35;
const LOUT2_VOLUME: u8 = 0x36;
const ROUT2_VOLUME: u8 = 0x37;
const BIAS_CTRL: u8 = 0x3D;

/// `wm8985.h:102,106,111`: VMID through 75k is `VMIDSEL` 1 (`wm8985.c:957-960`).
const BIASEN: u16 = 0x008;
const BUFIOEN: u16 = 0x004;
const VMID_75K: u16 = 0x001;
/// `wm8985.h:118,122`.
const ROUT1EN: u16 = 0x100;
const LOUT1EN: u16 = 0x080;
/// `wm8985.h:166,170`.
const ROUT2EN: u16 = 0x040;
const LOUT2EN: u16 = 0x020;
/// `wm8985.h:174-186`.
const RMIXEN: u16 = 0x008;
const LMIXEN: u16 = 0x004;
const DACENR: u16 = 0x002;
const DACENL: u16 = 0x001;
/// `wm8985.h:202-206`: I2S is `FMT` 2, 16 bits is `WL` 0 (`wm8985.c:764-767`).
const FORMAT_I2S_16BIT: u16 = 2 << 3;
/// `wm8985.h:242-252`: `CLKSEL` 0 takes MCLK, not the PLL; `MS` 0 is slave.
const MCLKDIV_SHIFT: u16 = 5;
/// `wm8985.h:267`, and `wm8985.c:208`: 48 kHz is `SR` 0.
const SR_48K: u16 = 0;
/// `wm8985.h:321,325`.
const SOFTMUTE: u16 = 0x040;
const DACOSR128: u16 = 0x008;
/// `wm8985.h:345`: `DACVU` latches both DAC volumes. 0xFF is 0 dB (`dac_tlv`, `wm8985.c:219`: -127 dB at 1, 0.5 dB a step).
const DACVU: u16 = 0x100;
const DAC_0DB: u16 = 0x0FF;
/// `wm8985.h:664`.
const POBCTRL: u16 = 0x004;
/// `wm8985.h:840,844`.
const TSOPCTRL: u16 = 0x004;
const TSDEN: u16 = 0x002;
/// `wm8985.h:870,892`, the reset values of both mixers.
const DAC_TO_MIXER: u16 = 0x001;
/// `wm8985.h:899,908,912`: `OUT1VU` latches both headphone volumes.
const OUT1VU: u16 = 0x100;
const OUT1MUTE: u16 = 0x040;
/// `wm8985.h:938,946`, the same bits for LOUT2 and ROUT2.
const OUT2VU: u16 = 0x100;
const OUT2MUTE: u16 = 0x040;
/// `wm8985.h:1047`, set by `wm8985.c:1061-1063`.
const BIASCUT: u16 = 0x100;

/// The volume field of each output: 0 is -57 dB, 1 dB a step (`out_tlv`, `wm8985.c:221`,
/// for the headphones and speakers alike, `wm8985.c:346-354`), 0x39 is 0 dB.
const OUT_RAW_0DB: i16 = 0x39;
/// The lowest output volume, in dB.
pub const OUT_MIN_DB: i8 = -57;
/// The highest output volume, in dB.
pub const OUT_MAX_DB: i8 = 6;
/// The mixers and DACs, always on while playing.
const POWER_3_PLAYING: u16 = DACENL | DACENR | LMIXEN | RMIXEN;

/// Why a call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The I2C transfer failed.
    Bus(i2c::Error),
    /// Turning on the codec's rail failed.
    Pmic(max77686::Error),
}

impl From<i2c::Error> for Error {
    fn from(error: i2c::Error) -> Self {
        Self::Bus(error)
    }
}

impl From<max77686::Error> for Error {
    fn from(error: max77686::Error) -> Self {
        Self::Pmic(error)
    }
}

/// The master clock divider (`MCLKDIV`, `wm8985.h:246`), between MCLK and the codec's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MclkDiv {
    /// MCLK as it comes.
    Div1 = 0,
    /// Half of MCLK: what both Linux and the vendor kernel set for MCLK at 256 times the frame
    /// rate (`wm8985.c:197-206,806-820`).
    Div2 = 2,
}

fn wait_ms(mct: Mct, ms: u64) {
    let end = mct.count() + ms * u64::from(soc::MCT_CLOCK_HZ / 1000);
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

/// Turns on the codec's rail, sets its pins as Linux does, and checks that it answers. Leaves
/// the speaker amplifier off. `pmic` is on `i2c_0`, initialized.
pub fn power_up(pmic: Max77686, mct: Mct) -> Result<Wm8758, Error> {
    let (ldo, millivolts) = LDO_AUDIO;
    pmic.enable_ldo(ldo, millivolts)?;
    wait_ms(mct, RAIL_SETTLE_MS);
    SPEAKER_AMP.set_output(false);
    POP_SUPPRESSOR.set_output(true);
    JACK_DETECT.set_input(Pull::None);
    let i2c = I2c::new(Bus::I2c1);
    i2c.init(BUS_HZ, BUS_SDA_DELAY_NS)?;
    i2c.probe(ADDRESS)?;
    Ok(Wm8758 { i2c })
}

/// Whether a jack is in the headphone socket.
pub fn jack_in() -> bool {
    !JACK_DETECT.read()
}

fn volume_raw(db: i8) -> u16 {
    (OUT_RAW_0DB + i16::from(db.clamp(OUT_MIN_DB, OUT_MAX_DB))) as u16
}

/// The codec, answering on its bus.
#[derive(Clone, Copy, Debug)]
pub struct Wm8758 {
    i2c: I2c,
}

impl Wm8758 {
    /// Writes `value`, 9 bits, to `register`.
    pub fn write(&self, register: u8, value: u16) -> Result<(), Error> {
        assert!(register < 0x80 && value < 0x200);
        self.i2c.write(ADDRESS, &[register << 1 | (value >> 8) as u8, value as u8])?;
        Ok(())
    }

    /// Puts every register back to its reset value (`wm8985.c:647-650`).
    pub fn reset(&self) -> Result<(), Error> {
        self.write(SOFTWARE_RESET, 0)
    }

    /// From reset to the DAC on the headphones at `db` (see
    /// [`set_headphone_db`](Self::set_headphone_db)), for 16-bit I2S at 48 kHz with MCLK
    /// divided by `mclk`. Follows the anti-pop order of `wm8985.c:975-1000`: bias and VMID
    /// first, then the outputs, muted until they are on. Takes [`VMID_CHARGE_MS`]. MCLK should
    /// run already.
    pub fn start_headphones(&self, mct: Mct, mclk: MclkDiv, db: i8) -> Result<(), Error> {
        self.reset()?;
        self.write(BIAS_CTRL, BIASCUT)?;
        self.write(OUT4_TO_ADC, POBCTRL)?;
        self.write(OUTPUT_CTRL0, TSDEN | TSOPCTRL)?;
        self.write(POWER_1, BIASEN | BUFIOEN | VMID_75K)?;
        wait_ms(mct, VMID_CHARGE_MS);
        self.write(OUT4_TO_ADC, 0)?;

        self.write(AUDIO_INTERFACE, FORMAT_I2S_16BIT)?;
        self.set_mclk_div(mclk)?;
        self.write(ADDITIONAL, SR_48K)?;
        self.write(DAC_CONTROL, DACOSR128 | SOFTMUTE)?;
        self.write(LEFT_DAC_VOLUME, DAC_0DB)?;
        self.write(RIGHT_DAC_VOLUME, DACVU | DAC_0DB)?;
        self.write(LEFT_MIXER, DAC_TO_MIXER)?;
        self.write(RIGHT_MIXER, DAC_TO_MIXER)?;
        self.headphone_volume(db, true)?;
        self.write(POWER_3, POWER_3_PLAYING)?;
        self.write(POWER_2, LOUT1EN | ROUT1EN)?;
        self.headphone_volume(db, false)?;
        self.set_mute(false)
    }

    /// Sets the master clock divider.
    pub fn set_mclk_div(&self, mclk: MclkDiv) -> Result<(), Error> {
        self.write(CLOCK_GEN, (mclk as u16) << MCLKDIV_SHIFT)
    }

    /// Mutes the DAC softly, or lets it through.
    pub fn set_mute(&self, mute: bool) -> Result<(), Error> {
        self.write(DAC_CONTROL, DACOSR128 | if mute { SOFTMUTE } else { 0 })
    }

    /// Sets both headphone outputs to `db`, limited to [`OUT_MIN_DB`]..=[`OUT_MAX_DB`]. Returns
    /// the volume set.
    pub fn set_headphone_db(&self, db: i8) -> Result<i8, Error> {
        let db = db.clamp(OUT_MIN_DB, OUT_MAX_DB);
        self.headphone_volume(db, false)?;
        Ok(db)
    }

    fn headphone_volume(&self, db: i8, mute: bool) -> Result<(), Error> {
        let raw = volume_raw(db);
        let mute = if mute { OUT1MUTE } else { 0 };
        // The left one waits for the right one's update bit.
        self.write(LOUT1_VOLUME, raw | mute)?;
        self.write(ROUT1_VOLUME, OUT1VU | raw | mute)
    }

    fn speaker_volume(&self, db: i8, mute: bool) -> Result<(), Error> {
        let raw = volume_raw(db);
        let mute = if mute { OUT2MUTE } else { 0 };
        self.write(LOUT2_VOLUME, raw | mute)?;
        self.write(ROUT2_VOLUME, OUT2VU | raw | mute)
    }

    /// Turns the speaker outputs and the amplifier on at `db`, limited as in
    /// [`set_headphone_db`](Self::set_headphone_db), or both off. The outputs come on muted and
    /// the amplifier after them; off, the amplifier goes first. Call it after
    /// [`start_headphones`](Self::start_headphones). Returns the volume set.
    pub fn set_speakers(&self, on: bool, db: i8) -> Result<i8, Error> {
        let db = db.clamp(OUT_MIN_DB, OUT_MAX_DB);
        if on {
            self.speaker_volume(db, true)?;
            self.write(POWER_3, POWER_3_PLAYING | LOUT2EN | ROUT2EN)?;
            SPEAKER_AMP.write(true);
            self.speaker_volume(db, false)?;
        } else {
            self.speaker_volume(OUT_MIN_DB, true)?;
            SPEAKER_AMP.write(false);
            self.write(POWER_3, POWER_3_PLAYING)?;
        }
        Ok(db)
    }

    /// Sets both speaker outputs to `db`, limited as in
    /// [`set_headphone_db`](Self::set_headphone_db). Returns the volume set.
    pub fn set_speaker_db(&self, db: i8) -> Result<i8, Error> {
        let db = db.clamp(OUT_MIN_DB, OUT_MAX_DB);
        self.speaker_volume(db, false)?;
        Ok(db)
    }

    /// Mutes, then turns the outputs, mixers, DAC and bias off, in the reverse order of
    /// [`start_headphones`](Self::start_headphones).
    pub fn stop(&self) -> Result<(), Error> {
        self.set_mute(true)?;
        self.set_speakers(false, OUT_MIN_DB)?;
        self.headphone_volume(OUT_MIN_DB, true)?;
        self.write(POWER_2, 0)?;
        self.write(POWER_3, 0)?;
        self.write(POWER_1, 0)
    }
}
