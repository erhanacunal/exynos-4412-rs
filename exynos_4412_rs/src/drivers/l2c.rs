//! PL310 L2 cache controller.
//!
//! Register offsets are verified against Linux 5.15.198
//! `arch/arm/include/asm/hardware/cache-l2x0.h`, cited as `cache-l2x0.h:<line>`.
//!
//! The configuration registers are only read here. A Non-secure caller has to change
//! them through the secure firmware: see [`L2c::enable`].

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::register_structs;
use tock_registers::registers::{ReadOnly, ReadWrite};

use crate::firmware::{self, SMC_CMD_L2X0_CTRL, SMC_CMD_L2X0_INVALL, SMC_CMD_L2X0_SETUP1, SMC_CMD_L2X0_SETUP2};
use crate::soc;

register_structs! {
    Registers {
        /// cache-l2x0.h:13 `L2X0_CACHE_ID`.
        (0x000 => cache_id: ReadOnly<u32>),
        (0x004 => _reserved0),
        /// cache-l2x0.h:15 `L2X0_CTRL`.
        (0x100 => ctrl: ReadOnly<u32>),
        /// cache-l2x0.h:16 `L2X0_AUX_CTRL`.
        (0x104 => aux_ctrl: ReadOnly<u32>),
        /// cache-l2x0.h:17 `L310_TAG_LATENCY_CTRL`.
        (0x108 => tag_latency: ReadOnly<u32>),
        /// cache-l2x0.h:18 `L310_DATA_LATENCY_CTRL`.
        (0x10C => data_latency: ReadOnly<u32>),
        (0x110 => _reserved1),
        /// cache-l2x0.h:28 `L2X0_CACHE_SYNC`.
        (0x730 => cache_sync: ReadWrite<u32>),
        (0x734 => _reserved2),
        /// cache-l2x0.h:31 `L2X0_INV_WAY`.
        (0x77C => inv_way: ReadWrite<u32>),
        (0x780 => _reserved3),
        /// cache-l2x0.h:32 `L2X0_CLEAN_LINE_PA`.
        (0x7B0 => clean_line_pa: ReadWrite<u32>),
        (0x7B4 => _reserved5),
        /// cache-l2x0.h:51 `L310_PREFETCH_CTRL`.
        (0xF60 => prefetch_ctrl: ReadOnly<u32>),
        (0xF64 => _reserved4),
        /// cache-l2x0.h:52 `L310_POWER_CTRL`.
        (0xF80 => power_ctrl: ReadOnly<u32>),
        (0xF84 => @END),
    }
}

/// cache-l2x0.h:144 `L2X0_CTRL_EN`.
const CTRL_EN: u32 = 1;
/// cache-l2x0.h:120 `L310_AUX_CTRL_ASSOCIATIVITY_16`.
const AUX_ASSOCIATIVITY_16: u32 = 1 << 16;

// Configuration for `enable`. Each value below matches both the Linux 5.15 device tree and
// the board's vendor kernel (`willow`, `arch/arm/mach-exynos/cpu-exynos4.c:373-378`).

/// `exynos4412.dtsi` `arm,tag-latency = <2 2 1>`, encoded by cache-l2x0.h:130-132.
const TAG_LATENCY: u32 = 0x110;
/// `exynos4412.dtsi` `arm,data-latency = <3 2 1>`, encoded by cache-l2x0.h:130-132.
const DATA_LATENCY: u32 = 0x120;
/// `exynos4412.dtsi` `arm,prefetch-offset = <7>`, `arm,double-linefill = <1>`,
/// `arm,double-linefill-incr = <0>`, `arm,double-linefill-wrap = <1>` (bit clear),
/// `arm,prefetch-drop = <1>`, `prefetch-data = <1>`, `prefetch-instr = <1>`:
/// cache-l2x0.h:136-142.
const PREFETCH_CTRL: u32 = (1 << 30) | (1 << 29) | (1 << 28) | (1 << 24) | 7;
/// Dynamic clock gating and standby mode, both on in the vendor kernel.
const POWER_CTRL: u32 = 0b11;
/// Bits kept from the current value: `arch/arm/mach-exynos/exynos.c:308` `l2c_aux_mask`.
/// This keeps the associativity and way size that describe the 1 MiB cache.
const AUX_KEEP: u32 = 0xF60F_FFFF;
/// `exynos.c:307` `l2c_aux_val` (non-secure interrupt access, shared override), plus
/// data and instruction prefetch to match [`PREFETCH_CTRL`], early BRESP and non-secure
/// lockdown access (`cache-l2x0.c`, `l2c310_enable`).
///
/// Full line of zeros stays off, unlike in the vendor kernel: it also needs `ACTLR` bit 3,
/// which the board leaves clear.
const AUX_SET: u32 = 0x0840_0000 | (1 << 29) | (1 << 28) | (1 << 30) | (1 << 26);

