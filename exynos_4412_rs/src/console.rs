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
//!
//! # Buffered output
//!
//! Each byte takes 87 us at 115200 baud, so a polled line of 100 characters holds its core
//! for almost 9 ms. With the `embassy` feature, `embassy::uart::buffer_console` makes the
//! console buffered: `print!` puts what does not fit in the UART's FIFO into a ring of
//! [`TX_RING_BYTES`] and returns, and the UART's transmit interrupt, on CPU0, moves the ring
//! into the FIFO as it empties. Output keeps its order.
//!
//! - **Full ring:** the printing core sends the oldest bytes itself, polled, until the rest
//!   fits. So a print never loses anything, and still works with IRQs masked, but then takes
//!   as long as a polled one.
//! - **Halting:** bytes still in the ring are lost if nothing sends them. [`flush`] sends them
//!   polled; the panic handler, the default exception handlers, `power::restart` and
//!   `power::power_off` call it.

use core::cell::UnsafeCell;
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

/// IRQs masked on this core and the print lock held, until it is dropped.
struct Locked {
    irq_was_masked: bool,
    reentered: bool,
}

impl Locked {
    fn take() -> Self {
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
        Self { irq_was_masked, reentered }
    }

    /// The ring: only the holder of the lock reaches it.
    fn ring(&mut self) -> &mut Ring {
        // SAFETY: the print lock is held and IRQs are masked on this core, so nothing else
        // reaches the ring until `self` is dropped, and `&mut self` keeps this the only
        // reference meanwhile. A re-entered `Locked` belongs to an exception taken on this
        // core while it printed; it only ever adds bytes after the ones there.
        unsafe { &mut *RING.0.get() }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        if !self.reentered {
            OWNER.store(NO_OWNER, Ordering::Release);
        }
        cpu::restore_irq(self.irq_was_masked);
    }
}

/// Bytes the buffered console holds for the transmit interrupt.
pub const TX_RING_BYTES: usize = 16 * 1024;

struct Ring {
    bytes: [u8; TX_RING_BYTES],
    /// Where the oldest byte is.
    head: usize,
    len: usize,
}

impl Ring {
    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.bytes[self.head];
        self.head = (self.head + 1) % TX_RING_BYTES;
        self.len -= 1;
        Some(byte)
    }

    fn push(&mut self, byte: u8) {
        self.bytes[(self.head + self.len) % TX_RING_BYTES] = byte;
        self.len += 1;
    }

    /// Moves bytes into the FIFO while it has room.
    fn send(&mut self, uart: Uart) {
        while self.len > 0 && uart.try_write_byte(self.bytes[self.head]) {
            self.pop();
        }
    }
}

struct RingCell(UnsafeCell<Ring>);

// SAFETY: reached only through `Locked::ring`.
unsafe impl Sync for RingCell {}

static RING: RingCell = RingCell(UnsafeCell::new(Ring { bytes: [0; TX_RING_BYTES], head: 0, len: 0 }));
static BUFFERED: AtomicBool = AtomicBool::new(false);

static BUFFERED_BYTES: AtomicU32 = AtomicU32::new(0);
static POLLED_BYTES: AtomicU32 = AtomicU32::new(0);
static TX_INTERRUPTS: AtomicU32 = AtomicU32::new(0);
static MOST_HELD: AtomicU32 = AtomicU32::new(0);

/// What the buffered console has counted since it was turned on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxStats {
    /// Bytes that went through the ring.
    pub buffered: u32,
    /// Bytes a printing core sent itself, polled, because the ring was full.
    pub polled: u32,
    /// Transmit interrupts handled.
    pub interrupts: u32,
    /// The most the ring held at once.
    pub most_held: u32,
    /// What it holds now.
    pub held: u32,
}

/// The counts of the buffered console.
pub fn tx_stats() -> TxStats {
    let held = if BUFFERED.load(Ordering::Acquire) { Locked::take().ring().len as u32 } else { 0 };
    TxStats {
        buffered: BUFFERED_BYTES.load(Ordering::Relaxed),
        polled: POLLED_BYTES.load(Ordering::Relaxed),
        interrupts: TX_INTERRUPTS.load(Ordering::Relaxed),
        most_held: MOST_HELD.load(Ordering::Relaxed),
        held,
    }
}

