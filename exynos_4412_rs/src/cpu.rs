//! Access to the CPU state: status register and CP15 identification and fault registers.

use core::arch::asm;

/// Processor mode, decoded from `CPSR.M`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// User.
    User,
    /// FIQ.
    Fiq,
    /// IRQ.
    Irq,
    /// Supervisor.
    Supervisor,
    /// Monitor.
    Monitor,
    /// Abort.
    Abort,
    /// Undefined.
    Undefined,
    /// System.
    System,
    /// A reserved `CPSR.M` encoding, carrying the raw value.
    Unknown(u8),
}

impl Mode {
    /// Decodes the mode field of a CPSR or SPSR value.
    pub const fn from_psr(psr: u32) -> Self {
        match (psr & 0x1F) as u8 {
            0x10 => Self::User,
            0x11 => Self::Fiq,
            0x12 => Self::Irq,
            0x13 => Self::Supervisor,
            0x16 => Self::Monitor,
            0x17 => Self::Abort,
            0x1B => Self::Undefined,
            0x1F => Self::System,
            other => Self::Unknown(other),
        }
    }
}

macro_rules! cp15_read {
    ($(#[$doc:meta])* $name:ident, $crn:literal, $op1:literal, $crm:literal, $op2:literal) => {
        $(#[$doc])*
        #[inline]
        pub fn $name() -> u32 {
            let value: u32;
            // SAFETY: reading this CP15 register at PL1, where all code runs, has no side effects.
            unsafe {
                asm!(
                    concat!("mrc p15, ", $op1, ", {}, ", $crn, ", ", $crm, ", ", $op2),
                    out(reg) value,
                    options(nomem, nostack, preserves_flags),
                );
            }
            value
        }
    };
}

cp15_read!(
    /// Multiprocessor affinity register. On the Cortex-A9, bits 1:0 are the CPU ID.
    read_mpidr, "c0", "0", "c0", "5"
);
cp15_read!(
    /// Data fault status register.
    read_dfsr, "c5", "0", "c0", "0"
);
cp15_read!(
    /// Instruction fault status register.
    read_ifsr, "c5", "0", "c0", "1"
);
cp15_read!(
    /// Data fault address register.
    read_dfar, "c6", "0", "c0", "0"
);
cp15_read!(
    /// Instruction fault address register.
    read_ifar, "c6", "0", "c0", "2"
);

cp15_read!(
    /// System control register.
    read_sctlr, "c1", "0", "c0", "0"
);

cp15_read!(
    /// Main ID register: the core's part number and revision.
    read_midr, "c0", "0", "c0", "0"
);

cp15_read!(
    /// Auxiliary control register. Bit 6 is `SMP`, bit 0 is `FW`.
    read_actlr, "c1", "0", "c0", "1"
);

/// This core's number, 0 to 3: `MPIDR` affinity level 0, as in
/// `arch/arm/mach-exynos/platsmp.c:321` and `exynos4412.dtsi` nodes `cpu@a00`..`cpu@a03`.
#[inline]
pub fn id() -> usize {
    (read_mpidr() & 0xFF) as usize
}

/// Masks IRQ on this core and returns whether it was already masked, for
/// [`restore_irq`].
#[inline]
pub fn mask_irq() -> bool {
    let was_masked = read_cpsr() & CPSR_I != 0;
    disable_irq();
    was_masked
}

/// Undoes [`mask_irq`].
#[inline]
pub fn restore_irq(was_masked: bool) {
    if !was_masked {
        enable_irq();
    }
}

/// `CPSR.I`: `arch/arm/include/uapi/asm/ptrace.h`, `PSR_I_BIT`.
const CPSR_I: u32 = 1 << 7;

/// Reads the current program status register.
#[inline]
pub fn read_cpsr() -> u32 {
    let value: u32;
    // SAFETY: `mrs` from CPSR has no side effects.
    unsafe {
        asm!("mrs {}, cpsr", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Unmasks IRQ on this core. The handler for it must be ready.
#[inline]
pub fn enable_irq() {
    // SAFETY: changes only CPSR.I. Not `nomem`: interrupt handlers may touch memory,
    // so the compiler must not move memory accesses across this point.
    unsafe {
        asm!("cpsie i", options(nostack, preserves_flags));
    }
}

/// Masks IRQ on this core.
#[inline]
pub fn disable_irq() {
    // SAFETY: changes only CPSR.I. Not `nomem`, as in `enable_irq`.
    unsafe {
        asm!("cpsid i", options(nostack, preserves_flags));
    }
}

/// Masks IRQ and FIQ, then waits for interrupts forever.
pub fn halt() -> ! {
    // SAFETY: masking interrupts touches only CPSR.I/F; nothing here relies on them.
    unsafe {
        asm!("cpsid if", options(nomem, nostack, preserves_flags));
    }
    loop {
        // SAFETY: `wfi` only suspends the core until the next wake-up event.
        unsafe {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Starts this core's cycle counter, `PMCCNTR`, from 0, counting every cycle of the core clock:
/// `PMCR.E` and `PMCR.C`, `PMCR.D` clear, then `PMCNTENSET.C` (ARMv7-A ARM, B4.1.117-118).
/// Each core has its own.
pub fn start_cycle_counter() {
    // SAFETY: writes to the PMU's control registers at PL1 only start and reset its counters.
    unsafe {
        asm!(
            "mcr p15, 0, {pmcr}, c9, c12, 0",
            "mcr p15, 0, {enable}, c9, c12, 1",
            "isb",
            pmcr = in(reg) 0b101u32,
            enable = in(reg) 1u32 << 31,
            options(nomem, nostack, preserves_flags),
        );
    }
}

cp15_read!(
    /// This core's cycle counter, `PMCCNTR`: see [`start_cycle_counter`].
    read_cycle_counter, "c9", "0", "c13", "0"
);
