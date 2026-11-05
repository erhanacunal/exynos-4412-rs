//! Exynos 4412 physical base addresses, kept in one place.
//!
//! All paths are relative to the Linux 5.15.198 tree, `arch/arm/boot/dts/`.

use crate::drivers::uart::UartId;

/// UART0 registers. Verified: `exynos4.dtsi`, node `serial_0: serial@13800000`.
pub const UART0_BASE: usize = 0x1380_0000;
/// UART1 registers. Verified: `exynos4.dtsi`, node `serial_1: serial@13810000`.
pub const UART1_BASE: usize = 0x1381_0000;
/// UART2 registers, the board's console. Verified: `exynos4.dtsi`, node `serial_2: serial@13820000`.
pub const UART2_BASE: usize = 0x1382_0000;
/// UART3 registers. Verified: `exynos4.dtsi`, node `serial_3: serial@13830000`.
pub const UART3_BASE: usize = 0x1383_0000;
/// UART4 registers.
///
/// Linux 5.15 has no node for it. Verified in the vendor kernel `/home/developer/willow`
/// (3.0.101): `arch/arm/mach-exynos/include/mach/map-exynos4.h:175,310,315` and
/// `map.h:22` give `0x13800000 + 4 * 0x10000`; `arch/arm/plat-s5p/dev-uart.c:125` uses it.
pub const UART4_BASE: usize = 0x1384_0000;

/// GIC distributor. Verified: `exynos4.dtsi`, node `gic`, first `reg` entry.
pub const GIC_DIST_BASE: usize = 0x1049_0000;
/// GIC CPU interface. Verified: `exynos4.dtsi`, node `gic`, second `reg` entry.
pub const GIC_CPU_BASE: usize = 0x1048_0000;
/// Distance between the per-CPU windows of both GIC blocks: `exynos4412.dtsi:724`,
/// `cpu-offset`, applied to the two of them by `drivers/irqchip/irq-gic.c:1176-1180`.
pub const GIC_CPU_STRIDE: usize = 0x4000;
/// Snoop control unit. Verified: `exynos4.dtsi:98`, node `scu: snoop-control-unit@10500000`.
pub const SCU_BASE: usize = 0x1050_0000;
/// Power management unit. Verified: `exynos4.dtsi:183`, node
/// `pmu_system_controller: system-controller@10020000`.
pub const PMU_BASE: usize = 0x1002_0000;
/// Non-secure page of SYSRAM, shared with the secure firmware. Verified:
/// `exynos4412.dtsi:213,220`, `ranges = <0 0x02020000 0x40000>` plus `smp-sram@2f000`, and
/// `exynos4412-mehmet.dts:35`, node `firmware@204f000`, gives the same address.
pub const SYSRAM_NS_BASE: usize = 0x0202_0000 + 0x2_F000;
const _: () = assert!(SYSRAM_NS_BASE == 0x0204_F000);
/// Multi-core timer. Verified: `exynos4412.dtsi`, node `timer@10050000`.
pub const MCT_BASE: usize = 0x1005_0000;
/// GIC interrupt ID of the MCT global comparator 0.
///
/// Verified: `exynos4412.dtsi`, node `timer@10050000`, first entry of `interrupts-extended`
/// is `GIC_SPI 57`, and SPI numbers start at ID 32 (`drivers/irqchip/irq-gic.c`,
/// `gic_irq_domain_translate`).
pub const MCT_G0_IRQ: u16 = 32 + 57;
/// GIC interrupt ID of the MCT local timers, one PPI banked per core.
///
/// Verified: `exynos4412.dtsi:274`, last entry of `interrupts-extended` is `GIC_PPI 12`;
/// `exynos_mct.c:638` selects `MCT_INT_PPI` for this SoC, and `exynos_mct.c:554` requests
/// it as one per-CPU interrupt. PPI numbers start at ID 16
/// (`drivers/irqchip/irq-gic.c`, `gic_irq_domain_translate`).
pub const MCT_LOCAL_IRQ: u16 = 16 + 12;
/// Rate of the MCT global counter.
///
/// `exynos_mct.c:515-518` runs it at `fin_pll`, whose parent is `xusbxti` or `xxti`
/// (`clk-exynos4.c:1044`). `exynos4412-mehmet.dts` gives them 24 MHz and 0 Hz, so 24 MHz.
/// Measured on the board against the 115200 baud UART by `l2_test`: 23 998 400 Hz.
pub const MCT_CLOCK_HZ: u32 = 24_000_000;
/// Chip ID block, whose first word holds the product ID and revision. Verified:
/// `exynos4.dtsi`, node `chipid@10000000`.
pub const CHIPID_BASE: usize = 0x1000_0000;
/// PL310 L2 cache controller. Verified: `exynos4412.dtsi`, node `l2c: cache-controller@10502000`.
pub const L2C_BASE: usize = 0x1050_2000;
/// First byte of DRAM. Verified: `exynos4412-mehmet.dts`, node `memory@40000000`, `reg`.
pub const DRAM_BASE: usize = 0x4000_0000;
/// DRAM usable by this system. Verified: `exynos4412-mehmet.dts`, node `memory@40000000`, `reg`.
pub const DRAM_SIZE: usize = 0x7EA0_0000;
/// Clock management unit. Verified: `exynos4412.dtsi`, node `clock: clock-controller@10030000`.
pub const CMU_BASE: usize = 0x1003_0000;

