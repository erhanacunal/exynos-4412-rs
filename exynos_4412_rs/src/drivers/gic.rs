//! GIC distributor and CPU interface.
//!
//! Register offsets are verified against Linux 5.15.198
//! `include/linux/irqchip/arm-gic.h`, cited as `arm-gic.h:<line>`.
//!
//! The board runs in the Non-secure view of the GIC (measured by `mmu_test`). The
//! accesses below mean the same in both views: bit 0 of each control
//! register enables the interrupts of the current world, and interrupts reach the
//! core as IRQ.
//!
//! This GIC has no banked registers: both the distributor and the CPU interface have one
//! window per CPU, [`soc::GIC_CPU_STRIDE`] apart. Every access here goes through the
//! calling core's window, so one handle works from any core.
//!
//! # Concurrency
//!
//! [`init`](Gic::init) runs once, on CPU0, before anything else. Configuring SPIs is a
//! read-modify-write on shared registers: do it from one core at a time. Acknowledging,
//! finishing and sending interrupts is safe from all cores at once.

use core::arch::asm;

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite, WriteOnly};
use tock_registers::register_structs;

use crate::{cpu, soc};

register_structs! {
    DistRegisters {
        /// arm-gic.h:42 `GIC_DIST_CTRL`.
        (0x000 => ctlr: ReadWrite<u32>),
        /// arm-gic.h:43 `GIC_DIST_CTR`. Bits 4:0 give the number of 32-interrupt banks, minus 1.
        (0x004 => typer: ReadOnly<u32>),
        (0x008 => _reserved0),
        /// arm-gic.h:46 `GIC_DIST_ENABLE_SET`.
        (0x100 => isenabler: [WriteOnly<u32>; 32]),
        /// arm-gic.h:47 `GIC_DIST_ENABLE_CLEAR`.
        (0x180 => icenabler: [WriteOnly<u32>; 32]),
        /// arm-gic.h:48 `GIC_DIST_PENDING_SET`. Reads give the pending state.
        (0x200 => ispendr: [ReadOnly<u32>; 32]),
        /// arm-gic.h:49 `GIC_DIST_PENDING_CLEAR`.
        (0x280 => icpendr: [WriteOnly<u32>; 32]),
        /// arm-gic.h:50 `GIC_DIST_ACTIVE_SET`. Reads give the active state.
        (0x300 => isactiver: [ReadOnly<u32>; 32]),
        (0x380 => _reserved2),
        /// arm-gic.h:52 `GIC_DIST_PRI`.
        (0x400 => ipriorityr: [ReadWrite<u32>; 256]),
        /// arm-gic.h:53 `GIC_DIST_TARGET`.
        (0x800 => itargetsr: [ReadWrite<u32>; 256]),
        /// arm-gic.h:54 `GIC_DIST_CONFIG`.
        (0xC00 => icfgr: [ReadWrite<u32>; 64]),
        (0xD00 => _reserved3),
        /// arm-gic.h:55 `GIC_DIST_SOFTINT`.
        (0xF00 => sgir: WriteOnly<u32>),
        (0xF04 => _reserved4),
        /// arm-gic.h:56 `GIC_DIST_SGI_PENDING_CLEAR`. One byte per SGI, one bit per sender.
        (0xF10 => cpendsgir: [WriteOnly<u32>; 4]),
        (0xF20 => @END),
    }
}

register_structs! {
    CpuRegisters {
        /// arm-gic.h:10 `GIC_CPU_CTRL`.
        (0x00 => ctlr: ReadWrite<u32>),
        /// arm-gic.h:11 `GIC_CPU_PRIMASK`.
        (0x04 => pmr: ReadWrite<u32>),
        (0x08 => _reserved0),
        /// arm-gic.h:13 `GIC_CPU_INTACK`.
        (0x0C => iar: ReadOnly<u32>),
        /// arm-gic.h:14 `GIC_CPU_EOI`.
        (0x10 => eoir: WriteOnly<u32>),
        (0x14 => @END),
    }
}

