//! Buffers in `.bss` instead of on the stack, each handed out once.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

/// `N` bytes that one caller can take, for good.
pub struct Once<const N: usize> {
    taken: AtomicBool,
    bytes: UnsafeCell<[u8; N]>,
}

// SAFETY: the bytes are reached only through `take`, which lets one caller through.
unsafe impl<const N: usize> Sync for Once<N> {}

impl<const N: usize> Once<N> {
    pub const fn new() -> Self {
        Self {
            taken: AtomicBool::new(false),
            bytes: UnsafeCell::new([0; N]),
        }
    }

    /// The bytes, the first time. `None` after that. Exclusive access needs the MMU on, as for
    /// any atomic.
    #[expect(clippy::mut_from_ref, reason = "the flag makes the mutable borrow unique")]
    pub fn take(&'static self) -> Option<&'static mut [u8; N]> {
        if self.taken.swap(true, Ordering::AcqRel) {
            return None;
        }
        // SAFETY: the flag was clear, so no one else has the bytes, and no one will.
        Some(unsafe { &mut *self.bytes.get() })
    }
}
