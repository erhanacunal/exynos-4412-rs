//! Clock management unit: reads the clock of the peripheral bus and opens clock gates.
//!
//! Nothing here changes a PLL, a mux or a divider: those stay as U-Boot set them.
//!
//! Register offsets and clock tree are verified against Linux 5.15.198
//! `drivers/clk/samsung/clk-exynos4.c`, cited as `clk-exynos4.c:<line>`, relative to
//! [`soc::CMU_BASE`], the `reg` of the node the same driver binds to.
//!
//! # Concurrency
//!
//! [`Cmu::enable_i2c`] is a read-modify-write of a register shared with other peripherals. It
//! masks IRQs on the calling core. Use it from one core at a time.

use core::ptr;

use crate::{cpu, soc};

/// clk-exynos4.c:40.
const SRC_TOP0: usize = 0xC210;
/// clk-exynos4.c:41.
const SRC_TOP1: usize = 0xC214;
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
/// Number of gates from [`GATE_I2C0_BIT`] that this module opens, leaving out `CLK_I2C_HDMI`.
const I2C_GATES: usize = 8;

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
        let mpll = if self.read(SRC_DMC) & SRC_DMC_MPLL != 0 {
            self.fout_mpll_hz()
        } else {
            soc::FIN_PLL_HZ
        };
        let parent = if self.read(SRC_TOP1) & SRC_TOP1_MPLL_USER_T != 0 {
            mpll
        } else {
            soc::FIN_PLL_HZ
        };
        Some(parent / (((self.read(DIV_TOP) >> DIV_TOP_ACLK100_SHIFT) & 0xF) + 1))
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
