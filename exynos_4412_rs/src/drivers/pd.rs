//! The PMU's power domains: blocks of the chip whose supply can be cut while the rest runs.
//!
//! The domains, their registers and their one parent come from Linux 5.15.198
//! `arch/arm/boot/dts/exynos4.dtsi` and `exynos4412.dtsi`, the power sequence from
//! `drivers/soc/samsung/pm_domains.c` (`exynos_pd_power`): write [`LOCAL_PWR_CFG`] to turn a
//! domain on or 0 to turn it off, then wait until the status register, 4 bytes above, has the
//! same three bits.
//!
//! The board's vendor kernel also opens all clock gates of a domain while it comes on, and reads a
//! register of one of its blocks before it closes them again (`willow`
//! `arch/arm/mach-exynos/dev-pd.c`, `exynos_pd_enable`). Linux 5.15 does not, and neither does
//! this module: the drivers open their gates once the domain is on.
//!
//! # What turning a domain off means
//!
//! Every register of its blocks is lost, and touching one while it is off may stop the bus: the
//! drivers check their domain in `is_accessible` and have to run their `power_on` again
//! afterwards. Linux's runtime PM closes the blocks' clock gates first; the CMU driver has no call
//! for that yet, so the gates stay as the drivers left them.
//!
//! # Users
//!
//! There is no reference count. The application decides what is on, as Linux's genpd would from
//! the devices in use; [`Domain::power_off`] only refuses to cut a parent under its child.

use core::ptr;

use super::timer::Mct;
use crate::soc;

/// `pm_domains.c:76-78`, `exynos4210_cfg`, for `samsung,exynos4210-pd`: what turns a domain on,
/// and what its status reads when it is.
pub const LOCAL_PWR_CFG: u32 = 0x7;
/// `pm_domains.c:52`: the status register.
const STATUS: usize = 0x4;

/// `pm_domains.c:49-60` waits about a millisecond. Kept at the 10 ms the drivers used before.
const TIMEOUT_US: u64 = 10_000;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

/// A power domain of the Exynos 4412.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// `pd_cam`: FIMC0-3, JPEG, CSIS and their System MMUs.
    Cam,
    /// `pd_tv`: the HDMI, the mixer and the video processor. A child of [`Domain::Lcd0`].
    Tv,
    /// `pd_mfc`: the video codec.
    Mfc,
    /// `pd_g3d`: the Mali-400 GPU.
    G3d,
    /// `pd_lcd0`: the FIMD, the image enhancer and the MIPI DSI.
    Lcd0,
    /// `pd_isp`: FIMC-IS, the camera's image signal processor.
    Isp,
    /// `pd_gps`: the GPS baseband, which this board does not use.
    Gps,
    /// `pd_gps_alive`: its always-on part.
    GpsAlive,
}

/// Why a domain did not change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// It did not reach the state in time: its status.
    Timeout(Domain, u32),
    /// Turning it off was refused: this child of it is on.
    ChildOn(Domain),
}

impl Domain {
    /// Every domain, in the order of [`Domain::name`]'s table.
    pub const ALL: [Domain; 8] =
        [Domain::Cam, Domain::Tv, Domain::Mfc, Domain::G3d, Domain::Lcd0, Domain::Isp, Domain::Gps, Domain::GpsAlive];

    /// Address of the configuration register; the status register is 4 bytes above.
    pub const fn base(self) -> usize {
        match self {
            // exynos4.dtsi:143-148
            Domain::Cam => soc::PD_CAM_BASE,
            // exynos4.dtsi:135-141
            Domain::Tv => soc::PD_TV_BASE,
            // exynos4.dtsi:114-119
            Domain::Mfc => soc::PD_MFC_BASE,
            // exynos4.dtsi:121-126
            Domain::G3d => soc::PD_G3D_BASE,
            // exynos4.dtsi:128-133
            Domain::Lcd0 => soc::PD_LCD0_BASE,
            // exynos4412.dtsi:226-231
            Domain::Isp => soc::PD_ISP_BASE,
            // exynos4.dtsi:150-155
            Domain::Gps => soc::PD_GPS_BASE,
            // exynos4.dtsi:157-162
            Domain::GpsAlive => soc::PD_GPS_ALIVE_BASE,
        }
    }

    /// The `label` of its node.
    pub const fn name(self) -> &'static str {
        match self {
            Domain::Cam => "CAM",
            Domain::Tv => "TV",
            Domain::Mfc => "MFC",
            Domain::G3d => "G3D",
            Domain::Lcd0 => "LCD0",
            Domain::Isp => "ISP",
            Domain::Gps => "GPS",
            Domain::GpsAlive => "GPS alive",
        }
    }

    /// The domain this one is powered through: `power-domains` in its node. Only `pd_tv` has one,
    /// `exynos4.dtsi:139`.
    pub const fn parent(self) -> Option<Domain> {
        match self {
            Domain::Tv => Some(Domain::Lcd0),
            _ => None,
        }
    }

    fn children(self) -> impl Iterator<Item = Domain> {
        Self::ALL.into_iter().filter(move |child| child.parent() == Some(self))
    }

    /// The raw configuration register.
    pub fn config(self) -> u32 {
        // SAFETY: a PMU register, device memory that stays mapped for the whole program. Reading it
        // has no side effects.
        unsafe { ptr::read_volatile(self.base() as *const u32) }
    }

    /// The raw status register.
    pub fn status(self) -> u32 {
        // SAFETY: as in `config`, the register above it.
        unsafe { ptr::read_volatile((self.base() + STATUS) as *const u32) }
    }

    /// Whether the domain is fully on.
    pub fn is_on(self) -> bool {
        self.status() & LOCAL_PWR_CFG == LOCAL_PWR_CFG
    }

    /// Whether the domain is fully off.
    pub fn is_off(self) -> bool {
        self.status() & LOCAL_PWR_CFG == 0
    }

    /// Turns the domain on, its parent first, and waits. Does nothing to a domain that is on.
    /// Starts the MCT if needed.
    pub fn power_on(self, mct: Mct) -> Result<(), Error> {
        if let Some(parent) = self.parent() {
            parent.power_on(mct)?;
        }
        if self.is_on() {
            return Ok(());
        }
        self.set(LOCAL_PWR_CFG, mct)
    }

    /// Turns the domain off and waits, once no child of it is on. Does nothing to a domain that
    /// is off. Starts the MCT if needed. See the [module documentation](self) for what is lost.
    pub fn power_off(self, mct: Mct) -> Result<(), Error> {
        if let Some(child) = self.children().find(|child| !child.is_off()) {
            return Err(Error::ChildOn(child));
        }
        if self.is_off() {
            return Ok(());
        }
        self.set(0, mct)
    }

    fn set(self, value: u32, mct: Mct) -> Result<(), Error> {
        mct.start();
        // SAFETY: the configuration register, as in `config`. `power_on` and `power_off` hold the
        // parent's rule; what the blocks inside lose is the caller's choice, as documented.
        unsafe { ptr::write_volatile(self.base() as *mut u32, value) };
        let begin = mct.count();
        while self.status() & LOCAL_PWR_CFG != value {
            if mct.count() - begin > TIMEOUT_US * COUNTS_PER_US {
                return Err(Error::Timeout(self, self.status()));
            }
        }
        Ok(())
    }
}
