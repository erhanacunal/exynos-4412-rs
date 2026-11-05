//! Clock management unit: reads the clock of the peripheral bus and opens clock gates.
//!
//! The only PLL it changes is the EPLL, for audio: see [`Cmu::set_epll`]. Nothing else here
//! runs from the EPLL. The only muxes and dividers it changes are those of the FIMD, of
//! `sclk_mmc3` and of `sclk_g3d`, which nothing else uses: see [`Cmu::set_fimd_clock`],
//! [`Cmu::set_mmc3_clock`] and [`Cmu::set_g3d_clock`]. The rest stay as U-Boot set them.
//!
//! Register offsets and clock tree are verified against Linux 5.15.198
//! `drivers/clk/samsung/clk-exynos4.c`, cited as `clk-exynos4.c:<line>`, relative to
//! [`soc::CMU_BASE`], the `reg` of the node the same driver binds to.
//!
//! # Concurrency
//!
//! [`Cmu::enable_i2c`], [`Cmu::open_fimd_gates`], [`Cmu::set_fimd_clock`],
//! [`Cmu::set_mmc3_clock`], [`Cmu::open_audio_gates`], [`Cmu::open_pdma0_gate`],
//! [`Cmu::open_g3d_gate`] and [`Cmu::set_g3d_clock`] are
//! read-modify-writes of registers shared with other peripherals. They mask IRQs on the
//! calling core. Use them from one core at a time.

use core::ptr;

use crate::drivers::timer::Mct;
use crate::{cpu, soc};

/// clk-exynos4.c:32.
const EPLL_LOCK: usize = 0xC010;
/// clk-exynos4.c:34.
const EPLL_CON0: usize = 0xC110;
/// clk-exynos4.c:35.
const EPLL_CON1: usize = 0xC114;
/// clk-exynos4.c:96 `E4X12_GATE_IP_MAUDIO`.
const GATE_IP_MAUDIO: usize = 0xC93C;
/// clk-exynos4.c:962: `CLK_AUDSS`, the clock of the audio subsystem's registers.
const GATE_MAUDIO_AUDSS: u32 = 1 << 0;
/// clk-exynos4.c:1001: `CLK_I2S0`.
const GATE_MAUDIO_I2S0: u32 = 1 << 3;
/// `drivers/clk/samsung/clk-pll.c:321`.
const PLL36XX_ENABLE: u32 = 1 << 31;
/// `clk-pll.c:320`.
const PLL36XX_LOCKED: u32 = 1 << 29;
/// `clk-pll.c:310`: the lock time, in input cycles, is this times P.
const PLL36XX_LOCK_FACTOR: u32 = 3000;
/// `clk-pll.c:20` `PLL_TIMEOUT_US`.
const PLL_TIMEOUT_US: u64 = 20_000;

