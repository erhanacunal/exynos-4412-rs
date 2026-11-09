//! The MFC (Multi Format Codec v5.1, the video codec) at [`soc::MFC_BASE`](crate::soc::MFC_BASE): its power domain
//! and clocks only.
//!
//! What the codec is told, and in which order, is not here: Samsung describes it nowhere but in
//! GPL drivers, so it lives in the separate GPL crate `exynos_4412_mfc`, which builds on this.
//! This module, like the rest of this crate, takes only register addresses and fields from
//! Linux: `exynos4.dtsi:114-118,438-447` and `clk-exynos4.c`.
//!
//! The vendor kernel runs `sclk_mfc` at 200 MHz on this board (`mach-mehmet.c:4231`,
//! `setup-mfc.c:28`): [`VENDOR_HZ`].

use crate::drivers::cmu::Cmu;
use crate::drivers::pd::Domain;
use crate::drivers::timer::Mct;

/// `sclk_mfc` as the vendor kernel sets it.
pub const VENDOR_HZ: u32 = 200_000_000;

/// The power domain did not turn on: its last status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerDomainTimeout(pub u32);

/// Raw status of the MFC's power domain.
pub fn power_domain_status() -> u32 {
    Domain::Mfc.status()
}

/// Whether the MFC's registers answer: its power domain on and its clock gate open.
pub fn is_accessible() -> bool {
    Domain::Mfc.is_on() && Cmu::new().mfc_clocks().gate_open()
}

/// Turns the power domain on if it is off, opens the clock gate and runs `sclk_mfc` at the
/// highest rate not above `hz` from the MPLL. Returns the rate.
pub fn power_on(hz: u32, mct: Mct) -> Result<u32, PowerDomainTimeout> {
    Domain::Mfc.power_on(mct).map_err(|_| PowerDomainTimeout(power_domain_status()))?;
    let cmu = Cmu::new();
    cmu.open_mfc_gate();
    Ok(cmu.set_mfc_clock(hz))
}
