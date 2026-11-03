//! Async UART receiving, on the receive interrupt, and the buffered console, on the transmit
//! interrupt of the console UART: see [`buffer_console`].
//!
//! The interrupt handler moves every byte from the UART's FIFO into a ring of [`RING_BYTES`]
//! and wakes the reader. The FIFO of UART2, the console, holds few bytes, which last about a
//! millisecond at 115200 baud; the ring holds them while the reading task is busy, and a
//! pasted block of text. A byte
//! that finds the ring full is dropped and counted.
//!
//! Sending goes through `print!`. Once [`buffer_console`] has run, it returns as soon as the
//! text is in the console's ring (see [`console`](crate::console)), and [`flush_console`]
//! awaits the ring's end.

use core::cell::RefCell;
use core::future::poll_fn;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::Poll;

use critical_section::Mutex;
use embassy_sync::waitqueue::AtomicWaker;

use crate::console;
use crate::drivers::gic::Gic;
use crate::drivers::uart::{Uart, UartId};

/// Bytes the ring of each UART holds.
pub const RING_BYTES: usize = 2048;
/// UART0 to UART3: UART4 has no interrupt in the device tree.
const UARTS: usize = 4;

struct Ring {
    bytes: [u8; RING_BYTES],
    /// Where the oldest byte is.
    head: usize,
    len: usize,
}

impl Ring {
    const fn new() -> Self {
        Self { bytes: [0; RING_BYTES], head: 0, len: 0 }
    }

    fn push(&mut self, byte: u8) -> bool {
        if self.len == RING_BYTES {
            return false;
        }
        self.bytes[(self.head + self.len) % RING_BYTES] = byte;
        self.len += 1;
        true
    }

    fn pop_into(&mut self, buffer: &mut [u8]) -> usize {
        let count = self.len.min(buffer.len());
        for target in &mut buffer[..count] {
            *target = self.bytes[self.head];
            self.head = (self.head + 1) % RING_BYTES;
        }
        self.len -= count;
        count
    }
}

struct RxState {
    ring: Mutex<RefCell<Ring>>,
    waker: AtomicWaker,
    dropped: AtomicU32,
    /// `UERSTAT` bits seen, ORed together.
    errors: AtomicU32,
    /// Bytes the interrupt handler moved, and how many times it ran.
    received: AtomicU32,
    interrupts: AtomicU32,
    /// A [`UartRx`] has turned on receiving: only then does the handler empty the FIFO.
    enabled: AtomicBool,
}

/// Woken when the console's ring has been sent.
static TX_EMPTY: AtomicWaker = AtomicWaker::new();

/// Makes the console buffered, with its transmit interrupt to CPU0: see
/// [`console`](crate::console). Call it after [`Gic::init`], with the MMU on. A
/// [`UartRx`] on the console UART may come before or after.
///
/// # Panics
///
/// If the console is on UART4, which has no interrupt.
pub fn buffer_console() {
    let uart = console::uart();
    let irq = uart.id().irq().expect("the console UART has an interrupt");
    let index = uart.id() as usize;
    // Polled output before this may still be in the FIFO: its interrupt starts masked.
    if !RX[index].enabled.load(Ordering::Acquire) {
        uart.mask_interrupts();
    } else {
        uart.set_tx_interrupt(false);
    }
    console::enable_buffering();
    let gic = Gic::new();
    gic.set_target(irq, 0);
    gic.enable(irq);
}

/// Waits until the console's ring has gone into the UART's FIFO: what was printed before is
/// then out, but for the FIFO's last bytes. At once if the console is not buffered.
pub async fn flush_console() {
    poll_fn(|cx| {
        if console::held() == 0 {
            return Poll::Ready(());
        }
        TX_EMPTY.register(cx.waker());
        // Emptied since the look above: the interrupt has woken nobody.
        if console::held() == 0 { Poll::Ready(()) } else { Poll::Pending }
    })
    .await;
}

static RX: [RxState; UARTS] = [const {
    RxState {
        ring: Mutex::new(RefCell::new(Ring::new())),
        waker: AtomicWaker::new(),
        dropped: AtomicU32::new(0),
        errors: AtomicU32::new(0),
        received: AtomicU32::new(0),
        interrupts: AtomicU32::new(0),
        enabled: AtomicBool::new(false),
    }
}; UARTS];

