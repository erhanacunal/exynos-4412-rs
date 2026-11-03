//! Exception handling.
//!
//! The stubs in `startup.s` build an [`ExceptionFrame`] on the stack of the exception
//! mode and call a handler symbol with it. When the handler returns, the registers
//! are restored from the frame and execution resumes at
//! [`return_addr`](ExceptionFrame::return_addr).
//!
//! # Overriding a handler
//!
//! Each handler is a linker symbol with a default (see `link.x`, the `PROVIDE` lines).
//! The defaults report on the console and halt. To replace one, an application
//! defines the symbol, with this exact signature:
//!
//! ```ignore
//! #[unsafe(no_mangle)]
//! extern "C" fn handle_irq(frame: &mut exynos_4412_rs::exceptions::ExceptionFrame) {
//!     // ...
//! }
//! ```
//!
//! The symbols are `handle_undef`, `handle_svc`, `handle_prefetch_abort`,
//! `handle_data_abort`, `handle_irq` and `handle_fiq`.
//!
//! The application runs in SVC mode, so an `svc` it issues overwrites its own `LR`:
//! any inline `svc` must declare `lr` as clobbered.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::cpu;
use crate::println;

global_asm!(include_str!("startup.s"), options(raw));

/// Registers saved on exception entry, lowest address first.
///
/// Holds what a handler written in Rust could clobber, plus r4-r12 for diagnostics.
/// `d8-d15` are callee-saved under the AAPCS and stay in the registers. The target's
/// code generation does use `d16-d31`, so they are saved as well. The banked
/// `SP` and `LR` of the interrupted mode are not part of the frame.
#[repr(C)]
#[derive(Debug)]
pub struct ExceptionFrame {
    /// `d0-d7` as pairs of words: the frame is only guaranteed to be 4-byte aligned.
    pub d_low: [u32; 16],
    /// `d16-d31` as pairs of words.
    pub d_high: [u32; 32],
    /// Floating-point status and control register.
    pub fpscr: u32,
    /// `r0-r12`.
    pub r: [u32; 13],
    _lr_copy: u32,
    /// Where execution resumes when the handler returns. See [`ExceptionKind::fault_pc`].
    pub return_addr: u32,
    /// CPSR of the interrupted code, restored on return.
    pub spsr: u32,
}

const _: () = assert!(size_of::<ExceptionFrame>() == 65 * 4);

/// The exception types with a handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionKind {
    /// Undefined instruction.
    Undefined,
    /// Supervisor call.
    Supervisor,
    /// Prefetch abort.
    PrefetchAbort,
    /// Data abort.
    DataAbort,
    /// IRQ.
    Irq,
    /// FIQ.
    Fiq,
}

impl ExceptionKind {
    /// The value `startup.s` subtracts from the banked `LR` to get `return_addr`.
    pub const fn lr_adjustment(self) -> u32 {
        match self {
            Self::Undefined | Self::Supervisor => 0,
            Self::PrefetchAbort | Self::Irq | Self::Fiq => 4,
            Self::DataAbort => 8,
        }
    }

    /// Address of the instruction that raised the exception, given `return_addr`.
    ///
    /// Exact for aborts. For undefined instructions and SVC it assumes ARM state
    /// (`return_addr - 4`), and it is the resume address for IRQ and FIQ, which
    /// no instruction raises.
    pub const fn fault_pc(self, return_addr: u32) -> u32 {
        match self {
            Self::Undefined | Self::Supervisor => return_addr.wrapping_sub(4),
            Self::PrefetchAbort | Self::DataAbort | Self::Irq | Self::Fiq => return_addr,
        }
    }
}

// Set by the first report: a fault while reporting would otherwise recurse.
// Plain load and store, see the `console` module documentation.
static REPORTING: AtomicBool = AtomicBool::new(false);

fn report_and_halt(kind: ExceptionKind, frame: &ExceptionFrame) -> ! {
    if REPORTING.load(Ordering::Relaxed) {
        cpu::halt();
    }
    REPORTING.store(true, Ordering::Relaxed);

    println!("\n*** CPU{} unhandled exception: {:?}", cpu::id(), kind);
    println!(
        "LR  {:#010x}  (fault PC {:#010x})",
        frame.return_addr.wrapping_add(kind.lr_adjustment()),
        kind.fault_pc(frame.return_addr),
    );
    println!(
        "SPSR {:#010x}  (from {:?} mode)",
        frame.spsr,
        cpu::Mode::from_psr(frame.spsr),
    );
    match kind {
        ExceptionKind::DataAbort => {
            println!("DFSR {:#010x}", cpu::read_dfsr());
            println!("DFAR {:#010x}", cpu::read_dfar());
        }
        ExceptionKind::PrefetchAbort => {
            println!("IFSR {:#010x}", cpu::read_ifsr());
            println!("IFAR {:#010x}", cpu::read_ifar());
        }
        ExceptionKind::Undefined
        | ExceptionKind::Supervisor
        | ExceptionKind::Irq
        | ExceptionKind::Fiq => {}
    }
    for (index, value) in frame.r.iter().enumerate() {
        println!("r{:<2} {:#010x}", index, value);
    }
    println!("Halted.");
    // A buffered console would keep the report in its ring: see `console`.
    crate::console::flush();
    cpu::halt();
}

/// Defines the Rust default for one handler and the assembly symbol `link.x` binds to it.
macro_rules! default_handler {
    ($rust:ident, $symbol:literal, $kind:expr) => {
        extern "C" fn $rust(frame: &mut ExceptionFrame) {
            report_and_halt($kind, frame);
        }

        // A `sym` operand keeps the Rust function alive through LTO, which an
        // unreferenced `#[no_mangle]` function in a dependency is not guaranteed.
        global_asm!(
            ".pushsection .text.exceptions, \"ax\", %progbits",
            concat!(".global ", $symbol),
            concat!(".type ", $symbol, ", %function"),
            concat!($symbol, ":"),
            "b {handler}",
            concat!(".size ", $symbol, ", . - ", $symbol),
            ".popsection",
            handler = sym $rust,
        );
    };
}

default_handler!(default_undef, "__default_handle_undef", ExceptionKind::Undefined);
default_handler!(default_svc, "__default_handle_svc", ExceptionKind::Supervisor);
default_handler!(
    default_prefetch_abort,
    "__default_handle_prefetch_abort",
    ExceptionKind::PrefetchAbort
);
default_handler!(default_data_abort, "__default_handle_data_abort", ExceptionKind::DataAbort);
default_handler!(default_irq, "__default_handle_irq", ExceptionKind::Irq);
default_handler!(default_fiq, "__default_handle_fiq", ExceptionKind::Fiq);
