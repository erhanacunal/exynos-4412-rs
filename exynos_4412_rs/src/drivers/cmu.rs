//! Clock management unit: reads the clock of the peripheral bus and opens clock gates.
//!
//! Nothing here changes a PLL. The only muxes and dividers it changes are those of the FIMD and
//! of `sclk_mmc3`, which nothing else uses: see [`Cmu::set_fimd_clock`] and
//! [`Cmu::set_mmc3_clock`]. The rest stay as U-Boot set them.
//!
//! Register offsets and clock tree are verified against Linux 5.15.198
//! `drivers/clk/samsung/clk-exynos4.c`, cited as `clk-exynos4.c:<line>`, relative to
//! [`soc::CMU_BASE`], the `reg` of the node the same driver binds to.
//!
//! # Concurrency
//!
//! [`Cmu::enable_i2c`], [`Cmu::open_fimd_gates`], [`Cmu::set_fimd_clock`] and
//! [`Cmu::set_mmc3_clock`] are read-modify-writes of registers shared with other peripherals. They mask IRQs on the
//! calling core. Use them from one core at a time.

use core::ptr;

use crate::{cpu, soc};

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
/// clk-exynos4.c:104 `E4X12_MPLL_CON0`.
const MPLL_CON0: usize = 0x1_0108;
/// clk-exynos4.c:105.
const SRC_DMC: usize = 0x1_0200;

/// clk-exynos4.c:531: `mout_aclk100` selects `sclk_apll` when set (clk-exynos4.c:352).
const SRC_TOP0_ACLK100_APLL: u32 = 1 << 16;
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
        // FIMD or `sdhci_3`.
        unsafe {
            ptr::write_volatile(address, (ptr::read_volatile(address) & !clear) | set);
        }
        cpu::restore_irq(was_masked);
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