/// GPIO bank controller 0. Verified: `exynos4412.dtsi`, node `pinctrl_0: pinctrl@11400000`.
pub const GPIO0_BASE: usize = 0x1140_0000;
/// GPIO bank controller 1. Verified: `exynos4412.dtsi`, node `pinctrl_1: pinctrl@11000000`.
pub const GPIO1_BASE: usize = 0x1100_0000;
/// GPIO bank controller 2. Verified: `exynos4412.dtsi`, node `pinctrl_2: pinctrl@3860000`.
pub const GPIO2_BASE: usize = 0x0386_0000;
/// GPIO bank controller 3. Verified: `exynos4412.dtsi`, node `pinctrl_3: pinctrl@106e0000`.
pub const GPIO3_BASE: usize = 0x106E_0000;

/// GIC interrupt ID of the wake-up interrupt of pin 0 of GPX0, followed by the other seven.
/// Verified: `exynos4412-pinctrl.dtsi:571-578`, node `gpx0`, `GIC_SPI 16`..`GIC_SPI 23`.
pub const EINT_GPX0_IRQ: u16 = 32 + 16;
/// GIC interrupt ID of the wake-up interrupt of pin 0 of GPX1, followed by the other seven.
/// Verified: `exynos4412-pinctrl.dtsi:588-595`, node `gpx1`, `GIC_SPI 24`..`GIC_SPI 31`.
pub const EINT_GPX1_IRQ: u16 = 32 + 24;
/// GIC interrupt ID shared by the wake-up interrupts of GPX2 and GPX3. Verified:
/// `exynos4412.dtsi:188-191`, node `wakup_eint`, `GIC_SPI 32`; the nodes `gpx2` and `gpx3`
/// of `exynos4412-pinctrl.dtsi:599-613` have no interrupts of their own.
pub const EINT_GPX23_IRQ: u16 = 32 + 32;

/// I2C controllers 0 to 7. Verified: `exynos4.dtsi:493-584`, nodes `i2c_0`..`i2c_7`, `reg`.
pub const I2C_BASES: [usize; 8] = [
    0x1386_0000,
    0x1387_0000,
    0x1388_0000,
    0x1389_0000,
    0x138A_0000,
    0x138B_0000,
    0x138C_0000,
    0x138D_0000,
];
/// Input of the PLLs. `clk-exynos4.c:1044` takes `fin_pll` from `xusbxti` or `xxti`, and
/// `exynos4412-mehmet.dts:48,53` gives them 0 Hz and 24 MHz.
pub const FIN_PLL_HZ: u32 = 24_000_000;