/// arm-gic.h:59 `GICD_ENABLE` and arm-gic.h:22 `GICC_ENABLE`.
const ENABLE: u32 = 1;
/// arm-gic.h:38 `GICC_IAR_INT_ID_MASK`.
const IAR_ID_MASK: u32 = 0x3FF;
/// arm-gic.h:39 `GICC_INT_SPURIOUS`.
const SPURIOUS_ID: u16 = 1023;
/// Unmasks every priority numerically below this: arm-gic.h:23 `GICC_INT_PRI_THRESHOLD`.
const PRIORITY_THRESHOLD: u32 = 0xF0;
/// Default interrupt priority: `drivers/irqchip/irq-gic-common.h:12` `GICD_INT_DEF_PRI`.
const DEFAULT_PRIORITY: u32 = 0xA0;
/// `GIC_DIST_SOFTINT` target list: `drivers/irqchip/irq-gic.c:846`.
const SGIR_TARGET_SHIFT: u32 = 16;
/// Bits of the acknowledge register holding the sender of an SGI: arm-gic.h:40.
const IAR_SOURCE_SHIFT: u32 = 10;
/// Number of software generated interrupts, IDs 0 to 15.
const SGI_COUNT: u16 = 16;
/// First shared peripheral interrupt; lower IDs are banked per CPU.
const FIRST_SPI: u16 = 32;

/// An interrupt taken from [`Gic::acknowledge`], to be finished with
/// [`Gic::end_of_interrupt`].
#[derive(Debug)]
#[must_use = "an acknowledged interrupt stays active until its end_of_interrupt"]
pub struct AckedIrq {
    raw: u32,
}

impl AckedIrq {
    /// The GIC interrupt ID.
    pub const fn id(&self) -> u16 {
        (self.raw & IAR_ID_MASK) as u16
    }

    /// The core that sent this interrupt, if it is an SGI.
    pub const fn source_cpu(&self) -> Option<usize> {
        if self.id() < SGI_COUNT {
            Some(((self.raw >> IAR_SOURCE_SHIFT) & 7) as usize)
        } else {
            None
        }
    }
}

/// Handle to the GIC. Holds no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gic;

impl Gic {
    /// Creates a handle.
    pub const fn new() -> Self {
        Self
    }

    fn dist(&self) -> &DistRegisters {
        // SAFETY: GIC_DIST_BASE is this MMIO block, device memory that stays mapped for
        // the whole program, and the struct has interior mutable cells only.
        unsafe { &*((soc::GIC_DIST_BASE + window_offset()) as *const DistRegisters) }
    }

    fn cpu(&self) -> &CpuRegisters {
        // SAFETY: as in `dist`, for GIC_CPU_BASE.
        unsafe { &*((soc::GIC_CPU_BASE + window_offset()) as *const CpuRegisters) }
    }

    /// Disables every interrupt left on by the bootloader and turns the GIC on, for the
    /// whole chip and for this core. CPU0 only, once.
    ///
    /// Interrupts are delivered once enabled with [`enable`](Self::enable) and
    /// unmasked with [`cpu::enable_irq`].
    pub fn init(&self) {
        let dist = self.dist();

        dist.ctlr.set(0);
        self.cpu().ctlr.set(0);

        let banks = (dist.typer.get() & 0x1F) as usize + 1;
        for bank in 0..banks {
            dist.icenabler[bank].set(u32::MAX);
            dist.icpendr[bank].set(u32::MAX);
        }

        dist.ctlr.set(ENABLE);
        self.init_this_cpu();
    }

    /// Turns on this core's CPU interface and clears its per-CPU interrupts. Every
    /// secondary core calls it once; [`init`](Self::init) does it for CPU0.
    ///
    /// This tries to discard the SGI that [`smp::start_cpu`](crate::smp::start_cpu) used to
    /// wake the core, but on the board each core still took one afterwards (`smp_test`).
    /// Its interrupt handler must tolerate one [`smp::WAKE_SGI`](crate::smp::WAKE_SGI).
    pub fn init_this_cpu(&self) {
        let dist = self.dist();
        let cpu = self.cpu();

        cpu.ctlr.set(0);
        dist.icenabler[0].set(u32::MAX);
        dist.icpendr[0].set(u32::MAX);
        for sgis in &dist.cpendsgir {
            sgis.set(u32::MAX);
        }
        cpu.pmr.set(PRIORITY_THRESHOLD);
        cpu.ctlr.set(ENABLE);
    }