/// `arch/arm/mm/cache-l2x0.c:37` `CACHE_LINE_SIZE`.
const LINE_SIZE: usize = 32;

// Turns a stuck controller into a panic instead of a hang.
const POLL_LIMIT: u32 = 10_000_000;

/// Handle to the L2 cache controller. Holds no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct L2c;

impl L2c {
    /// Creates a handle.
    pub const fn new() -> Self {
        Self
    }

    fn regs(&self) -> &Registers {
        // SAFETY: L2C_BASE is this MMIO block, device memory that stays mapped for the
        // whole program, and the struct has interior mutable cells only.
        unsafe { &*(soc::L2C_BASE as *const Registers) }
    }

    /// Returns whether the L2 cache is on.
    pub fn is_enabled(&self) -> bool {
        self.regs().ctrl.get() & CTRL_EN != 0
    }

    /// Raw cache ID register.
    pub fn cache_id(&self) -> u32 {
        self.regs().cache_id.get()
    }

    /// Raw control register.
    pub fn ctrl(&self) -> u32 {
        self.regs().ctrl.get()
    }

    /// Raw auxiliary control register.
    pub fn aux_ctrl(&self) -> u32 {
        self.regs().aux_ctrl.get()
    }

    /// Raw tag latency register.
    pub fn tag_latency(&self) -> u32 {
        self.regs().tag_latency.get()
    }

    /// Raw data latency register.
    pub fn data_latency(&self) -> u32 {
        self.regs().data_latency.get()
    }

    /// Raw prefetch control register.
    pub fn prefetch_ctrl(&self) -> u32 {
        self.regs().prefetch_ctrl.get()
    }

    /// Raw power control register.
    pub fn power_ctrl(&self) -> u32 {
        self.regs().power_ctrl.get()
    }

    /// Configures and turns on the L2 cache through the secure firmware. Does nothing
    /// if it is already on.
    ///
    /// Call this after [`mmu::enable`](crate::mmu::enable) with interrupts masked, as the
    /// kernels do. Dirty L1 lines stay valid: the L2 cache simply starts out empty.
    ///
    /// # Panics
    ///
    /// If the cache is still off afterwards, meaning the firmware ignored the request.
    pub fn enable(&self) {
        if self.is_enabled() {
            return;
        }
        let aux = (self.aux_ctrl() & AUX_KEEP) | AUX_SET;
        // SAFETY: these are the commands listed in `firmware`, in the order and with the
        // argument layout both kernels use on this firmware, with the cache still off.
        unsafe {
            firmware::smc(SMC_CMD_L2X0_SETUP1, TAG_LATENCY, DATA_LATENCY, PREFETCH_CTRL);
            firmware::smc(SMC_CMD_L2X0_SETUP2, POWER_CTRL, aux, 0);
            firmware::smc(SMC_CMD_L2X0_INVALL, 0, 0, 0);
            firmware::smc(SMC_CMD_L2X0_CTRL, CTRL_EN, 0, 0);
        }
        if !self.is_enabled() {
            panic!("firmware did not enable the L2 cache: CTRL {:#x}", self.ctrl());
        }
    }

    /// Writes the dirty lines of a physical address range out to DRAM, keeping them valid.
    /// Does nothing if the cache is off. Clean the L1 cache first:
    /// [`mmu::clean_dcache_range`](crate::mmu::clean_dcache_range) does both, in that order.
    ///
    /// # Panics
    ///
    /// If the controller does not finish.
    pub fn clean_range(&self, start: usize, len: usize) {
        if len == 0 || !self.is_enabled() {
            return;
        }
        let regs = self.regs();
        let mut line = start & !(LINE_SIZE - 1);
        while line < start + len {
            regs.clean_line_pa.set(line as u32);
            line += LINE_SIZE;
        }
        regs.cache_sync.set(0);
        wait_until_zero(&regs.cache_sync, 1);
    }

    /// Invalidates every way, discarding its contents, even dirty lines.
    ///
    /// # Panics
    ///
    /// If the controller does not finish.
    pub fn invalidate_all(&self) {
        let regs = self.regs();
        let ways = if regs.aux_ctrl.get() & AUX_ASSOCIATIVITY_16 != 0 { 0xFFFF } else { 0xFF };

        regs.inv_way.set(ways);
        wait_until_zero(&regs.inv_way, ways);
        regs.cache_sync.set(0);
        wait_until_zero(&regs.cache_sync, 1);
    }
}

fn wait_until_zero(register: &ReadWrite<u32>, mask: u32) {
    for _ in 0..POLL_LIMIT {
        if register.get() & mask == 0 {
            return;
        }
    }
    panic!("L2 cache operation did not finish");
}