/// clk-exynos4.c:40.
const SRC_TOP0: usize = 0xC210;
/// clk-exynos4.c:41.
const SRC_TOP1: usize = 0xC214;
/// clk-exynos4.c:47.
const SRC_LCD0: usize = 0xC234;
/// clk-exynos4.c:58.
const SRC_MASK_LCD0: usize = 0xC334;
/// clk-exynos4.c:71.
const DIV_LCD0: usize = 0xC534;
/// clk-exynos4.c:93.
const GATE_IP_LCD0: usize = 0xC934;
/// clk-exynos4.c:51.
const SRC_FSYS: usize = 0xC240;
/// clk-exynos4.c:62.
const SRC_MASK_FSYS: usize = 0xC340;
/// clk-exynos4.c:77.
const DIV_FSYS2: usize = 0xC548;
/// clk-exynos4.c:78.
const DIV_FSYS3: usize = 0xC54C;
/// clk-exynos4.c:97.
const GATE_IP_FSYS: usize = 0xC940;
/// clk-exynos4.c:65.
const DIV_TOP: usize = 0xC510;
/// clk-exynos4.c:99.
const GATE_IP_PERIL: usize = 0xC950;
/// clk-exynos4.c:45.
const SRC_G3D: usize = 0xC22C;
/// clk-exynos4.c:69.
const DIV_G3D: usize = 0xC52C;
/// clk-exynos4.c:91.
const GATE_IP_G3D: usize = 0xC92C;
/// clk-exynos4.c:553,339: `mout_g3d0` selects `sclk_apll` when set, `mout_mpll_user_t` when
/// clear.
const SRC_G3D_G3D0_APLL: u32 = 1 << 0;
/// clk-exynos4.c:427,286: `mout_g3d` selects `mout_g3d1` when set, `mout_g3d0` when clear.
const SRC_G3D_G3D1: u32 = 1 << 8;
/// clk-exynos4.c:620: the divider of `sclk_g3d`, bits 0 to 3.
const DIV_G3D_MASK: u32 = 0xF;
/// clk-exynos4.c:728: `CLK_G3D`, the GPU's own clock gate.
const GATE_G3D: u32 = 1 << 0;
/// clk-exynos4.c:44.
const SRC_MFC: usize = 0xC228;
/// clk-exynos4.c:68.
const DIV_MFC: usize = 0xC528;
/// clk-exynos4.c:90.
const GATE_IP_MFC: usize = 0xC928;
/// clk-exynos4.c:552,339: `mout_mfc0` selects `mout_apll` when set, `mout_mpll_user_t` when
/// clear.
const SRC_MFC_MFC0_APLL: u32 = 1 << 0;
/// clk-exynos4.c:424,285: `mout_mfc` selects `mout_mfc1` (EPLL or VPLL) when set, `mout_mfc0`
/// when clear.
const SRC_MFC_MFC1: u32 = 1 << 8;
/// clk-exynos4.c:619: the divider of `sclk_mfc`, bits 0 to 3.
const DIV_MFC_MASK: u32 = 0xF;
/// clk-exynos4.c:823: `CLK_MFC`, the MFC's own gate. Bits 1 to 4 are its system MMUs and PPMUs.
const GATE_MFC: u32 = 1 << 0;
/// clk-exynos4.c:104 `E4X12_MPLL_CON0`.
const MPLL_CON0: usize = 0x1_0108;
/// clk-exynos4.c:105.
const SRC_DMC: usize = 0x1_0200;

/// clk-exynos4.c:531: `mout_aclk100` selects `sclk_apll` when set (clk-exynos4.c:352).
const SRC_TOP0_ACLK100_APLL: u32 = 1 << 16;
/// clk-exynos4.c:431: `sclk_epll` selects `fout_epll` when set (clk-exynos4.c:281).
const SRC_TOP0_SCLK_EPLL: u32 = 1 << 4;
/// clk-exynos4.c:523-524: `mout_mpll_user_t` selects `sclk_mpll` when set (clk-exynos4.c:335).
const SRC_TOP1_MPLL_USER_T: u32 = 1 << 12;
/// clk-exynos4.c:540: `sclk_mpll` selects `fout_mpll` when set (clk-exynos4.c:280).
const SRC_DMC_MPLL: u32 = 1 << 12;
/// clk-exynos4.c:630: `aclk100` divides by this field, plus 1.
const DIV_TOP_ACLK100_SHIFT: u32 = 4;
/// clk-exynos4.c:858: `CLK_I2C0` is bit 6, and the next seven follow up to clk-exynos4.c:872.
const GATE_I2C0_BIT: u32 = 6;
/// clk-exynos4.c:555: `mout_fimd0` is this field of `SRC_LCD0`.
const SRC_LCD0_FIMD0_MASK: u32 = 0xF;
/// Index of `mout_mpll_user_t` in `group1_p4x12`, clk-exynos4.c:340-341. It is the parent the
/// vendor kernel gives the FIMD: `mach-mehmet.c:4253`. `mout_mmc3` has the same list of parents.
const SRC_LCD0_FIMD0_MPLL_USER: u32 = 6;
/// clk-exynos4.c:621: `div_fimd0` is this field of `DIV_LCD0`, plus 1.
const DIV_LCD0_FIMD0_MASK: u32 = 0xF;
/// clk-exynos4.c:765: `CLK_SCLK_FIMD0`.
const SRC_MASK_LCD0_FIMD0: u32 = 1 << 0;
/// clk-exynos4.c:830: `CLK_FIMD0`.
const GATE_LCD0_FIMD0: u32 = 1 << 0;
/// clk-exynos4.c:832: `CLK_SMMU_FIMD0`. The System MMU stays off, but it sits on the path of
/// the FIMD's reads. TODO(verify): whether a closed gate here blocks them.
const GATE_LCD0_SMMU_FIMD0: u32 = 1 << 4;
/// clk-exynos4.c:751: `CLK_SDMMC4`, the register clock of the eMMC controller.
const GATE_FSYS_SDMMC4: u32 = 1 << 9;
/// clk-exynos4.c:835: `CLK_PDMA0`.
const GATE_FSYS_PDMA0: u32 = 1 << 0;
/// clk-exynos4.c:845: `CLK_SDMMC3`, the register clock of `sdhci_3`.
const GATE_FSYS_SDMMC3: u32 = 1 << 8;
/// clk-exynos4.c:561: `mout_mmc3` is this field of `SRC_FSYS`.
const SRC_FSYS_MMC3_SHIFT: u32 = 12;
const SRC_FSYS_MMC3_MASK: u32 = 0xF << SRC_FSYS_MMC3_SHIFT;
/// clk-exynos4.c:773: `CLK_SCLK_MMC3`.
const SRC_MASK_FSYS_MMC3: u32 = 1 << 12;
/// clk-exynos4.c:628: `div_mmc3` is this field of `DIV_FSYS2`, plus 1.
const DIV_FSYS2_MMC3_SHIFT: u32 = 16;
const DIV_FSYS2_MMC3_MAX: u32 = 16;
/// clk-exynos4.c:664: `div_mmc_pre3` is this field of `DIV_FSYS2`, plus 1.
const DIV_FSYS2_MMC3_PRE_SHIFT: u32 = 24;
const DIV_FSYS2_MMC3_PRE_MAX: u32 = 256;
/// Number of gates from [`GATE_I2C0_BIT`] that this module opens, leaving out `CLK_I2C_HDMI`.
const I2C_GATES: usize = 8;

