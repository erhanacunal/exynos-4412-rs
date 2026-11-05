//! The MFC (Multi Format Codec v5.1, the video codec) at [`soc::MFC_BASE`]: its power domain
//! and clocks only.
//!
//! What the codec is told, and in which order, is not here: Samsung describes it nowhere but in
//! GPL drivers, so it lives in the separate GPL crate `exynos_4412_mfc`, which builds on this.
//! This module, like the rest of this crate, takes only register addresses and fields from
//! Linux: `exynos4.dtsi:114-118,438-447` and `clk-exynos4.c`.
//!
//! The vendor kernel runs `sclk_mfc` at 200 MHz on this board (`mach-mehmet.c:4231`,
//! `setup-mfc.c:28`): [`VENDOR_HZ`].

use core::ptr;

use crate::drivers::cmu::Cmu;
use crate::drivers::timer::Mct;
use crate::soc;

/// `sclk_mfc` as the vendor kernel sets it.
pub const VENDOR_HZ: u32 = 200_000_000;

/// The power domain's configuration and status registers, `pd_mfc`: the status reads 7 when on,
/// as for the GPU's (`drivers::mali`).
const DOMAIN_ON: u32 = 7;
const DOMAIN_STATUS: usize = 0x04;
const TIMEOUT_US: u64 = 10_000;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

/// The power domain did not turn on: its last status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerDomainTimeout(pub u32);

/// Raw status of the MFC's power domain.
pub fn power_domain_status() -> u32 {
    // SAFETY: this register of the PMU is device memory that stays mapped for the whole program.
    unsafe { ptr::read_volatile((soc::PD_MFC_BASE + DOMAIN_STATUS) as *const u32) }
}

/// Whether the MFC's registers answer: its power domain on and its clock gate open.
pub fn is_accessible() -> bool {
    power_domain_status() & DOMAIN_ON == DOMAIN_ON && Cmu::new().mfc_clocks().gate_open()
}

/// Turns the power domain on if it is off, opens the clock gate and runs `sclk_mfc` at the
/// highest rate not above `hz` from the MPLL. Returns the rate.
pub fn power_on(hz: u32, mct: Mct) -> Result<u32, PowerDomainTimeout> {
    if power_domain_status() & DOMAIN_ON != DOMAIN_ON {
        // SAFETY: as in `power_domain_status`, the configuration register before it. Nothing
        // uses this domain while it is off.
        unsafe { ptr::write_volatile(soc::PD_MFC_BASE as *mut u32, DOMAIN_ON) };
        let begin = mct.count();
        while power_domain_status() & DOMAIN_ON != DOMAIN_ON {
            if mct.count() - begin > TIMEOUT_US * COUNTS_PER_US {
                return Err(PowerDomainTimeout(power_domain_status()));
            }
        }
    }
    let cmu = Cmu::new();
    cmu.open_mfc_gate();
    Ok(cmu.set_mfc_clock(hz))
}