/// Whether the console is buffered.
pub fn is_buffered() -> bool {
    BUFFERED.load(Ordering::Acquire)
}

/// Makes the console buffered: see the [module documentation](self). The caller has masked
/// the console UART's interrupts and routes its interrupt to CPU0, where its handler calls
/// [`service_tx`]. Requires the MMU on.
pub(crate) fn enable_buffering() {
    enable_locking();
    BUFFERED.store(true, Ordering::Release);
}

/// Unmasks the console's transmit interrupt again if the ring holds bytes, after something
/// masked the UART's interrupts.
pub(crate) fn resume_tx() {
    if BUFFERED.load(Ordering::Acquire) {
        let mut locked = Locked::take();
        if locked.ring().len > 0 {
            uart().set_tx_interrupt(true);
        }
    }
}

/// Handles the transmit interrupt of `uart`: fills its FIFO from the ring, and masks the
/// interrupt once the ring is empty. Whether the ring is empty now.
pub(crate) fn service_tx(uart: Uart) -> bool {
    if !BUFFERED.load(Ordering::Acquire) || uart.id() != self::uart().id() {
        uart.set_tx_interrupt(false);
        uart.clear_tx_interrupt();
        return true;
    }
    TX_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    let mut locked = Locked::take();
    let ring = locked.ring();
    ring.send(uart);
    let empty = ring.len == 0;
    if empty {
        uart.set_tx_interrupt(false);
    }
    uart.clear_tx_interrupt();
    empty
}

/// Bytes the ring holds now, 0 if the console is not buffered.
pub fn held() -> usize {
    if BUFFERED.load(Ordering::Acquire) { Locked::take().ring().len } else { 0 }
}

/// Sends what the ring holds, polled, then waits for the UART to finish. On a console that
/// is not buffered, only the wait.
pub fn flush() {
    let uart = uart();
    if BUFFERED.load(Ordering::Acquire) {
        let mut locked = Locked::take();
        let ring = locked.ring();
        while let Some(byte) = ring.pop() {
            uart.write_byte(byte);
        }
        uart.set_tx_interrupt(false);
    }
    uart.flush();
}

/// Writes through the ring: see [`Ring`].
struct Buffered<'a> {
    uart: Uart,
    ring: &'a mut Ring,
    count: u32,
}

impl Buffered<'_> {
    fn put(&mut self, byte: u8) {
        // Nothing waits before it: straight into the FIFO if it has room.
        if self.ring.len == 0 && self.uart.try_write_byte(byte) {
            return;
        }
        if self.ring.len == TX_RING_BYTES {
            // The interrupt may be on another core's hands, or masked: send the oldest here.
            self.ring.send(self.uart);
            while self.ring.len == TX_RING_BYTES {
                if let Some(oldest) = self.ring.pop() {
                    self.uart.write_byte(oldest);
                    POLLED_BYTES.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.ring.push(byte);
        self.count += 1;
    }
}

impl Write for Buffered<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.put(b'\r');
            }
            self.put(byte);
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    if !LOCKING.load(Ordering::Acquire) {
        // `Uart::write_str` never fails.
        let _ = uart().write_fmt(args);
        return;
    }

    let mut locked = Locked::take();
    if BUFFERED.load(Ordering::Acquire) {
        let uart = uart();
        let mut writer = Buffered { uart, ring: locked.ring(), count: 0 };
        let _ = writer.write_fmt(args);
        let (count, held) = (writer.count, writer.ring.len);
        BUFFERED_BYTES.fetch_add(count, Ordering::Relaxed);
        MOST_HELD.fetch_max(held as u32, Ordering::Relaxed);
        if held > 0 {
            uart.set_tx_interrupt(true);
        }
    } else {
        let _ = uart().write_fmt(args);
    }
    drop(locked);
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