/// The settings of a PLL36xx, the type of the EPLL (clk-exynos4.c:1162), as in
/// `clk-pll.c:310-321`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pll36xx {
    /// Feedback divider, 9 bits.
    pub m: u16,
    /// Input divider, 6 bits.
    pub p: u8,
    /// Output divider, a power of two, 3 bits.
    pub s: u8,
    /// Fraction of M, in 1/65536.
    pub k: i16,
}

impl Pll36xx {
    /// 196.608 MHz from 24 MHz, `clk-exynos4.c:1124`: 16 times 12.288 MHz, which is 256 times
    /// 48 kHz. Linux lists it as 196608001 Hz, rounded down; the rate is
    /// 196608001.7 Hz.
    pub const AUDIO_196_608_MHZ: Self = Self { m: 197, p: 3, s: 3, k: -25690 };

    /// The settings in `con0` and `con1`.
    pub const fn from_registers(con0: u32, con1: u32) -> Self {
        Self {
            m: ((con0 >> 16) & 0x1FF) as u16,
            p: ((con0 >> 8) & 0x3F) as u8,
            s: (con0 & 7) as u8,
            k: con1 as u16 as i16,
        }
    }

    /// The output rate from `fin_hz`, rounded down as `clk-pll.c:323-343` does.
    pub const fn hz(&self, fin_hz: u32) -> u32 {
        let multiplier = ((self.m as i64) << 16) + self.k as i64;
        let divider = (self.p as i64) << self.s;
        if multiplier <= 0 || divider == 0 {
            return 0;
        }
        ((fin_hz as i64 * multiplier / divider) >> 16) as u32
    }
}

/// The EPLL as it is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Epll {
    /// `EPLL_CON0`.
    pub con0: u32,
    /// `EPLL_CON1`.
    pub con1: u32,
}

impl Epll {
    /// Whether it runs.
    pub const fn enabled(&self) -> bool {
        self.con0 & PLL36XX_ENABLE != 0
    }

    /// Whether it has locked.
    pub const fn locked(&self) -> bool {
        self.con0 & PLL36XX_LOCKED != 0
    }

    /// Its settings.
    pub const fn settings(&self) -> Pll36xx {
        Pll36xx::from_registers(self.con0, self.con1)
    }

