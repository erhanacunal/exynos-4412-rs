//! The JPEG codec (JPEG v2.x, Linux's `exynos4212-jpeg`) at [`soc::JPEG_BASE`](crate::soc::JPEG_BASE):
//! its power domain and clock gate only.
//!
//! How the codec is programmed is not here: Samsung describes it nowhere but in GPL drivers, so
//! it lives in the separate GPL crate `exynos_4412_jpeg`, which builds on this, as the MFC's does
//! ([`mfc`](super::mfc)). This module takes only addresses and clock fields from Linux:
//! `exynos4.dtsi:727-735` and `clk-exynos4.c:631,718,815`.
//!
//! The codec has no clock of its own: it runs on `aclk160` ([`Cmu::aclk160_hz`]), 160 MHz as
//! U-Boot leaves it.

use crate::drivers::cmu::Cmu;
use crate::drivers::pd::Domain;
use crate::drivers::timer::Mct;

/// The camera power domain did not turn on: its last status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerDomainTimeout(pub u32);

/// Raw status of the codec's power domain, the camera block's.
pub fn power_domain_status() -> u32 {
    Domain::Cam.status()
}

/// Whether the codec's registers answer: its power domain on and its clock gate open.
pub fn is_accessible() -> bool {
    Domain::Cam.is_on() && Cmu::new().jpeg_gate_open()
}

/// Turns the camera power domain on if it is off and opens the codec's gates. Returns the rate
/// of `aclk160`, `None` if this crate cannot tell it.
pub fn power_on(mct: Mct) -> Result<Option<u32>, PowerDomainTimeout> {
    Domain::Cam.power_on(mct).map_err(|_| PowerDomainTimeout(power_domain_status()))?;
    let cmu = Cmu::new();
    cmu.open_jpeg_gates();
    Ok(cmu.aclk160_hz())
}
