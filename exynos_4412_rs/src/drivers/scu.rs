//! Cortex-A9 snoop control unit, which keeps the L1 data caches coherent.
//!
//! Register offsets and bits are verified against Linux 5.15.198
//! `arch/arm/kernel/smp_scu.c`, cited as `smp_scu.c:<line>`.

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite};
use tock_registers::register_structs;

use crate::soc;

register_structs! {
    Registers {
        /// smp_scu.c:16 `SCU_CTRL`.
        (0x00 => ctrl: ReadWrite<u32>),
        /// smp_scu.c:19 `SCU_CONFIG`.
        (0x04 => config: ReadOnly<u32>),
        (0x08 => _reserved),
        /// smp_scu.c:45, the diagnostic control register, which Linux reaches by its offset.
        (0x30 => diagnostic: ReadWrite<u32>),
        (0x34 => @END),
    }
}

/// smp_scu.c:17 `SCU_ENABLE`.
const CTRL_ENABLE: u32 = 1;
/// smp_scu.c:47: the workaround for Cortex-A9 erratum 764369, a cache maintenance operation by
/// address on Inner Shareable memory that may not reach its point of coherency.
const DIAGNOSTIC_764369: u32 = 1;

/// Handle to the SCU. Holds no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scu;

impl Scu {
    /// Creates a handle.
    pub const fn new() -> Self {
        Self
    }

    fn regs(&self) -> &Registers {
        // SAFETY: SCU_BASE is this MMIO block, device memory that stays mapped for the
        // whole program, and the struct has interior mutable cells only.
        unsafe { &*(soc::SCU_BASE as *const Registers) }
    }

    /// The raw control register.
    pub fn ctrl(&self) -> u32 {
        self.regs().ctrl.get()
    }

    /// The raw configuration register.
    pub fn config(&self) -> u32 {
        self.regs().config.get()
    }

    /// Whether the SCU is on.
    pub fn is_enabled(&self) -> bool {
        self.ctrl() & CTRL_ENABLE != 0
    }

    /// Number of cores attached to the SCU: smp_scu.c `scu_get_core_count`.
    pub fn core_count(&self) -> usize {
        (self.config() & 3) as usize + 1
    }

    /// The raw diagnostic control register.
    pub fn diagnostic(&self) -> u32 {
        self.regs().diagnostic.get()
    }

    /// Whether the workaround for erratum 764369 is on.
    pub fn erratum_764369_workaround(&self) -> bool {
        self.diagnostic() & DIAGNOSTIC_764369 != 0
    }

    fn set_erratum_764369_workaround(&self, on: bool) {
        let value = self.diagnostic() & !DIAGNOSTIC_764369;
        self.regs().diagnostic.set(if on { value | DIAGNOSTIC_764369 } else { value });
    }

    /// Tries to turn on the workaround for erratum 764369, as Linux does whether the SCU is on
    /// or not (smp_scu.c:42-49). On this board the register stays 0: running Non-secure, the
    /// write is ignored, as it is for the vendor kernel, which writes it the same way. Then turns the SCU on, unless it is on already. Call it before any
    /// core enables its data cache, or that core's lines are invisible to the SCU.
    pub(crate) fn enable(&self) {
        self.set_erratum_764369_workaround(true);
        if !self.is_enabled() {
            self.regs().ctrl.set(self.ctrl() | CTRL_ENABLE);
        }
    }
}