    /// Its output rate, 0 if it does not run.
    pub const fn hz(&self) -> u32 {
        if self.enabled() { self.settings().hz(soc::FIN_PLL_HZ) } else { 0 }
    }
}

/// The EPLL did not lock within [`PLL_TIMEOUT_US`] of being set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpllTimeout(pub Epll);

/// Raw clock registers of the GPU, for reports.
#[derive(Clone, Copy, Debug)]
pub struct G3dClocks {
    /// `SRC_G3D`.
    pub src: u32,
    /// `DIV_G3D`.
    pub div: u32,
    /// `GATE_IP_G3D`.
    pub gate: u32,
}

impl G3dClocks {
    /// Whether the GPU's clock gate is open, without which its registers do not answer.
    pub const fn gate_open(&self) -> bool {
        self.gate & GATE_G3D != 0
    }
}

/// Raw clock registers of the MFC, for reports.
#[derive(Clone, Copy, Debug)]
pub struct MfcClocks {
    /// `SRC_MFC`.
    pub src: u32,
    /// `DIV_MFC`.
    pub div: u32,
    /// `GATE_IP_MFC`.
    pub gate: u32,
}

impl MfcClocks {
    /// Whether the MFC's clock gate is open, without which its registers do not answer.
    pub const fn gate_open(&self) -> bool {
        self.gate & GATE_MFC != 0
    }
}

/// Raw clock registers of the FIMD, for reports.
#[derive(Clone, Copy, Debug)]
pub struct FimdClocks {
    /// `SRC_LCD0`.
    pub src: u32,
    /// `SRC_MASK_LCD0`.
    pub src_mask: u32,
    /// `DIV_LCD0`.
    pub div: u32,
    /// `GATE_IP_LCD0`.
    pub gate: u32,
}

impl FimdClocks {
    /// Whether the register clock of the FIMD runs, without which its registers do not answer.
    pub const fn bus_clock_on(&self) -> bool {
        self.gate & GATE_LCD0_FIMD0 != 0
    }
}

/// Raw clock registers of the eMMC controller, for reports.
#[derive(Clone, Copy, Debug)]
pub struct Mmc4Clocks {
    /// `SRC_FSYS`.
    pub src: u32,
    /// `SRC_MASK_FSYS`.
    pub src_mask: u32,
    /// `DIV_FSYS3`.
    pub div: u32,
    /// `GATE_IP_FSYS`.
    pub gate: u32,
}

/// Handle to the clock management unit. Holds no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cmu;

impl Cmu {
    /// Creates a handle.
    pub const fn new() -> Self {
        Self
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: every offset above is a register of this block, device memory that stays
        // mapped for the whole program.
        unsafe { ptr::read_volatile((soc::CMU_BASE + offset) as *const u32) }
    }

    /// Rate of the MPLL output: `clk-exynos4.c:1160` makes it a `pll_35xx`, whose fields and
    /// formula are in `drivers/clk/samsung/clk-pll.c:210-215,227-232`.
    pub fn fout_mpll_hz(&self) -> u32 {
        let con = self.read(MPLL_CON0);
        let m = u64::from((con >> 16) & 0x3FF);
        let p = u64::from((con >> 8) & 0x3F);
        let s = (con & 7) as u64;
        (u64::from(soc::FIN_PLL_HZ) * m / (p << s).max(1)) as u32
    }

    /// Rate of `aclk100`, the clock of the UARTs, I2C buses and PWM (`clk-exynos4.c:848-870`).
    /// `None` if it comes from the APLL, whose rate this module does not read.
    pub fn aclk100_hz(&self) -> Option<u32> {
        if self.read(SRC_TOP0) & SRC_TOP0_ACLK100_APLL != 0 {
            return None;
        }
        Some(self.mout_mpll_user_t_hz() / (((self.read(DIV_TOP) >> DIV_TOP_ACLK100_SHIFT) & 0xF) + 1))
    }

    fn mout_mpll_user_t_hz(&self) -> u32 {
        let mpll = if self.read(SRC_DMC) & SRC_DMC_MPLL != 0 {
            self.fout_mpll_hz()
        } else {
            soc::FIN_PLL_HZ
        };
        if self.read(SRC_TOP1) & SRC_TOP1_MPLL_USER_T != 0 {
            mpll
        } else {
            soc::FIN_PLL_HZ
        }
    }

