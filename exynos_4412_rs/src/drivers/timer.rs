//! Multi-core timer: the free-running global counter, its comparator 0 ([`Mct`]), and one
//! independent interval timer per core ([`LocalTimer`]).
//!
//! Register offsets and bits are verified against Linux 5.15.198
//! `drivers/clocksource/exynos_mct.c`, cited as `exynos_mct.c:<line>`.

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite, WriteOnly};
use tock_registers::register_structs;

use crate::soc;

register_structs! {
    Registers {
        (0x000 => _reserved0),
        /// exynos_mct.c:25 `EXYNOS4_MCT_G_CNT_L`.
        (0x100 => g_cnt_l: ReadOnly<u32>),
        /// exynos_mct.c:26 `EXYNOS4_MCT_G_CNT_U`.
        (0x104 => g_cnt_u: ReadOnly<u32>),
        (0x108 => _reserved1),
        /// exynos_mct.c:28 `EXYNOS4_MCT_G_COMP0_L`.
        (0x200 => g_comp0_l: WriteOnly<u32>),
        /// exynos_mct.c:29 `EXYNOS4_MCT_G_COMP0_U`.
        (0x204 => g_comp0_u: WriteOnly<u32>),
        /// exynos_mct.c:30 `EXYNOS4_MCT_G_COMP0_ADD_INCR`.
        (0x208 => g_comp0_add_incr: WriteOnly<u32>),
        (0x20C => _reserved2),
        /// exynos_mct.c:31 `EXYNOS4_MCT_G_TCON`.
        (0x240 => g_tcon: ReadWrite<u32>),
        /// exynos_mct.c:32 `EXYNOS4_MCT_G_INT_CSTAT`.
        (0x244 => g_int_cstat: WriteOnly<u32>),
        /// exynos_mct.c:33 `EXYNOS4_MCT_G_INT_ENB`.
        (0x248 => g_int_enb: WriteOnly<u32>),
        /// exynos_mct.c:34 `EXYNOS4_MCT_G_WSTAT`.
        (0x24C => g_wstat: ReadWrite<u32>),
        (0x250 => @END),
    }
}

/// exynos_mct.c:45 `MCT_G_TCON_START`.
const TCON_START: u32 = 1 << 8;
/// exynos_mct.c:46 `MCT_G_TCON_COMP0_AUTO_INC`.
const TCON_COMP0_AUTO_INC: u32 = 1 << 1;
/// exynos_mct.c:47 `MCT_G_TCON_COMP0_ENABLE`.
const TCON_COMP0_ENABLE: u32 = 1 << 0;

/// exynos_mct.c:126 G_COMP0_L write status.
const WSTAT_COMP0_L: u32 = 1 << 0;
/// exynos_mct.c:130 G_COMP0_U write status.
const WSTAT_COMP0_U: u32 = 1 << 1;
/// exynos_mct.c:134 G_COMP0_ADD_INCR write status.
const WSTAT_COMP0_ADD_INCR: u32 = 1 << 2;
/// exynos_mct.c:122 G_TCON write status.
const WSTAT_TCON: u32 = 1 << 16;

/// exynos_mct.c:286, exynos_mct.c:334: bit 0 of G_INT_ENB and G_INT_CSTAT is comparator 0.
const INT_COMP0: u32 = 1;

// Generous: an uncached register read takes far longer than the ~1 ms the hardware may
// need, and the bound exists only to turn a dead clock into a panic instead of a hang.
const WSTAT_POLL_LIMIT: u32 = 1_000_000;

/// Handle to the MCT global timer. Holds no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mct;

impl Mct {
    /// Creates a handle.
    pub const fn new() -> Self {
        Self
    }

    fn regs(&self) -> &Registers {
        // SAFETY: MCT_BASE is this MMIO block, device memory that stays mapped for the
        // whole program, and the struct has interior mutable cells only.
        unsafe { &*(soc::MCT_BASE as *const Registers) }
    }

    // Writes to these registers cross into the timer's clock domain. Each sets a status
    // bit when it has been applied, and the bit is cleared by writing 1 to it.
    fn wait_write(&self, mask: u32) {
        let regs = self.regs();
        for _ in 0..WSTAT_POLL_LIMIT {
            if regs.g_wstat.get() & mask != 0 {
                regs.g_wstat.set(mask);
                return;
            }
        }
        panic!("MCT write not applied, status mask {:#x}: is its clock running?", mask);
    }

    fn set_tcon(&self, value: u32) {
        self.regs().g_tcon.set(value);
        self.wait_write(WSTAT_TCON);
    }

