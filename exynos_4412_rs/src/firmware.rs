//! Calls into the board's secure firmware.
//!
//! The CPU runs Non-secure (measured by `mmu_test`), and some registers can only be
//! written through these calls. The firmware is signed and its source is unavailable;
//! the commands and their order come from the kernels that run on it:
//!
//! - Linux 5.15.198 `arch/arm/mach-exynos/smc.h:22-26` and `firmware.c:151-186`
//! - the board's vendor kernel `/home/developer/willow`,
//!   `arch/arm/mach-exynos/cpu-exynos4.c:382-387`

use core::arch::asm;

/// `smc.h:15` `SMC_CMD_CPU1BOOT`: (core number, 0, 0) on the Exynos 4412: `firmware.c:68-70`,
/// vendor kernel `arch/arm/mach-exynos/platsmp.c:216-217`.
pub const SMC_CMD_CPU_BOOT: u32 = -4i32 as u32;
/// `smc.h:22` `SMC_CMD_L2X0CTRL`: (control register value, 0, 0).
pub const SMC_CMD_L2X0_CTRL: u32 = -21i32 as u32;
/// `smc.h:23` `SMC_CMD_L2X0SETUP1`: (tag latency, data latency, prefetch control).
pub const SMC_CMD_L2X0_SETUP1: u32 = -22i32 as u32;
/// `smc.h:24` `SMC_CMD_L2X0SETUP2`: (power control, auxiliary control, 0).
pub const SMC_CMD_L2X0_SETUP2: u32 = -23i32 as u32;
/// `smc.h:25` `SMC_CMD_L2X0INVALL`: (0, 0, 0). `firmware.c:159` requires it before enabling.
pub const SMC_CMD_L2X0_INVALL: u32 = -24i32 as u32;

/// Issues a secure monitor call.
///
/// # Safety
///
/// The command must be one the firmware supports, with valid arguments, and the
/// system must be in the state that command requires. Only commands listed in
/// this module are known to exist.
pub unsafe fn smc(command: u32, arg1: u32, arg2: u32, arg3: u32) {
    // SAFETY: the caller vouches for the command. The firmware may use r4-r11 and r12
    // (`exynos-smc.S` saves r4-r11 around the call), hence the explicit save: r7 and r11
    // cannot be asm operands here. Not `nomem`: the firmware changes system state.
    unsafe {
        asm!(
            ".arch_extension sec",
            "push {{r4-r11}}",
            "dsb",
            "smc #0",
            "pop {{r4-r11}}",
            inout("r0") command => _,
            inout("r1") arg1 => _,
            inout("r2") arg2 => _,
            inout("r3") arg3 => _,
            out("r12") _,
            out("lr") _,
        );
    }
}