/// FIMD display controller. Verified: `exynos4.dtsi:705`, node `fimd: fimd@11c00000`, `reg`.
pub const FIMD_BASE: usize = 0x11C0_0000;
/// System registers, among them the LCD block configuration. Verified: `exynos4.dtsi:178`,
/// node `sys_reg: syscon@10010000`.
pub const SYSREG_BASE: usize = 0x1001_0000;
/// Power domain of the FIMD. Verified: `exynos4.dtsi:128-130`, node
/// `pd_lcd0: power-domain@10023c80`, `reg`.
pub const PD_LCD0_BASE: usize = 0x1002_3C80;

/// The Mali-400 MP4 GPU: its GP, L2 cache, PMU, MMUs and four PPs. Verified: `exynos4.dtsi:398-400`,
/// node `gpu: gpu@13000000`, `reg`, 64 KiB.
pub const G3D_BASE: usize = 0x1300_0000;
/// Power domain of the GPU. Verified: `exynos4412.dtsi:121-123`, node
/// `pd_g3d: power-domain@10023c60`, `reg`.
pub const PD_G3D_BASE: usize = 0x1002_3C60;

/// The MFC, the video codec (v5.1). Verified: `exynos4.dtsi:438-447`, node `mfc: codec@13400000`,
/// `reg`, 64 KiB.
pub const MFC_BASE: usize = 0x1340_0000;
/// Its interrupt: `exynos4.dtsi:441`, `GIC_SPI 94`.
pub const MFC_IRQ: u16 = 32 + 94;
/// Power domain of the MFC. Verified: `exynos4.dtsi:114-118`, node `pd_mfc: power-domain@10023c40`,
/// `reg`.
pub const PD_MFC_BASE: usize = 0x1002_3C40;

/// `exynos4412.dtsi:298-305`, `g2d: g2d@10800000`: the 2D graphics accelerator (FIMG2D 4.1).
pub const G2D_BASE: usize = 0x1080_0000;

/// `exynos4.dtsi:658-670`, `pwm: pwm@139d0000`: the PWM timers.
pub const PWM_BASE: usize = 0x139D_0000;

/// `exynos4.dtsi:214-264`: the four FIMCs (camera interface, scaler and colour converter).
pub const FIMC_BASE: [usize; 4] = [0x1180_0000, 0x1181_0000, 0x1182_0000, 0x1183_0000];
/// Their interrupts, `GIC_SPI 84` to `87` (`exynos4.dtsi:217`, and so on).
pub const FIMC_IRQ: [u16; 4] = [32 + 84, 32 + 85, 32 + 86, 32 + 87];
/// `exynos4.dtsi:143-147`, `pd_cam`: the FIMCs' power domain.
pub const PD_CAM_BASE: usize = 0x1002_3C00;

/// eMMC controller, `mshc_0`. Verified: `exynos4412.dtsi:307-309`, node `mshc_0: mmc@12550000`,
/// and `exynos4412-mehmet.dts:1399-1414` has the eMMC on it.
pub const MSHC_BASE: usize = 0x1255_0000;

/// SD host controller of the WiFi chip, `sdhci_3`. Verified: `exynos4.dtsi:346-348`, node
/// `sdhci_3: sdhci@12540000`, and `exynos4412-mehmet.dts:1509-1541` has the chip on it.
pub const SDHCI3_BASE: usize = 0x1254_0000;

/// Clock controller of the audio subsystem. Verified: `exynos4.dtsi:66-68`, node
/// `clock_audss: clock-controller@3810000`, `reg`.
pub const AUDSS_BASE: usize = 0x0381_0000;
/// I2S controller 0, in the audio subsystem. Verified: `exynos4.dtsi:77-79`, node
/// `i2s0: i2s@3830000`, and `exynos4412-mehmet.dts` has the codec on it.
pub const I2S0_BASE: usize = 0x0383_0000;

/// Number of cores. Verified: `exynos4412.dtsi`, nodes `cpu@a00`..`cpu@a03`. `link.x`
/// reserves this many stack blocks.
pub const CPU_COUNT: usize = 4;

/// UART used by [`console`](crate::console) unless the application selects another.
///
/// Verified: `exynos4412-mehmet.dts`, `chosen` → `stdout-path = "serial2:115200n8"`;
/// `exynos4.dtsi`, `aliases` → `serial2 = &serial_2`.
pub const DEFAULT_CONSOLE_UART: UartId = UartId::Uart2;
