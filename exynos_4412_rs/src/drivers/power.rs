//! Restart and power off, from code or from the board's keys.
//!
//! Register offsets are verified against Linux 5.15.198
//! `include/linux/soc/samsung/exynos-regs-pmu.h`, cited as `exynos-regs-pmu.h:<line>`, relative
//! to [`soc::PMU_BASE`]. The boot modes and the meaning of `INFORM5` and `INFORM6` come from the
//! board's vendor kernel, `/home/developer/willow`, `arch/arm/mach-exynos/mach-mehmet.c`, cited
//! as `mach-mehmet.c:<line>`. U-Boot reads them, and its source is not available.
//!
//! # Keys
//!
//! [`PowerKeys::service`] has to be called often, from an application's main loop. It does not
//! run on its own: an application that has halted or hung still needs the long hardware press.
//!
//! # Concurrency
//!
//! Any core can restart or power off. The other cores simply stop with the rest of the chip.

use core::ptr;

use super::gpio::{self, Pin, Pull};
use super::timer::Mct;
use crate::{console, println, soc};

/// exynos-regs-pmu.h:42 `EXYNOS_SWRESET`.
const SWRESET: usize = 0x0400;
/// exynos-regs-pmu.h:61 `S5P_INFORM5`: the boot mode for U-Boot, mach-mehmet.c:980-1001.
const INFORM5: usize = 0x0814;
/// exynos-regs-pmu.h:62 `S5P_INFORM6`: whether U-Boot enters its charger mode.
const INFORM6: usize = 0x0818;
/// exynos-regs-pmu.h:170 `S5P_PS_HOLD_CONTROL`.
const PS_HOLD_CONTROL: usize = 0x330C;
/// The bit mach-mehmet.c:972 clears to power off, mask `0xFFFFFEFF`. The value that
/// `exynos-syscon-restart.dtsi:11` writes, `0x5200`, has it clear as well.
const PS_HOLD_HIGH: u32 = 1 << 8;

/// mach-mehmet.c:1005-1007: with this in `INFORM6`, U-Boot does not enter its charger mode.
const INFORM6_NO_CHARGER_MODE: u32 = 3;
/// mach-mehmet.c:965-967: what the vendor kernel leaves in `INFORM6` to "power off" with a charger
/// connected, a restart which U-Boot takes to its charger mode.
const INFORM6_CHARGER_MODE: u32 = 2;

/// Reads of a PMU register to wait while a UART drains, like the 50 ms of mach-mehmet.c:1008.
/// Not a measured time: it only has to be well above the few ms the FIFO takes.
const SETTLE_POLLS: u32 = 500_000;
/// Reads of a PMU register to wait for the power to fall. A few seconds, also not measured.
const POWER_FALL_POLLS: u32 = 30_000_000;
/// `arch/arm/mach-exynos/cpu-exynos4.c:411-418` in the vendor kernel repeats the reset request
/// three times. It normally takes effect on the first.
const RESET_ATTEMPTS: u32 = 3;

/// Power key: `exynos4412-mehmet.dts:70-72`, active low.
const POWER_KEY: Pin = gpio::GPX1.pin(3);
/// Volume up key: `exynos4412-mehmet.dts:80`, active low.
const VOLUME_UP_KEY: Pin = gpio::GPX3.pin(4);
/// Volume down key: `exynos4412-mehmet.dts:87`, active low.
const VOLUME_DOWN_KEY: Pin = gpio::GPX3.pin(3);
/// `nDC_OK`: `exynos4412-mehmet.dts:194`, active low.
const DC_OK: Pin = gpio::GPX2.pin(4);
/// `nUSB_OK`: `exynos4412-mehmet.dts:195`, active low.
const USB_OK: Pin = gpio::GPX2.pin(6);

/// How long the power key must stay down before the keys act.
const HOLD_MS: u64 = 2000;
/// How long all keys must stay up before acting, so that a bounce is not a new press.
const RELEASE_MS: u64 = 100;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// What U-Boot boots after a restart: mach-mehmet.c:160-165.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMode {
    /// The usual `bootcmd`: `MEHMET_BOOT_NORMAL`.
    Normal,
    /// The recovery system: `MEHMET_BOOT_RECOVERY`.
    Recovery,
    /// Stays in U-Boot's fastboot, ready for flashing: `MEHMET_BOOT_FASTBOOT`.
    Fastboot,
}

impl BootMode {
    const fn raw(self) -> u32 {
        match self {
            Self::Normal => 1,
            Self::Recovery => 2,
            Self::Fastboot => 6,
        }
    }
}

