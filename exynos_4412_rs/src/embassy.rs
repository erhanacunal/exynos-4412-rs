//! Async tasks with [Embassy](https://embassy.dev): one executor per core, time on the MCT.
//!
//! This module provides what Embassy leaves to the platform:
//!
//! - a `critical-section` implementation for all four cores: IRQs masked on the calling core,
//!   and one lock between the cores, which the same core may take again inside;
//! - the `embassy-time` driver: [`now`] counts the MCT's global counter at 1 MHz, and wake-ups
//!   go through comparator 0, whose interrupt goes to CPU0;
//! - [`run`], which starts an executor on the calling core;
//! - [`uart::UartRx`], receiving on a UART with `await`;
//! - [`eint::EintInput`], waiting for a level on a GPX pin with `await`;
//! - async I2C transfers: `I2c::write_read_async` and the like, in
//!   [`drivers::i2c`](crate::drivers::i2c);
//! - async eMMC transfers: `Mmc::read_blocks_async` and `MmcWriter::write_async`, in
//!   [`drivers::mmc`](crate::drivers::mmc).
//!
//! The executor is `embassy-executor`'s raw one, run here: it sleeps with `WFE`, and a wake-up
//! from any core sends `DSB` then `SEV`, which wakes all of them. An interrupt also ends a
//! `WFE`, when the core takes interrupts. The crate's own Cortex-A platform is not used: its
//! `SEV` has no barrier before it, so a woken core could look at its run queue before the
//! task that woke it is there, and sleep again.
//!
//! # Order on CPU0
//!
//! ```ignore
//! mmu::enable();                  // exclusive accesses, which the locks need
//! Gic::new().init();
//! exynos_4412_rs::embassy::init(); // the MCT interrupt, to CPU0
//! cpu::enable_irq();
//! smp::start_cpu(1, cpu1_entry)?; // which calls Gic::init_this_cpu, enable_irq, then run
//! exynos_4412_rs::embassy::run(|spawner| spawner.spawn(main_task().unwrap()));
//! ```
//!
//! The application's `handle_irq` passes each interrupt to [`on_interrupt`] first.
//!
//! An application that defines tasks also depends on `embassy-executor` itself, for the
//! `task` attribute, and on `embassy-time` and `embassy-sync` as it needs them; the features
//! that select the 1 MHz tick are set here. Do not enable an `embassy-executor` platform: this
//! module provides the pender.

pub mod eint;
pub mod uart;

use core::cell::{RefCell, UnsafeCell};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::Waker;

use critical_section::{Mutex, RawRestoreState};
use embassy_executor::raw::Executor;
pub use embassy_executor::Spawner;
use embassy_time_driver::{Driver, TICK_HZ};
use embassy_time_queue_utils::Queue;

use crate::cpu;
use crate::drivers::gic::Gic;
use crate::drivers::timer::Mct;
use crate::soc;
use crate::soc::CPU_COUNT;

/// `SCTLR.M`: without the MMU, exclusive accesses cannot be relied on.
const SCTLR_M: u32 = 1;

/// MCT counts per tick of `embassy-time`.
const COUNTS_PER_TICK: u64 = soc::MCT_CLOCK_HZ as u64 / TICK_HZ;
const _: () = assert!(COUNTS_PER_TICK * TICK_HZ == soc::MCT_CLOCK_HZ as u64, "tick must divide the MCT clock");

// --- critical-section -------------------------------------------------------------------------

/// No core holds the lock.
const NO_OWNER: u32 = u32::MAX;
/// The core that holds the critical section, or [`NO_OWNER`].
static OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

/// Bits of the restore state: IRQs were masked before, and the section was already held by
/// this core, so leaving it must not release the lock.
const STATE_IRQ_MASKED: u8 = 1 << 0;
const STATE_NESTED: u8 = 1 << 1;

struct MultiCore;

critical_section::set_impl!(MultiCore);