    /// The clock registers of the eMMC controller as they are now.
    pub fn mmc4_clocks(&self) -> Mmc4Clocks {
        Mmc4Clocks {
            src: self.read(SRC_FSYS),
            src_mask: self.read(SRC_MASK_FSYS),
            div: self.read(DIV_FSYS3),
            gate: self.read(GATE_IP_FSYS),
        }
    }

    /// Whether the register clock of the eMMC controller runs, without which it does not answer.
    pub fn mmc4_bus_clock_on(&self) -> bool {
        self.read(GATE_IP_FSYS) & GATE_FSYS_SDMMC4 != 0
    }

    /// The clock registers of the FIMD as they are now.
    pub fn fimd_clocks(&self) -> FimdClocks {
        FimdClocks {
            src: self.read(SRC_LCD0),
            src_mask: self.read(SRC_MASK_LCD0),
            div: self.read(DIV_LCD0),
            gate: self.read(GATE_IP_LCD0),
        }
    }

    /// Rate of `sclk_fimd0`, the clock the FIMD divides into the pixel clock. `None` unless it
    /// comes from `mout_mpll_user_t`, the only parent this module reads.
    pub fn sclk_fimd0_hz(&self) -> Option<u32> {
        let clocks = self.fimd_clocks();
        if clocks.src & SRC_LCD0_FIMD0_MASK != SRC_LCD0_FIMD0_MPLL_USER {
            return None;
        }
        Some(self.mout_mpll_user_t_hz() / ((clocks.div & DIV_LCD0_FIMD0_MASK) + 1))
    }

    /// Opens the gate of `pdma0`, the DMA controller with the audio requests.
    pub fn open_pdma0_gate(&self) {
        self.modify(GATE_IP_FSYS, 0, GATE_FSYS_PDMA0);
    }

    /// Opens the gate of the registers of `sdhci_3`.
    pub fn open_sdmmc3_gate(&self) {
        self.modify(GATE_IP_FSYS, 0, GATE_FSYS_SDMMC3);
    }

    /// Runs `sclk_mmc3` from `mout_mpll_user_t`, at the highest rate its two dividers reach that
    /// is not above `hz`, and lets it through. Returns the rate. Stop the card clock first: this
    /// can glitch it.
    ///
    /// # Panics
    ///
    /// If `hz` is too low for the dividers to reach.
    pub fn set_mmc3_clock(&self, hz: u32) -> u32 {
        let input = self.mout_mpll_user_t_hz();
        let wanted = input.div_ceil(hz.max(1));
        let mut best = None;
        for div in 1..=DIV_FSYS2_MMC3_MAX {
            let pre = wanted.div_ceil(div).max(1);
            if pre <= DIV_FSYS2_MMC3_PRE_MAX && best.is_none_or(|(d, p)| div * pre < d * p) {
                best = Some((div, pre));
            }
        }
        let (div, pre) = best.expect("sclk_mmc3 rate below what the dividers reach");
        self.modify(SRC_FSYS, SRC_FSYS_MMC3_MASK, SRC_LCD0_FIMD0_MPLL_USER << SRC_FSYS_MMC3_SHIFT);
        self.modify(
            DIV_FSYS2,
            (0xF << DIV_FSYS2_MMC3_SHIFT) | (0xFF << DIV_FSYS2_MMC3_PRE_SHIFT),
            ((div - 1) << DIV_FSYS2_MMC3_SHIFT) | ((pre - 1) << DIV_FSYS2_MMC3_PRE_SHIFT),
        );
        self.modify(SRC_MASK_FSYS, 0, SRC_MASK_FSYS_MMC3);
        input / (div * pre)
    }

    /// The clock registers of the GPU as they are now.
    pub fn g3d_clocks(&self) -> G3dClocks {
        G3dClocks { src: self.read(SRC_G3D), div: self.read(DIV_G3D), gate: self.read(GATE_IP_G3D) }
    }