    /// Enables an interrupt at the default priority. An SPI is routed to the calling core
    /// and configured as level-sensitive, as every SPI this crate uses is. An SGI or a PPI
    /// is enabled for the calling core only.
    pub fn enable(&self, id: u16) {
        let dist = self.dist();
        let index = id as usize;

        write_byte_field(&dist.ipriorityr[index / 4], index % 4, DEFAULT_PRIORITY);
        if id >= FIRST_SPI {
            write_byte_field(&dist.itargetsr[index / 4], index % 4, 1 << cpu::id());
            // Two configuration bits per interrupt; the upper one selects edge.
            let edge_bit = 1 << (index % 16 * 2 + 1);
            let config = &dist.icfgr[index / 16];
            config.set(config.get() & !edge_bit);
        }
        dist.isenabler[index / 32].set(1 << (index % 32));
    }

    /// Routes an SPI to the core `target`, whichever core enabled it.
    pub fn set_target(&self, id: u16, target: usize) {
        assert!(id >= FIRST_SPI && target < soc::CPU_COUNT);
        let index = id as usize;
        write_byte_field(&self.dist().itargetsr[index / 4], index % 4, 1 << target);
    }

    /// Whether an interrupt is pending and whether it is active, in this core's view.
    pub fn state(&self, id: u16) -> (bool, bool) {
        let index = id as usize;
        let mask = 1 << (index % 32);
        let dist = self.dist();
        (dist.ispendr[index / 32].get() & mask != 0, dist.isactiver[index / 32].get() & mask != 0)
    }

    /// The raw `ITARGETSR` byte of an interrupt: one bit per core.
    pub fn target_mask(&self, id: u16) -> u8 {
        let index = id as usize;
        (self.dist().itargetsr[index / 4].get() >> (index % 4 * 8)) as u8
    }

    /// Sends the software interrupt `id`, below 16, to the core `target`.
    pub fn send_sgi(&self, target: usize, id: u16) {
        assert!(id < SGI_COUNT && target < soc::CPU_COUNT);
        // SAFETY: a barrier has no effect other than ordering: earlier writes to normal
        // memory must be visible before the target can react (irq-gic.c:840-843, there
        // `dmb`; here `dsb` so that this also holds against the device write below).
        unsafe {
            asm!("dsb", options(nostack, preserves_flags));
        }
        self.dist().sgir.set((1 << (SGIR_TARGET_SHIFT + target as u32)) | u32::from(id));
    }

    /// Disables an interrupt.
    pub fn disable(&self, id: u16) {
        let index = id as usize;
        self.dist().icenabler[index / 32].set(1 << (index % 32));
    }

    /// Takes the highest priority pending interrupt, or `None` if the wake-up was spurious.
    pub fn acknowledge(&self) -> Option<AckedIrq> {
        let raw = self.cpu().iar.get();
        ((raw & IAR_ID_MASK) as u16 != SPURIOUS_ID).then_some(AckedIrq { raw })
    }

    /// Signals that an interrupt is finished. Clear a level-sensitive source first,
    /// or the interrupt fires again immediately.
    pub fn end_of_interrupt(&self, irq: AckedIrq) {
        self.cpu().eoir.set(irq.raw);
    }
}

fn window_offset() -> usize {
    cpu::id() * soc::GIC_CPU_STRIDE
}

// Word access, valid on every GIC, unlike byte access to these registers.
fn write_byte_field(register: &ReadWrite<u32>, byte: usize, value: u32) {
    let shift = byte * 8;
    register.set((register.get() & !(0xFF << shift)) | (value << shift));
}
