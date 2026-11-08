//! The USB 2.0 PHY at [`soc::USB_PHY_BASE`]. PHY0 drives the micro-USB port, for the OTG
//! controller (the device side) or for the EHCI's port 0 (the host side): a mode switch in the
//! system registers picks one. The HSIC PHYs, EHCI ports 1 and 2, are not used on this board.
//!
//! Registers, fields and the order of the steps: Linux 5.15.198
//! `drivers/phy/samsung/phy-exynos4x12-usb2.c`, cited by line, for the `exynos4x12-usb2-phy`
//! variant (`has_mode_switch`). The vendor kernel (`/home/developer/willow`,
//! `arch/arm/mach-exynos/setup-usb-phy.c:284-310`) resets the OTG link along with PHY0, which is
//! done here too: the controller's state from U-Boot's fastboot goes with it.

use core::ptr;

use crate::drivers::cmu::Cmu;
use crate::drivers::timer::Mct;
use crate::{sleep, soc};

// phy-exynos4x12-usb2.c:20-36: power. A bit set turns that part off.
const UPHYPWR: usize = 0x00;
const UPHYPWR_PHY0: u32 = 1 << 0 | 1 << 3 | 1 << 4 | 1 << 5;
const UPHYPWR_PHY1: u32 = 1 << 6 | 1 << 7 | 1 << 8;
// phy-exynos4x12-usb2.c:61-72,85-87: the reference clock.
const UPHYCLK: usize = 0x04;
const UPHYCLK_FSEL_MASK: u32 = 7;
/// 24 MHz: `xusbxti`, `exynos4412-mehmet.dts:43-46`.
const UPHYCLK_FSEL_24MHZ: u32 = 5;
const UPHYCLK_PHY1_COMMON_ON: u32 = 1 << 7;
// phy-exynos4x12-usb2.c:98-110: resets, held while set.
const UPHYRST: usize = 0x08;
const URSTCON_PHY0: u32 = 1 << 0;
const URSTCON_OTG_HLINK: u32 = 1 << 1;
const URSTCON_OTG_PHYLINK: u32 = 1 << 2;
const URSTCON_HOST_PHY: u32 = 1 << 3;
const URSTCON_PHY1: u32 = 1 << 4;
const URSTCON_HOST_LINK_P0: u32 = 1 << 10;
/// phy-exynos4x12-usb2.c:112-113: in the PMU, PHY0's isolation, off while bit 0 is set.
const PMU_USB_ISOL: usize = 0x704;
const PMU_USB_ISOL_OTG: u32 = 1 << 0;
/// phy-exynos4x12-usb2.c:119-122: in the system registers, who PHY0 serves.
const SYSREG_MODE_SWITCH: usize = 0x21C;
const MODE_SWITCH_HOST: u32 = 1;

/// phy-exynos4x12-usb2.c:233-238: a reset is held 10 us, then the PHY needs 80 us.
const RESET_HOLD_US: u64 = 10;
const RESET_SETTLE_US: u64 = 80;

/// Who PHY0 serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The OTG controller, as a USB device: [`otg`](crate::drivers::otg).
    Device,
    /// The EHCI's port 0, as a USB host.
    Host,
}

/// The PHY's registers, for a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    /// `UPHYPWR`: set bits are parts turned off.
    pub power: u32,
    /// `UPHYCLK`.
    pub clock: u32,
    /// `UPHYRST`: set bits are resets held.
    pub reset: u32,
    /// The PMU's `USB_ISOL` register: bit 0 set means PHY0 is not isolated.
    pub isolation: u32,
    /// The mode switch: 0 device, 1 host.
    pub mode: u32,
}

fn pmu(offset: usize) -> *mut u32 {
    (soc::PMU_BASE + offset) as *mut u32
}

fn sysreg(offset: usize) -> *mut u32 {
    (soc::SYSREG_BASE + offset) as *mut u32
}

fn phy(offset: usize) -> *mut u32 {
    (soc::USB_PHY_BASE + offset) as *mut u32
}

fn read(address: *mut u32) -> u32 {
    // SAFETY: callers pass registers of the PMU, the system registers or the PHY block, device
    // memory that stays mapped for the whole program; the PHY block's gate is open by then.
    unsafe { ptr::read_volatile(address) }
}

fn write(address: *mut u32, value: u32) {
    // SAFETY: as in `read`. Each write changes only the USB PHY's own fields.
    unsafe { ptr::write_volatile(address, value) }
}

fn modify(address: *mut u32, clear: u32, set: u32) {
    write(address, (read(address) & !clear) | set);
}

/// The registers as they are now. Opens the PHY block's gate to read them.
pub fn state() -> State {
    Cmu::new().open_usb_device_gate();
    State {
        power: read(phy(UPHYPWR)),
        clock: read(phy(UPHYCLK)),
        reset: read(phy(UPHYRST)),
        isolation: read(pmu(PMU_USB_ISOL)),
        mode: read(sysreg(SYSREG_MODE_SWITCH)),
    }
}

/// Turns PHY0 on for `mode`, from whatever state it is in: the gates, the mode switch, the
/// reference clock, the isolation off, power on, then a reset of the PHY and of the links on
/// it. The OTG controller or the EHCI has to be set up again afterwards. Its supplies,
/// `vdd_uotg_3v3` (LDO12) and `vdd_uotg_1v0` (LDO15), must be on.
pub fn power_on(mode: Mode, mct: Mct) {
    let cmu = Cmu::new();
    cmu.open_usb_device_gate();
    if mode == Mode::Host {
        cmu.open_usb_host_gate();
    }
    // exynos4x12_power_on (phy-exynos4x12-usb2.c:255-283): the switch first.
    modify(sysreg(SYSREG_MODE_SWITCH), MODE_SWITCH_HOST, if mode == Mode::Host { MODE_SWITCH_HOST } else { 0 });
    // exynos4x12_setup_clk (phy-exynos4x12-usb2.c:186-200).
    modify(phy(UPHYCLK), UPHYCLK_FSEL_MASK, UPHYCLK_FSEL_24MHZ | UPHYCLK_PHY1_COMMON_ON);
    // exynos4x12_isol (phy-exynos4x12-usb2.c:155-184).
    modify(pmu(PMU_USB_ISOL), 0, PMU_USB_ISOL_OTG);
    // exynos4x12_phy_pwr (phy-exynos4x12-usb2.c:202-246): PHY0, plus PHY1's part for the host.
    let (power, resets) = match mode {
        Mode::Device => (UPHYPWR_PHY0, URSTCON_PHY0 | URSTCON_OTG_HLINK | URSTCON_OTG_PHYLINK),
        Mode::Host => (
            UPHYPWR_PHY0 | UPHYPWR_PHY1,
            URSTCON_PHY0 | URSTCON_HOST_PHY | URSTCON_PHY1 | URSTCON_HOST_LINK_P0,
        ),
    };
    modify(phy(UPHYPWR), power, 0);
    modify(phy(UPHYRST), 0, resets);
    sleep::for_us(mct, RESET_HOLD_US);
    modify(phy(UPHYRST), resets, 0);
    sleep::for_us(mct, RESET_SETTLE_US);
}

/// Turns PHY0 (and PHY1's part) off and isolates it: the port goes quiet, as when unplugged.
pub fn power_off() {
    Cmu::new().open_usb_device_gate();
    modify(phy(UPHYPWR), 0, UPHYPWR_PHY0 | UPHYPWR_PHY1);
    modify(pmu(PMU_USB_ISOL), PMU_USB_ISOL_OTG, 0);
}
