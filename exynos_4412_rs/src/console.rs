//! Global text console behind [`print!`](crate::print) and [`println!`](crate::println).
//!
//! Defaults to [`soc::DEFAULT_CONSOLE_UART`]; [`set_uart`] selects another one.
//!
//! # Locking
//!
//! Until [`smp`](crate::smp) starts a second core there is no lock and no exclusive
//! access, because with the MMU off all memory is Strongly-ordered and `LDREX`/`STREX`
//! cannot be relied on there. Output from an exception handler can then land between the
//! bytes of an interrupted `print!`, which garbles text but is otherwise harmless.
//!
//! Afterwards every `print!` is atomic against other cores, and IRQs stay masked on the
//! printing core. A core that faults while printing still gets its report out, because
//! the lock is re-entrant, but it then halts owning the lock and blocks all output.

use core::fmt::{self, Write};
use core::hint;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use crate::drivers::uart::{Uart, UartId};
use crate::{cpu, soc};

// Plain load and store only: see the module documentation.
static CONSOLE_UART: AtomicU8 = AtomicU8::new(soc::DEFAULT_CONSOLE_UART as u8);

static LOCKING: AtomicBool = AtomicBool::new(false);

/// Value of [`OWNER`] when free. A core stores its number plus one.
const NO_OWNER: u32 = 0;
static OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

/// Turns on the lock for good. Requires the MMU on, from this core on.
pub(crate) fn enable_locking() {
    LOCKING.store(true, Ordering::Release);
}

/// The core holding the print lock, for diagnostics: `None` if it is free.
pub fn lock_owner() -> Option<usize> {
    match OWNER.load(Ordering::Relaxed) {
        NO_OWNER => None,
        owner => Some(owner as usize - 1),
    }
}

/// Selects the UART used by the console from now on.
pub fn set_uart(id: UartId) {
    CONSOLE_UART.store(id as u8, Ordering::Relaxed);
}

/// Returns the UART the console currently writes to.
pub fn uart() -> Uart {
    let index = CONSOLE_UART.load(Ordering::Relaxed);
    Uart::new(UartId::from_index(index).unwrap_or(soc::DEFAULT_CONSOLE_UART))
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    if !LOCKING.load(Ordering::Acquire) {
        // `Uart::write_str` never fails.
        let _ = uart().write_fmt(args);
        return;
    }

    let irq_was_masked = cpu::mask_irq();
    let me = cpu::id() as u32 + 1;
    let reentered = OWNER.load(Ordering::Relaxed) == me;
    if !reentered {
        while OWNER
            .compare_exchange_weak(NO_OWNER, me, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            hint::spin_loop();
        }
    }

    let _ = uart().write_fmt(args);

    if !reentered {
        OWNER.store(NO_OWNER, Ordering::Release);
    }
    cpu::restore_irq(irq_was_masked);
}

/// Prints to the console.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!($($arg)*))
    };
}

/// Prints to the console, followed by a newline.
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!("{}\n", ::core::format_args!($($arg)*)))
    };
}
