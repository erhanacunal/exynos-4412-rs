//! Light standby, the way a phone sleeps when its power key is pressed: the screen goes dark,
//! the CPU slows down, and the cores sleep until the power key is pressed again.
//!
//! [`enter`] turns the backlight, the LVDS transmitter and the FIMD's output off, moves the CPU
//! to [`STANDBY_MHZ`] at its lowest voltage ([`cpufreq`]), and sleeps in `wfi` with the power
//! key's interrupt (`gpx1-3`, `exynos4412-mehmet.dts:70-74`) enabled in the GIC. On a short
//! press it puts everything back as it was. [`PowerButton`] tells a short press from a long one
//! while the program runs.
//!
//! The chip stays powered, and DRAM keeps running: this is not suspend to RAM, and needs nothing
//! from the bootloader.
//!
//! ```ignore
//! let mut button = PowerButton::new();
//! loop {
//!     match button.poll(mct) {
//!         Some(Press::Short) => { standby::enter(mct, Some(pmic), &mut button)?; }
//!         Some(Press::Long) => power::power_off(),
//!         None => {}
//!     }
//! }
//! ```
//!
//! # What the caller does
//!
//! Only the display and the CPU clock are handled here. The rest the program turns off before
//! and on after, as only it knows what runs: the touch controller, audio, WiFi
//! (`bcm4334::power_off`), the GPU. IRQs stay masked in the CPSR on the calling core while it
//! sleeps, so no handler runs on it; an interrupt the program left enabled in the GIC still wakes
//! it, and a periodic one keeps it from sleeping much: [`Report::slept_us`] tells. The other
//! cores keep running at the slower clock; they should be idle in `wfi` themselves.

use crate::drivers::display::{self, Fimd, LVDS_SETTLE_MS};
use crate::drivers::gpio::{Eint, Pull, Trigger};
use crate::drivers::max77686::Max77686;
use crate::drivers::power::POWER_KEY;
use crate::drivers::timer::Mct;
use crate::{cpufreq, sleep, soc};

/// The CPU clock in standby: the lowest point of [`crate::drivers::cmu::ARM_POINTS`], 0.9 V.
pub const STANDBY_MHZ: u32 = 200;
/// A press shorter than this is contact bounce, not a press.
const DEBOUNCE_MS: u64 = 30;
/// A press held this long is a [`Press::Long`], reported while still held.
pub const LONG_PRESS_MS: u64 = 2000;
/// How often the key is looked at while held, during [`enter`].
const HELD_POLL_MS: u64 = 10;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
/// The power key's wake-up interrupt: GPX pins all have one.
const POWER_EINT: Eint = match POWER_KEY.eint() {
    Some(eint) => eint,
    None => panic!("the power key is a GPX pin"),
};

/// A press of the power key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Down for at least [`DEBOUNCE_MS`] and up again before [`LONG_PRESS_MS`]; reported on release.
    Short,
    /// Down for [`LONG_PRESS_MS`]; reported once, while still held.
    Long,
}

/// Tells presses of the power key apart. Call [`poll`](Self::poll) often, every few
/// milliseconds; [`enter`] does while it sleeps.
#[derive(Debug)]
pub struct PowerButton {
    down_since: Option<u64>,
    long_sent: bool,
}

impl PowerButton {
    /// Makes the key an input with its pull-up, as `exynos4412-mehmet.dts:608-627` does.
    pub fn new() -> Self {
        POWER_KEY.set_input(Pull::Up);
        Self { down_since: None, long_sent: false }
    }

    /// Whether the key is down now. Active low.
    pub fn is_down(&self) -> bool {
        !POWER_KEY.read()
    }

    /// Looks at the key, and returns a press once it is one.
    pub fn poll(&mut self, mct: Mct) -> Option<Press> {
        let now = mct.count();
        if self.is_down() {
            let since = *self.down_since.get_or_insert(now);
            if !self.long_sent && now - since >= LONG_PRESS_MS * COUNTS_PER_MS {
                self.long_sent = true;
                return Some(Press::Long);
            }
            return None;
        }
        let since = self.down_since.take()?;
        let long_sent = core::mem::replace(&mut self.long_sent, false);
        (!long_sent && now - since >= DEBOUNCE_MS * COUNTS_PER_MS).then_some(Press::Short)
    }
}

