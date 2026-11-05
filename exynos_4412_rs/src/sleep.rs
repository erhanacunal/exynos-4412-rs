//! Waiting asleep instead of busy polling: the core stops in `wfi` until an interrupt line it
//! waits for rises, then checks whether what it waits for has happened.
//!
//! No interrupt is taken. IRQs stay masked in the CPSR while [`until`] runs; the device's
//! interrupt is only enabled in the GIC, which is enough to wake the core from `wfi`: the ARMv7-A
//! Architecture Reference Manual's "Wait For Interrupt" wakes on an IRQ even while CPSR.I masks
//! it. The `ready` check then clears whatever the device flagged, so that the line falls and the
//! next `wfi` sleeps. A timeout is the MCT's comparator 0 ([`Mct::set_alarm`]), whose interrupt
//! wakes the core the same way.
//!
//! Each write to the comparator waits for the MCT's clock domain, so the alarm stays armed from
//! one wait to the next and is set again only when it would come after the new deadline or has
//! passed: one early wake-up then costs less than setting it every time. Nothing else may set
//! the comparator while its interrupt is disabled in the GIC. It is taken only if nothing else
//! has that interrupt enabled: under Embassy's time driver, which owns it, [`until`] still
//! sleeps between interrupts, but times out only when one of them wakes it. Everything here
//! runs on the calling core.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::drivers::gic::Gic;
use crate::drivers::timer::Mct;
use crate::{cpu, soc};

/// How many MCT counts the cores have spent in `wfi` here, in all.
static SLEPT: AtomicU64 = AtomicU64::new(0);
/// Wake-ups from `wfi` by the interrupt pending then, `WAKE_IDS` for none or an ID beyond.
const WAKE_IDS: usize = 160;
static WAKES: [AtomicU32; WAKE_IDS + 1] = [const { AtomicU32::new(0) }; WAKE_IDS + 1];

/// Takes the wake-up counts since the last call: calls `each` with every interrupt ID that
/// woke the core, `None` for wake-ups with nothing pending, and how many times.
pub fn take_wakes(mut each: impl FnMut(Option<u16>, u32)) {
    for (id, count) in WAKES.iter().enumerate() {
        let count = count.swap(0, Ordering::Relaxed);
        if count != 0 {
            each((id < WAKE_IDS).then_some(id as u16), count);
        }
    }
}

/// The count comparator 0 is set to, 0 if it has not been here.
static ARMED: AtomicU64 = AtomicU64::new(0);

/// Sets the alarm to `at`, unless one is armed that comes no later and has not passed.
fn arm(mct: Mct, at: u64, now: u64) {
    let armed = ARMED.load(Ordering::Relaxed);
    if armed == 0 || armed > at || armed <= now {
        mct.set_alarm(at);
        ARMED.store(at, Ordering::Relaxed);
    }
}

/// MCT counts spent asleep in [`until`] since the start, at [`soc::MCT_CLOCK_HZ`]. The share of
/// an interval asleep is the difference of two readings over the interval's length.
pub fn slept() -> u64 {
    SLEPT.load(Ordering::Relaxed)
}

/// Sleeps until `ready` returns true, or until the MCT counter reaches `deadline`. Returns
/// whether `ready` did. `ready` runs first, then after each wake-up; it must clear the device's
/// flag for `irqs` before it checks, so that nothing raised afterwards is lost, or leave it
/// raised only when it returns true. `irqs` are GIC interrupt IDs, enabled while this runs and
/// left as they were afterwards.
///
/// The MCT counter must run ([`Mct::start`]) for `deadline`. Runs [`Gic::init`] if nothing
/// has: U-Boot leaves interrupts enabled there, a level one of which would keep `wfi` from
/// sleeping (on the board, `gpx0-3`'s, ID 51).
pub fn until(mct: Mct, irqs: &[u16], deadline: Option<u64>, mut ready: impl FnMut() -> bool) -> bool {
    let gic = Gic::new();
    if !gic.is_initialized() {
        gic.init();
    }
    let was_masked = cpu::mask_irq();
    let mut enabled_before = 0u32;
    for (i, &id) in irqs.iter().enumerate().take(32) {
        if gic.is_enabled(id) {
            enabled_before |= 1 << i;
        } else {
            gic.enable(id);
        }
    }
    let alarm = deadline.filter(|_| !gic.is_enabled(soc::MCT_G0_IRQ));
    if let Some(at) = alarm {
        arm(mct, at, mct.count());
        gic.enable(soc::MCT_G0_IRQ);
    }

    let done = loop {
        if ready() {
            break true;
        }
        let now = mct.count();
        if deadline.is_some_and(|at| now >= at) {
            break false;
        }
        // An earlier alarm has gone off, so its interrupt is pending: the real deadline instead.
        if let Some(at) = alarm {
            arm(mct, at, now);
        }
        // SAFETY: `wfi` only suspends the core until an interrupt is pending at its interface.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) };
        SLEPT.fetch_add(mct.count() - now, Ordering::Relaxed);
        let id = gic.highest_pending().map_or(WAKE_IDS, |id| usize::from(id).min(WAKE_IDS));
        WAKES[id].fetch_add(1, Ordering::Relaxed);
    };

    if alarm.is_some() {
        gic.disable(soc::MCT_G0_IRQ);
    }
    for (i, &id) in irqs.iter().enumerate().take(32) {
        if enabled_before & (1 << i) == 0 {
            gic.disable(id);
        }
    }
    cpu::restore_irq(was_masked);
    done
}

/// Sleeps until the MCT counter reaches `at`.
pub fn until_count(mct: Mct, at: u64) {
    until(mct, &[], Some(at), || false);
}

/// Sleeps for `us` microseconds.
pub fn for_us(mct: Mct, us: u64) {
    until_count(mct, mct.count() + us * (soc::MCT_CLOCK_HZ as u64 / 1_000_000));
}

/// Sleeps until a byte comes in on the console's UART, or until `deadline`, and returns it.
/// Leaves the UART's receive interrupt unmasked and its transmit interrupt masked; the console
/// writes by polling.
pub fn read_key(mct: Mct, deadline: Option<u64>) -> Option<u8> {
    let uart = crate::console::uart();
    let irq = uart.id().irq()?;
    uart.enable_rx_interrupt();
    until(mct, &[irq], deadline, || {
        uart.clear_rx_interrupt();
        uart.rx_ready()
    });
    uart.try_read_byte()
}