    /// Rate of `sclk_g3d`, the GPU's core clock. `None` if it comes from a PLL whose rate this
    /// module does not read: the APLL, the VPLL, or the EPLL, which audio retunes.
    pub fn sclk_g3d_hz(&self) -> Option<u32> {
        let src = self.read(SRC_G3D);
        if src & (SRC_G3D_G3D1 | SRC_G3D_G3D0_APLL) != 0 {
            return None;
        }
        Some(self.mout_mpll_user_t_hz() / ((self.read(DIV_G3D) & DIV_G3D_MASK) + 1))
    }

    /// Opens the GPU's clock gate, without which its registers do not answer.
    pub fn open_g3d_gate(&self) {
        self.modify(GATE_IP_G3D, 0, GATE_G3D);
    }

    /// Runs `sclk_g3d` from `mout_mpll_user_t`, divided down to the highest rate not above `hz`,
    /// and returns the rate. The GPU must be idle.
    ///
    /// # Panics
    ///
    /// If `hz` is below what the divider reaches.
    pub fn set_g3d_clock(&self, hz: u32) -> u32 {
        let input = self.mout_mpll_user_t_hz();
        let div = input.div_ceil(hz.max(1)).max(1);
        assert!(div <= DIV_G3D_MASK + 1, "sclk_g3d rate below what the divider reaches");
        // The divider first: the rate only falls while the mux changes.
        let current = self.read(DIV_G3D) & DIV_G3D_MASK;
        if div - 1 > current {
            self.modify(DIV_G3D, DIV_G3D_MASK, div - 1);
        }
        self.modify(SRC_G3D, SRC_G3D_G3D1 | SRC_G3D_G3D0_APLL, 0);
        self.modify(DIV_G3D, DIV_G3D_MASK, div - 1);
        input / div
    }

    /// The clock registers of the MFC as they are now.
    pub fn mfc_clocks(&self) -> MfcClocks {
        MfcClocks { src: self.read(SRC_MFC), div: self.read(DIV_MFC), gate: self.read(GATE_IP_MFC) }
    }

    /// Rate of `sclk_mfc`, the MFC's core clock. `None` if it comes from a PLL whose rate this
    /// module does not read: the APLL, the EPLL or the VPLL.
    pub fn sclk_mfc_hz(&self) -> Option<u32> {
        let src = self.read(SRC_MFC);
        if src & (SRC_MFC_MFC1 | SRC_MFC_MFC0_APLL) != 0 {
            return None;
        }
        Some(self.mout_mpll_user_t_hz() / ((self.read(DIV_MFC) & DIV_MFC_MASK) + 1))
    }

    /// Opens the MFC's clock gate. Its system MMUs' gates are left as they are.
    pub fn open_mfc_gate(&self) {
        self.modify(GATE_IP_MFC, 0, GATE_MFC);
    }

    /// Runs `sclk_mfc` from `mout_mpll_user_t`, divided down to the highest rate not above `hz`,
    /// and returns the rate. The MFC must be idle.
    ///
    /// # Panics
    ///
    /// If `hz` is below what the divider reaches.
    pub fn set_mfc_clock(&self, hz: u32) -> u32 {
        let input = self.mout_mpll_user_t_hz();
        let div = input.div_ceil(hz.max(1)).max(1);
        assert!(div <= DIV_MFC_MASK + 1, "sclk_mfc rate below what the divider reaches");
        // The divider first, if it grows: the rate only falls while the mux changes.
        if div - 1 > self.read(DIV_MFC) & DIV_MFC_MASK {
            self.modify(DIV_MFC, DIV_MFC_MASK, div - 1);
        }
        self.modify(SRC_MFC, SRC_MFC_MFC1 | SRC_MFC_MFC0_APLL, 0);
        self.modify(DIV_MFC, DIV_MFC_MASK, div - 1);
        input / div
    }

    /// Opens the gates of the FIMD's registers and of its bus path.
    pub fn open_fimd_gates(&self) {
        self.modify(GATE_IP_LCD0, 0, GATE_LCD0_FIMD0 | GATE_LCD0_SMMU_FIMD0);
    }