// SAFETY: `acquire` masks IRQs on this core, then takes `OWNER`, so one core at a time runs
// inside, and nothing else on it does; a nested `acquire` on the owning core leaves the lock
// held, and its `release` keeps it, as the restore state records.
unsafe impl critical_section::Impl for MultiCore {
    unsafe fn acquire() -> RawRestoreState {
        assert!(cpu::read_sctlr() & SCTLR_M != 0, "critical section needs the MMU on");
        let irq_masked = cpu::mask_irq();
        let me = cpu::id() as u32;
        // Only this core stores its own number, and its IRQs are masked: no race here.
        if OWNER.load(Ordering::Relaxed) == me {
            return STATE_NESTED | if irq_masked { STATE_IRQ_MASKED } else { 0 };
        }
        while OWNER.compare_exchange_weak(NO_OWNER, me, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        if irq_masked { STATE_IRQ_MASKED } else { 0 }
    }

    unsafe fn release(state: RawRestoreState) {
        if state & STATE_NESTED == 0 {
            OWNER.store(NO_OWNER, Ordering::Release);
        }
        cpu::restore_irq(state & STATE_IRQ_MASKED != 0);
    }
}

// --- embassy-time driver ----------------------------------------------------------------------

struct MctDriver {
    /// Every timer of every core: one queue, so that a task's timer is never in two.
    queue: Mutex<RefCell<Queue>>,
}

embassy_time_driver::time_driver_impl!(static DRIVER: MctDriver = MctDriver { queue: Mutex::new(RefCell::new(Queue::new())) });

/// Ticks of 1 µs since the MCT counter started.
pub fn now() -> u64 {
    Mct::new().count() / COUNTS_PER_TICK
}

/// Arms the comparator for tick `at`, or turns it off for `u64::MAX`, no timer pending.
/// Returns `false` if `at` has already passed, when the alarm may never come.
fn set_alarm(at: u64) -> bool {
    let mct = Mct::new();
    if at == u64::MAX {
        mct.stop_periodic();
        return true;
    }
    mct.set_alarm(at.saturating_mul(COUNTS_PER_TICK));
    now() < at
}

/// Wakes the timers that are due and arms the comparator for the next one.
fn service_queue(queue: &mut Queue) {
    let mut next = queue.next_expiration(now());
    while !set_alarm(next) {
        next = queue.next_expiration(now());
    }
}

impl Driver for MctDriver {
    fn now(&self) -> u64 {
        now()
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow_ref_mut(cs);
            if queue.schedule_wake(at, waker) {
                service_queue(&mut queue);
            }
        });
    }
}

/// Starts the MCT counter and routes its comparator interrupt to CPU0, enabled. Call it on
/// CPU0, after [`Gic::init`], before any timer is used.
pub fn init() {
    Mct::new().start();
    let gic = Gic::new();
    gic.set_target(soc::MCT_G0_IRQ, 0);
    gic.enable(soc::MCT_G0_IRQ);
}

/// Handles interrupt `id` if it is this module's, returning whether it was: the MCT's, those
/// of the UARTs that [`uart::UartRx`] reads, those of the pins of [`eint::EintInput`], and
/// those of the I2C and eMMC controllers for their async transfers. The application's `handle_irq` calls it
/// after acknowledging the interrupt, and ends it after.
pub fn on_interrupt(id: u16) -> bool {
    if id != soc::MCT_G0_IRQ {
        return uart::on_interrupt(id)
            || eint::on_interrupt(id)
            || crate::drivers::i2c::on_interrupt(id)
            || crate::drivers::mmc::on_interrupt(id);
    }
    Mct::new().clear_interrupt();
    critical_section::with(|cs| service_queue(&mut DRIVER.queue.borrow_ref_mut(cs)));
    true
}

// --- executors --------------------------------------------------------------------------------

/// The pender `embassy-executor` calls when a task of an executor becomes ready, from any core
/// or from an interrupt. `DSB` makes the run queue's update visible before the event that
/// sends the cores to look at it, as ARM recommends before `SEV`.
#[unsafe(export_name = "__pender")]
fn pender(_context: *mut ()) {
    // SAFETY: barriers and an event, no memory access of their own.
    unsafe { core::arch::asm!("dsb ish", "sev", options(nostack, preserves_flags)) };
}

struct Executors([UnsafeCell<MaybeUninit<Executor>>; CPU_COUNT]);

// SAFETY: `run` lets each core reach only its own slot, and only once.
unsafe impl Sync for Executors {}

static EXECUTORS: Executors = Executors([const { UnsafeCell::new(MaybeUninit::uninit()) }; CPU_COUNT]);
static STARTED: [AtomicBool; CPU_COUNT] = [const { AtomicBool::new(false) }; CPU_COUNT];

/// Runs an executor on the calling core, after `init` has spawned its first tasks. Tasks
/// spawned from it stay on this core.
///
/// # Panics
///
/// If an executor already runs on this core, or the MMU is off.
pub fn run(init: impl FnOnce(Spawner)) -> ! {
    assert!(cpu::read_sctlr() & SCTLR_M != 0, "the executor needs the MMU on");
    let core = cpu::id();
    assert!(!STARTED[core].swap(true, Ordering::AcqRel), "an executor already runs on CPU{}", core);
    // SAFETY: `STARTED` lets only this core, once, reach its slot, which lives for the whole
    // program.
    let executor: &'static Executor =
        unsafe { (*EXECUTORS.0[core].get()).write(Executor::new(core as *mut ())) };
    init(executor.spawner());
    loop {
        // SAFETY: only this loop polls this executor, never from inside a poll.
        unsafe { executor.poll() };
        // An event since the last one, a wake-up during the poll, ends this at once.
        // SAFETY: waits for an event or an interrupt, nothing else.
        unsafe { core::arch::asm!("wfe", options(nostack, preserves_flags)) };
    }
}