/// What the receive side of one UART has counted since it was enabled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxStats {
    /// Bytes moved from the FIFO.
    pub received: u32,
    /// Bytes dropped on a full ring.
    pub dropped: u32,
    /// Interrupts handled.
    pub interrupts: u32,
    /// `UERSTAT` bits seen: 0 overrun, 1 parity, 2 frame, 3 break.
    pub errors: u32,
}

/// The receiving side of one UART, read with `await`.
#[derive(Clone, Copy, Debug)]
pub struct UartRx {
    id: UartId,
    index: usize,
}

impl UartRx {
    /// Turns on receiving by interrupt for `id`, with the interrupt to CPU0. `None` for UART4.
    /// Call it after [`Gic::init`], with the MMU on. Bytes already in the FIFO stay there for
    /// the first interrupt.
    pub fn new(id: UartId) -> Option<Self> {
        let irq = id.irq()?;
        let gic = Gic::new();
        RX[id as usize].enabled.store(true, Ordering::Release);
        gic.set_target(irq, 0);
        gic.enable(irq);
        Uart::new(id).enable_rx_interrupt();
        // That masked the transmit interrupt, which the buffered console may need.
        console::resume_tx();
        Some(Self { id, index: id as usize })
    }

    /// The UART this reads.
    pub const fn id(&self) -> UartId {
        self.id
    }

    /// Waits for at least one byte, then takes as many as are there, up to `buffer`'s length.
    /// Returns how many. An empty `buffer` returns 0 at once.
    pub async fn read(&self, buffer: &mut [u8]) -> usize {
        if buffer.is_empty() {
            return 0;
        }
        let state = &RX[self.index];
        poll_fn(|cx| {
            critical_section::with(|cs| {
                let count = state.ring.borrow_ref_mut(cs).pop_into(buffer);
                if count > 0 {
                    Poll::Ready(count)
                } else {
                    // Inside the same section as the check: the handler cannot fill the ring
                    // between them.
                    state.waker.register(cx.waker());
                    Poll::Pending
                }
            })
        })
        .await
    }

    /// Waits for one byte.
    pub async fn read_byte(&self) -> u8 {
        let mut byte = [0];
        self.read(&mut byte).await;
        byte[0]
    }

    /// The counts so far.
    pub fn stats(&self) -> RxStats {
        let state = &RX[self.index];
        RxStats {
            received: state.received.load(Ordering::Relaxed),
            dropped: state.dropped.load(Ordering::Relaxed),
            interrupts: state.interrupts.load(Ordering::Relaxed),
            errors: state.errors.load(Ordering::Relaxed),
        }
    }
}

/// Handles the interrupt of a UART, if `id` is one: see [`super::on_interrupt`].
pub(super) fn on_interrupt(id: u16) -> bool {
    let Some(index) = (0..UARTS).find(|index| UartId::from_index(*index as u8).and_then(UartId::irq) == Some(id))
    else {
        return false;
    };
    let Some(uart_id) = UartId::from_index(index as u8) else {
        return false;
    };
    let uart = Uart::new(uart_id);
    if uart.tx_interrupt_pending() && console::service_tx(uart) {
        TX_EMPTY.wake();
    }
    let state = &RX[index];
    if !state.enabled.load(Ordering::Acquire) {
        return true;
    }
    uart.clear_rx_interrupt();
    let errors = uart.take_errors();
    if errors != 0 {
        state.errors.fetch_or(errors, Ordering::Relaxed);
    }
    let (mut received, mut dropped) = (0, 0);
    critical_section::with(|cs| {
        let mut ring = state.ring.borrow_ref_mut(cs);
        while let Some(byte) = uart.try_read_byte() {
            if ring.push(byte) {
                received += 1;
            } else {
                dropped += 1;
            }
        }
    });
    state.received.fetch_add(received, Ordering::Relaxed);
    state.dropped.fetch_add(dropped, Ordering::Relaxed);
    state.interrupts.fetch_add(1, Ordering::Relaxed);
    if received > 0 {
        state.waker.wake();
    }
    true
}