    /// Runs `sclk_fimd0` from `mout_mpll_user_t`, undivided, and lets it through. Stop the FIMD
    /// first: this can glitch its clock.
    pub fn set_fimd_clock(&self) {
        self.modify(SRC_LCD0, SRC_LCD0_FIMD0_MASK, SRC_LCD0_FIMD0_MPLL_USER);
        self.modify(DIV_LCD0, DIV_LCD0_FIMD0_MASK, 0);
        self.modify(SRC_MASK_LCD0, 0, SRC_MASK_LCD0_FIMD0);
    }

    fn modify(&self, offset: usize, clear: u32, set: u32) {
        let address = (soc::CMU_BASE + offset) as *mut u32;
        let was_masked = cpu::mask_irq();
        // SAFETY: as in `read`. Every caller changes only fields of the block it configures: the
        // FIMD, `sdhci_3`, `pdma0`, the audio subsystem, the GPU or the MFC.
        unsafe {
            ptr::write_volatile(address, (ptr::read_volatile(address) & !clear) | set);
        }
        cpu::restore_irq(was_masked);
    }

    /// The EPLL as it is now.
    pub fn epll(&self) -> Epll {
        Epll { con0: self.read(EPLL_CON0), con1: self.read(EPLL_CON1) }
    }

    /// Runs the EPLL with `settings` and waits until it has locked, as
    /// `clk-pll.c:358-404` does. Does nothing if it runs locked with them already. Returns
    /// its rate. Stop what runs from it first; in this crate only the audio subsystem does.
    pub fn set_epll(&self, settings: Pll36xx, mct: Mct) -> Result<u32, EpllTimeout> {
        let now = self.epll();
        if now.enabled() && now.locked() && now.settings() == settings {
            return Ok(now.hz());
        }
        let field = |value: u32, shift: u32, mask: u32| (value & mask) << shift;
        let con0 = PLL36XX_ENABLE
            | field(settings.m.into(), 16, 0x1FF)
            | field(settings.p.into(), 8, 0x3F)
            | field(settings.s.into(), 0, 7);
        // SAFETY: as in `read`; these three registers are the EPLL's alone.
        unsafe {
            ptr::write_volatile(
                (soc::CMU_BASE + EPLL_LOCK) as *mut u32,
                u32::from(settings.p) * PLL36XX_LOCK_FACTOR,
            );
            // The fraction first, so that the PLL locks once, on the final settings.
            ptr::write_volatile((soc::CMU_BASE + EPLL_CON1) as *mut u32, (now.con1 & !0xFFFF) | u32::from(settings.k as u16));
            ptr::write_volatile((soc::CMU_BASE + EPLL_CON0) as *mut u32, (now.con0 & !0x01FF_3F07) | con0);
        }
        let end = mct.count() + PLL_TIMEOUT_US * u64::from(soc::MCT_CLOCK_HZ / 1_000_000);
        loop {
            let epll = self.epll();
            if epll.locked() {
                return Ok(epll.hz());
            }
            if mct.count() > end {
                return Err(EpllTimeout(epll));
            }
        }
    }

    /// Opens the gates of the audio subsystem's registers and of I2S0's.
    pub fn open_audio_gates(&self) {
        self.modify(GATE_IP_MAUDIO, 0, GATE_MAUDIO_AUDSS | GATE_MAUDIO_I2S0);
    }

    /// Whether `sclk_epll`, the clock of the audio subsystem's registers, comes from the EPLL
    /// rather than the 24 MHz input (clk-exynos4.c:281,431).
    pub fn sclk_epll_is_epll(&self) -> bool {
        self.read(SRC_TOP0) & SRC_TOP0_SCLK_EPLL != 0
    }

    /// `E4X12_GATE_IP_MAUDIO`, for reports.
    pub fn audio_gates(&self) -> u32 {
        self.read(GATE_IP_MAUDIO)
    }

    /// Opens the clock gate of the I2C controller `bus`, below 8.
    pub fn enable_i2c(&self, bus: usize) {
        assert!(bus < I2C_GATES);
        let address = (soc::CMU_BASE + GATE_IP_PERIL) as *mut u32;
        let was_masked = cpu::mask_irq();
        // SAFETY: as in `read`, and setting a gate bit only turns on that block's clock.
        unsafe {
            ptr::write_volatile(address, ptr::read_volatile(address) | (1 << (GATE_I2C0_BIT + bus as u32)));
        }
        cpu::restore_irq(was_masked);
    }
}
