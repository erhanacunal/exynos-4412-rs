//! The PWM timers at [`soc::PWM_BASE`], as square-wave outputs: channels 0 to 3 drive the pins
//! `TOUT0` to `TOUT3` (channel 4 has none). The board's backlight is on channel 1, `gpd0-1`
//! (`exynos4412-mehmet.dts`, `backlight`): see [`display::set_brightness`].
//!
//! Registers and fields: Linux 5.15.198 `drivers/pwm/pwm-samsung.c:29-56`. Each channel counts
//! down from `TCNTB` to 0 and reloads; its output changes when the count passes `TCMPB`. With the
//! inverter on, as the vendor kernel runs the backlight (`willow`
//! `arch/arm/plat-samsung/pwm.c:252`), the output is high for `TCNTB - TCMPB` ticks of every
//! `TCNTB + 1`.
//!
//! The timers run from `aclk100` undivided here: prescaler 0 for both groups of channels, mux
//! 1/1. A 32-bit count then reaches periods of 40 s.
//!
//! [`display::set_brightness`]: crate::drivers::display::set_brightness

use core::ptr;

use crate::drivers::cmu::Cmu;
use crate::{cpu, soc};

// pwm-samsung.c:29-56.
const TCFG0: usize = 0x00;
const TCFG1: usize = 0x04;
const TCON: usize = 0x08;
const fn tcntb(channel: usize) -> usize {
    0x0C + channel * 0x0C
}
const fn tcmpb(channel: usize) -> usize {
    0x10 + channel * 0x0C
}
/// `TCFG0`: the prescaler of channels 0 and 1, bits 0-7, and of 2 to 4, bits 8-15.
const TCFG0_PRESCALERS: u32 = 0xFFFF;
/// `TCFG1`: each channel's divider, 4 bits at 4 * channel; 0 is 1/1.
const TCFG1_MUX_MASK: u32 = 0xF;
/// `TCON`: channel 0's bits at 0, the others' at 4 * (channel + 1) (`to_tcon_channel`).
const TCON_START: u32 = 1 << 0;
const TCON_MANUAL_UPDATE: u32 = 1 << 1;
const TCON_INVERT: u32 = 1 << 2;
const TCON_AUTORELOAD: u32 = 1 << 3;
const CHANNELS: usize = 4;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The rate of `aclk100` is unknown: see [`Cmu::aclk100_hz`].
    UnknownClock,
    /// The period is under two ticks or over the counter's reach, or the channel has no pin.
    BadPeriod,
}

/// The shift of a channel's bits in `TCON`.
const fn tcon_shift(channel: usize) -> u32 {
    if channel == 0 { 0 } else { 4 * (channel as u32 + 1) }
}

fn read(offset: usize) -> u32 {
    // SAFETY: every offset is a register of the PWM block, device memory that stays mapped for
    // the whole program.
    unsafe { ptr::read_volatile((soc::PWM_BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as in `read`.
    unsafe { ptr::write_volatile((soc::PWM_BASE + offset) as *mut u32, value) }
}

/// Changes bits of a register shared by the channels, with interrupts off meanwhile.
fn modify(offset: usize, clear: u32, set: u32) {
    let was_masked = cpu::mask_irq();
    write(offset, (read(offset) & !clear) | set);
    cpu::restore_irq(was_masked);
}

/// Runs channel `channel` (0 to 3) with a period of `period_ns`, high for `high_ns` of it
/// (clamped to between one tick and one tick short of the period). The pin must be in its
/// `TOUT` function for the wave to leave the chip. The new values take over at once.
pub fn start(channel: usize, period_ns: u32, high_ns: u32) -> Result<(), Error> {
    if channel >= CHANNELS {
        return Err(Error::BadPeriod);
    }
    let cmu = Cmu::new();
    let hz = cmu.aclk100_hz().ok_or(Error::UnknownClock)?;
    cmu.open_pwm_gate();
    let ticks = u64::from(hz) * u64::from(period_ns) / 1_000_000_000;
    if !(2..=u64::from(u32::MAX)).contains(&ticks) {
        return Err(Error::BadPeriod);
    }
    let ticks = ticks as u32;
    let high = ((u64::from(hz) * u64::from(high_ns.min(period_ns)) / 1_000_000_000) as u32).clamp(1, ticks - 1);

    modify(TCFG0, TCFG0_PRESCALERS, 0);
    modify(TCFG1, TCFG1_MUX_MASK << (4 * channel), 0);
    write(tcntb(channel), ticks - 1);
    write(tcmpb(channel), ticks - 1 - high);
    // As `pwm_samsung_enable` (`pwm-samsung.c:247-256`): the buffers in by hand, then running.
    let shift = tcon_shift(channel);
    modify(TCON, (TCON_START | TCON_AUTORELOAD) << shift, (TCON_MANUAL_UPDATE | TCON_INVERT) << shift);
    modify(TCON, TCON_MANUAL_UPDATE << shift, (TCON_START | TCON_AUTORELOAD) << shift);
    Ok(())
}

/// Stops channel `channel` (0 to 3) at once. What its pin shows then is up to its function.
pub fn stop(channel: usize) {
    if channel >= CHANNELS || !Cmu::new().pwm_gate_open() {
        return;
    }
    modify(TCON, (TCON_START | TCON_AUTORELOAD) << tcon_shift(channel), 0);
}