impl Default for PowerButton {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`enter`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// The press that ended it.
    pub press: Press,
    /// How long it lasted, in microseconds.
    pub standby_us: u64,
    /// How much of that the calling core spent in `wfi`.
    pub slept_us: u64,
    /// Wake-ups of the calling core.
    pub wakes: u32,
    /// `armclk` before, in standby, and after, in Hz. Before and after are the same unless a
    /// clock change failed.
    pub before_hz: u32,
    /// See `before_hz`.
    pub standby_hz: u32,
    /// See `before_hz`.
    pub after_hz: u32,
}

/// Why [`enter`] could not finish. The display is back on in every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The CPU clock could not go back: the CPU stays at the rate this holds, in Hz.
    Restore(cpufreq::Error, u32),
}

/// Goes into standby until the power key is pressed, then comes back. With `pmic`, the CPU runs
/// at [`STANDBY_MHZ`] meanwhile; without it, or if that fails, at its rate. Call it when a press
/// has been released, as [`PowerButton::poll`] reports a [`Press::Short`]; a key still down
/// would end it at once. A long press ends it too: [`Report::press`] tells, and the caller
/// decides, for example to power off. See the [module documentation](self).
pub fn enter(mct: Mct, pmic: Option<Max77686>, button: &mut PowerButton) -> Result<Report, Error> {
    let start = mct.count();
    let slept_before = sleep::slept();

    // Screen off, in the reverse of the order it goes on.
    let brightness = display::brightness();
    let fimd = Fimd::new();
    let fimd_was_running = fimd.is_running();
    display::set_brightness(0);
    display::set_lvds(false);
    if fimd_was_running {
        fimd.stop();
    }

    let before_hz = cpufreq::arm_hz();
    let slowed = pmic.filter(|_| before_hz != STANDBY_MHZ * 1_000_000).and_then(|pmic| {
        cpufreq::set_arm_mhz(pmic, mct, STANDBY_MHZ).ok().map(|_| pmic)
    });
    let standby_hz = cpufreq::arm_hz();

    let (press, wakes) = wait_press(mct, button);

    let restored = match slowed {
        Some(pmic) => cpufreq::set_arm_mhz(pmic, mct, before_hz / 1_000_000).map(|_| ()),
        None => Ok(()),
    };
    let after_hz = cpufreq::arm_hz();

    if fimd_was_running {
        fimd.resume();
    }
    display::set_lvds(true);
    sleep::for_us(mct, u64::from(LVDS_SETTLE_MS) * 1000);
    display::set_brightness(brightness);

    restored.map_err(|error| Error::Restore(error, after_hz))?;
    Ok(Report {
        press,
        standby_us: (mct.count() - start) / COUNTS_PER_US,
        slept_us: (sleep::slept() - slept_before) / COUNTS_PER_US,
        wakes,
        before_hz,
        standby_hz,
        after_hz,
    })
}

/// Sleeps until `button` reports a press. The key's interrupt, on both edges, wakes the core;
/// while the key is down, so does an alarm every [`HELD_POLL_MS`], to time the press. Leaves
/// the key a plain input again.
fn wait_press(mct: Mct, button: &mut PowerButton) -> (Press, u32) {
    let eint = POWER_EINT;
    eint.listen(Trigger::BothEdges);
    let mut wakes = 0;
    let press = loop {
        if let Some(press) = button.poll(mct) {
            break press;
        }
        let deadline = button.is_down().then(|| mct.count() + HELD_POLL_MS * COUNTS_PER_MS);
        sleep::until(mct, &[eint.irq()], deadline, || {
            let edge = eint.is_pending();
            if edge {
                eint.clear();
            }
            edge
        });
        wakes += 1;
    };
    eint.mask();
    eint.clear();
    POWER_KEY.set_input(Pull::Up);
    (press, wakes)
}
