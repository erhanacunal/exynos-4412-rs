//! Waiting for a level on a GPX pin with `await`, on the pin's wake-up interrupt.
//!
//! The interrupt is level-triggered and stays masked except while a task waits. The handler
//! masks it, so that a level that lasts does not keep interrupting, and wakes the task. A
//! wait first clears what an earlier level left pending; if the level is there now, the
//! interrupt becomes pending again and the wait ends at once, otherwise when the interrupt
//! comes.
//!
//! A wait can end with the level already gone: the level may be short, and the pending bit
//! may still hold an earlier one if the controller latches it later than the clear. A task
//! that waits for a device to have work therefore checks the device, and waits again if it
//! has none. The pin's data bit is not relied on while the pin is in its interrupt function.

use core::future::poll_fn;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::Poll;

use embassy_sync::waitqueue::AtomicWaker;

use crate::drivers::gic::Gic;
use crate::drivers::gpio::{Eint, Pin, Pull, Trigger};
use crate::soc;

/// GPX0 to GPX3, eight pins each.
const PINS: usize = 32;

/// One bit per pin an [`EintInput`] owns, by [`Eint::index`].
static OWNED: AtomicU32 = AtomicU32::new(0);
static WAKERS: [AtomicWaker; PINS] = [const { AtomicWaker::new() }; PINS];
static INTERRUPTS: [AtomicU32; PINS] = [const { AtomicU32::new(0) }; PINS];

/// A GPX pin read with `await`.
#[derive(Clone, Copy, Debug)]
pub struct EintInput {
    eint: Eint,
}

impl EintInput {
    /// Makes `pin` an interrupt input with `pull`, masked, its interrupt to CPU0 and enabled
    /// in the GIC. `None` if the pin is not on GPX0 to GPX3. Call it after [`Gic::init`],
    /// with the MMU on, once per pin.
    pub fn new(pin: Pin, pull: Pull) -> Option<Self> {
        let eint = pin.eint()?;
        // Owned before it can interrupt, so that the handler takes the interrupt.
        OWNED.fetch_or(1 << eint.index(), Ordering::AcqRel);
        pin.set_pull(pull);
        eint.listen(Trigger::LevelLow);
        eint.mask();
        eint.clear();
        let gic = Gic::new();
        gic.set_target(eint.irq(), 0);
        gic.enable(eint.irq());
        Some(Self { eint })
    }

    /// The pin this reads.
    pub const fn pin(&self) -> Pin {
        self.eint.pin()
    }

    /// Waits until the pin is low, or has been since the wait began: see the [module
    /// documentation](self).
    pub async fn wait_low(&self) {
        self.wait(Trigger::LevelLow).await;
    }

    /// Waits until the pin is high, as [`wait_low`](Self::wait_low).
    pub async fn wait_high(&self) {
        self.wait(Trigger::LevelHigh).await;
    }

    /// Interrupts the handler took for this pin so far.
    pub fn interrupts(&self) -> u32 {
        INTERRUPTS[usize::from(self.eint.index())].load(Ordering::Relaxed)
    }

    async fn wait(&self, trigger: Trigger) {
        let eint = self.eint;
        let waker = &WAKERS[usize::from(eint.index())];
        eint.mask();
        eint.set_trigger(trigger);
        eint.clear();
        poll_fn(|cx| {
            // Masked here: by the lines above, or by the handler, or again below.
            eint.mask();
            if eint.is_pending() {
                return Poll::Ready(());
            }
            // Registered before the unmask, so that the interrupt finds it.
            waker.register(cx.waker());
            eint.unmask();
            Poll::Pending
        })
        .await;
    }
}

/// The pins owned here that interrupt `id`, as a mask by [`Eint::index`].
fn sources(id: u16) -> u32 {
    let owned = OWNED.load(Ordering::Acquire);
    let candidates = if (soc::EINT_GPX0_IRQ..soc::EINT_GPX0_IRQ + 8).contains(&id) {
        1 << (id - soc::EINT_GPX0_IRQ)
    } else if (soc::EINT_GPX1_IRQ..soc::EINT_GPX1_IRQ + 8).contains(&id) {
        1 << (8 + id - soc::EINT_GPX1_IRQ)
    } else if id == soc::EINT_GPX23_IRQ {
        0xFFFF_0000
    } else {
        0
    };
    candidates & owned
}

/// Handles the wake-up interrupt of a pin owned here, if `id` is one: see
/// [`super::on_interrupt`]. On the GIC interrupt GPX2 and GPX3 share, it takes only pins that
/// are owned here, unmasked and pending, and leaves the others to the application.
pub(super) fn on_interrupt(id: u16) -> bool {
    let mut sources = sources(id);
    // A pin with an interrupt of its own: the interrupt is this module's even if it finds
    // nothing pending.
    let mut handled = id != soc::EINT_GPX23_IRQ && sources != 0;
    while sources != 0 {
        let index = sources.trailing_zeros() as u8;
        sources &= sources - 1;
        let Some(eint) = Eint::from_index(index) else { continue };
        if eint.is_masked() || !eint.is_pending() {
            continue;
        }
        eint.mask();
        INTERRUPTS[usize::from(index)].fetch_add(1, Ordering::Relaxed);
        WAKERS[usize::from(index)].wake();
        handled = true;
    }
    handled
}