fn read(offset: usize) -> u32 {
    // SAFETY: every offset above is a register of the PMU, device memory that stays mapped for
    // the whole program.
    unsafe { ptr::read_volatile((soc::PMU_BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as in `read`. These registers either hold values for U-Boot or are the reset and
    // power controls, whose whole purpose is what the callers ask of them.
    unsafe { ptr::write_volatile((soc::PMU_BASE + offset) as *mut u32, value) }
}

fn wait_polls(polls: u32) {
    for _ in 0..polls {
        read(PS_HOLD_CONTROL);
    }
}

/// Raw `INFORM5`, the boot mode register.
pub fn inform5() -> u32 {
    read(INFORM5)
}

/// Raw `INFORM6`, the charger mode register.
pub fn inform6() -> u32 {
    read(INFORM6)
}

/// Raw `PS_HOLD_CONTROL`.
pub fn ps_hold_control() -> u32 {
    read(PS_HOLD_CONTROL)
}

/// Whether a charger is connected to the DC input and to USB, in that order. Configures both
/// pins as inputs with pull-up, as `exynos4412-mehmet.dts:806-810` does. A USB cable to a PC
/// counts as a charger.
pub fn charger_connected() -> (bool, bool) {
    DC_OK.set_input(Pull::Up);
    USB_OK.set_input(Pull::Up);
    (!DC_OK.read(), !USB_OK.read())
}

fn reset_with(inform5: Option<u32>, inform6: u32) -> ! {
    console::flush();
    if let Some(mode) = inform5 {
        write(INFORM5, mode);
    }
    write(INFORM6, inform6);
    wait_polls(SETTLE_POLLS);
    loop {
        for _ in 0..RESET_ATTEMPTS {
            write(SWRESET, 1);
            wait_polls(SETTLE_POLLS);
        }
    }
}

/// Resets the whole chip. U-Boot then boots `mode`, and skips its charger mode.
///
/// The PMIC keeps its state over this: rails that were on stay on.
pub fn restart(mode: BootMode) -> ! {
    reset_with(Some(mode.raw()), INFORM6_NO_CHARGER_MODE)
}

/// Powers off as the vendor kernel does (mach-mehmet.c:963-975). With a charger connected, that
/// means restarting with `INFORM6` set for charger mode, not cutting the power: on the board,
/// dropping `PS_HOLD` with a charger connected left it running (`power_test`).
///
/// # Panics
///
/// If the board is still on after dropping `PS_HOLD`, with no charger connected.
pub fn power_off() -> ! {
    let (dc, usb) = charger_connected();
    if dc || usb {
        reset_with(None, INFORM6_CHARGER_MODE);
    }
    // Drops the line that keeps the PMIC on.
    console::flush();
    write(PS_HOLD_CONTROL, read(PS_HOLD_CONTROL) & !PS_HOLD_HIGH);
    wait_polls(POWER_FALL_POLLS);
    panic!("still powered after dropping PS_HOLD");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    PowerOff,
    Restart,
    Fastboot,
}

/// Watches the keys:
///
/// | Keys, held 2 s | Action |
/// |---|---|
/// | Power | [`power_off`] |
/// | Power and volume up | [`restart`] normally |
/// | Power and volume down | [`restart`] into fastboot |
///
/// It acts only after every key is up, for two reasons: the power key held down could turn the
/// board straight back on, and U-Boot could read a held volume key as a request of its own.
///
/// Do not use it with a pin of these keys set up as an interrupt: [`PowerKeys::new`] makes them
/// plain inputs.
#[derive(Debug)]
pub struct PowerKeys {
    pressed_since: Option<u64>,
    armed: Option<Action>,
    released_since: Option<u64>,
}

impl PowerKeys {
    /// Configures the key pins as `exynos4412-mehmet.dts:608-627` does.
    pub fn new() -> Self {
        for pin in [POWER_KEY, VOLUME_UP_KEY, VOLUME_DOWN_KEY] {
            pin.set_input(Pull::Up);
        }
        Self { pressed_since: None, armed: None, released_since: None }
    }

    /// Checks the keys, and restarts or powers off once the time comes. Needs the MCT running.
    pub fn service(&mut self, mct: Mct) {
        let now = mct.count();
        let power = !POWER_KEY.read();
        let up = !VOLUME_UP_KEY.read();
        let down = !VOLUME_DOWN_KEY.read();

        if let Some(action) = self.armed {
            if power || up || down {
                self.released_since = None;
            } else {
                let since = *self.released_since.get_or_insert(now);
                if now - since >= RELEASE_MS * COUNTS_PER_MS {
                    match action {
                        Action::PowerOff => power_off(),
                        Action::Restart => restart(BootMode::Normal),
                        Action::Fastboot => restart(BootMode::Fastboot),
                    }
                }
            }
            return;
        }

        if !power {
            self.pressed_since = None;
            return;
        }
        let since = *self.pressed_since.get_or_insert(now);
        if now - since >= HOLD_MS * COUNTS_PER_MS {
            let action = match (up, down) {
                (true, false) => Action::Restart,
                (false, true) => Action::Fastboot,
                _ => Action::PowerOff,
            };
            println!("{:?}: release the keys", action);
            self.armed = Some(action);
        }
    }
}

impl Default for PowerKeys {
    fn default() -> Self {
        Self::new()
    }
}