    /// Starts the global counter if it is not running. The value is kept.
    pub fn start(&self) {
        let tcon = self.regs().g_tcon.get();
        if tcon & TCON_START == 0 {
            self.set_tcon(tcon | TCON_START);
        }
    }

    /// Reads the 64-bit global counter, which counts at [`soc::MCT_CLOCK_HZ`].
    pub fn count(&self) -> u64 {
        let regs = self.regs();
        let mut upper = regs.g_cnt_u.get();
        loop {
            let lower = regs.g_cnt_l.get();
            let upper_again = regs.g_cnt_u.get();
            if upper == upper_again {
                return (u64::from(upper) << 32) | u64::from(lower);
            }
            upper = upper_again;
        }
    }

    /// Starts the counter and raises [`soc::MCT_G0_IRQ`] every `period` counts.
    ///
    /// The interrupt handler must call [`clear_interrupt`](Self::clear_interrupt).
    pub fn start_periodic(&self, period: u32) {
        let regs = self.regs();

        self.stop_periodic();
        self.start();

        regs.g_comp0_add_incr.set(period);
        self.wait_write(WSTAT_COMP0_ADD_INCR);

        let first = self.count().wrapping_add(u64::from(period));
        regs.g_comp0_l.set(first as u32);
        self.wait_write(WSTAT_COMP0_L);
        regs.g_comp0_u.set((first >> 32) as u32);
        self.wait_write(WSTAT_COMP0_U);

        self.clear_interrupt();
        regs.g_int_enb.set(INT_COMP0);
        self.set_tcon(regs.g_tcon.get() | TCON_COMP0_ENABLE | TCON_COMP0_AUTO_INC);
    }

    /// Stops the comparator interrupt. The counter keeps running.
    pub fn stop_periodic(&self) {
        let regs = self.regs();
        self.set_tcon(regs.g_tcon.get() & !(TCON_COMP0_ENABLE | TCON_COMP0_AUTO_INC));
        regs.g_int_enb.set(0);
    }

    /// Clears a pending comparator interrupt.
    pub fn clear_interrupt(&self) {
        self.regs().g_int_cstat.set(INT_COMP0);
    }
}

register_structs! {
    LocalRegisters {
        /// exynos_mct.c:39 `MCT_L_TCNTB_OFFSET`.
        (0x00 => tcntb: WriteOnly<u32>),
        (0x04 => _reserved0),
        /// exynos_mct.c:40 `MCT_L_ICNTB_OFFSET`.
        (0x08 => icntb: WriteOnly<u32>),
        (0x0C => _reserved1),
        /// exynos_mct.c:41 `MCT_L_TCON_OFFSET`.
        (0x20 => tcon: ReadWrite<u32>),
        (0x24 => _reserved2),
        /// exynos_mct.c:42 `MCT_L_INT_CSTAT_OFFSET`.
        (0x30 => int_cstat: ReadWrite<u32>),
        /// exynos_mct.c:43 `MCT_L_INT_ENB_OFFSET`.
        (0x34 => int_enb: WriteOnly<u32>),
        (0x38 => _reserved3),
        /// exynos_mct.c:44 `MCT_L_WSTAT_OFFSET`.
        (0x40 => wstat: ReadWrite<u32>),
        (0x44 => @END),
    }
}

/// exynos_mct.c:35-36: the block of local timer `n` is at `0x300 + 0x100 * n`.
const fn local_base(cpu: usize) -> usize {
    soc::MCT_BASE + 0x300 + 0x100 * cpu
}

/// exynos_mct.c:48 `MCT_L_TCON_INTERVAL_MODE`.
const L_TCON_INTERVAL_MODE: u32 = 1 << 2;
/// exynos_mct.c:49 `MCT_L_TCON_INT_START`.
const L_TCON_INT_START: u32 = 1 << 1;
/// exynos_mct.c:50 `MCT_L_TCON_TIMER_START`.
const L_TCON_TIMER_START: u32 = 1 << 0;
/// exynos_mct.c:377 `MCT_L_UPDATE_ICNTB`.
const L_ICNTB_UPDATE: u32 = 1 << 31;
/// exynos_mct.c:107 L_TCON write status.
const L_WSTAT_TCON: u32 = 1 << 3;
/// exynos_mct.c:110 L_ICNTB write status.
const L_WSTAT_ICNTB: u32 = 1 << 1;
/// exynos_mct.c:113 L_TCNTB write status.
const L_WSTAT_TCNTB: u32 = 1 << 0;
/// exynos_mct.c:383, exynos_mct.c:394: bit 0 of L_INT_ENB and L_INT_CSTAT is the tick.
const L_INT_TICK: u32 = 1;
/// exynos_mct.c:52 `TICK_BASE_CNT`: each interval count takes this plus one clocks.
const L_TICK_BASE_COUNT: u32 = 1;
/// Global counter counts per interval count.
const L_COUNTS_PER_INTERVAL: u32 = L_TICK_BASE_COUNT + 1;
/// Shortest period accepted, in counts: `exynos_mct.c:484-485` lets Linux program no fewer
/// than 0xf interval counts, so nothing shorter is known to work.
const L_MIN_PERIOD: u32 = (0xF + 1) * L_COUNTS_PER_INTERVAL;

/// One core's interval timer. Raises [`soc::MCT_LOCAL_IRQ`] on that core only, and has
/// no state beyond its registers.
///
/// Any core may program any timer, but not the same one at the same time. The core
/// that takes the interrupt must have enabled it with
/// [`Gic::enable`](crate::drivers::gic::Gic::enable), and must call
/// [`clear_interrupt`](Self::clear_interrupt) in its handler. To fire once, call
/// [`stop`](Self::stop) there as well, before clearing.
#[derive(Clone, Copy, Debug)]
pub struct LocalTimer {
    cpu: usize,
}

impl LocalTimer {
    /// The timer of the core `cpu`.
    ///
    /// # Panics
    ///
    /// If there is no such core.
    pub const fn new(cpu: usize) -> Self {
        assert!(cpu < soc::CPU_COUNT);
        Self { cpu }
    }

    /// The timer of the calling core.
    pub fn this_cpu() -> Self {
        Self::new(crate::cpu::id())
    }

    fn regs(&self) -> &LocalRegisters {
        // SAFETY: `new` checked that this is one of the MMIO blocks of the MCT, device
        // memory that stays mapped for the whole program, and the struct has interior
        // mutable cells only.
        unsafe { &*(local_base(self.cpu) as *const LocalRegisters) }
    }

    // Same protocol as `Mct::wait_write`, with this timer's own status register.
    fn wait_write(&self, mask: u32) {
        let regs = self.regs();
        for _ in 0..WSTAT_POLL_LIMIT {
            if regs.wstat.get() & mask != 0 {
                regs.wstat.set(mask);
                return;
            }
        }
        panic!("MCT local timer {} write not applied, status mask {:#x}", self.cpu, mask);
    }

    /// Raises the interrupt every `period` counts of the global counter's clock,
    /// [`soc::MCT_CLOCK_HZ`], restarting the timer if it was running.
    ///
    /// # Panics
    ///
    /// If `period` is odd, because the timer divides its clock by 2 first
    /// (`exynos_mct.c:52,472`), or below 32.
    pub fn start_periodic(&self, period: u32) {
        assert!(period >= L_MIN_PERIOD && period.is_multiple_of(L_COUNTS_PER_INTERVAL));
        let regs = self.regs();

        self.stop();

        regs.tcntb.set(L_TICK_BASE_COUNT);
        self.wait_write(L_WSTAT_TCNTB);
        // Minus one: with `n` here the period measured on the board (`timer_test`) was
        // `2 * n + 2` counts, at each of 240000, 96000, 48000 and 24000.
        regs.icntb.set(L_ICNTB_UPDATE | (period / L_COUNTS_PER_INTERVAL - 1));
        self.wait_write(L_WSTAT_ICNTB);
        regs.int_enb.set(L_INT_TICK);
        regs.tcon.set(regs.tcon.get() | L_TCON_INT_START | L_TCON_TIMER_START | L_TCON_INTERVAL_MODE);
        self.wait_write(L_WSTAT_TCON);
    }

    /// Stops the timer. An interrupt already raised stays pending until cleared.
    pub fn stop(&self) {
        let regs = self.regs();
        let tcon = regs.tcon.get();
        if tcon & (L_TCON_INT_START | L_TCON_TIMER_START) != 0 {
            regs.tcon.set(tcon & !(L_TCON_INT_START | L_TCON_TIMER_START));
            self.wait_write(L_WSTAT_TCON);
        }
    }

    /// Clears a pending interrupt.
    pub fn clear_interrupt(&self) {
        let regs = self.regs();
        if regs.int_cstat.get() & L_INT_TICK != 0 {
            regs.int_cstat.set(L_INT_TICK);
        }
    }
}
