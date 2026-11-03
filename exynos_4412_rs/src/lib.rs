//! Bare-metal runtime for the Samsung Exynos 4412 (4 × Cortex-A9), started from U-Boot.
//!
//! The image starts with a Linux zImage header, so `bootm` can boot it as a kernel,
//! and it can also be started with `go <address>`. It moves itself to its link address
//! when the bootloader starts it above that address.
//!
//! An application depends on this crate, provides a `memory.x` and writes a `main`:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! use exynos_4412_rs::{BootArgs, println};
//!
//! exynos_4412_rs::entry!(main);
//!
//! fn main(args: BootArgs) -> ! {
//!     println!("r0 = {:#x}", args.r0);
//!     exynos_4412_rs::cpu::halt()
//! }
//! ```
//!
//! # Environment `main` runs in
//!
//! CPU0 only, SVC mode, IRQ and FIQ masked, MMU and caches off, `.bss` zeroed, VFP
//! enabled. With the MMU off every data access is Strongly-ordered, hence uncached and
//! slow, and an unaligned access raises a data abort (`SCTLR.A` is set). [`mmu::enable`]
//! turns on the MMU and L1 caches, and [`smp::start_cpu`] starts the other cores.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![warn(missing_docs)]

pub mod console;
pub mod cpu;
pub mod crypto;
pub mod drivers;
#[cfg(feature = "embassy")]
pub mod embassy;
pub mod exceptions;
#[cfg(feature = "fat")]
pub mod fat;
pub mod firmware;
pub mod mmu;
pub mod nvram;
pub mod partition;
#[cfg(feature = "panic-handler")]
mod panic;
pub mod soc;
pub mod time;
pub mod touch;
pub mod wav;

pub mod smp;
pub mod wpa;

/// State of the CPU when the bootloader jumped to `_start`.
///
/// The meaning of `r0`-`r2` depends on how the image was started:
///
/// - `bootm`: `r1` = machine type and `r2` = address of the ATAGS, as in the Linux ARM
///   boot protocol. On the board `r0` is not 0, despite that protocol: do not rely on it.
///   Observed values: `r0` = 0x43E00898, `r1` = 0x10B4, `r2` = 0x40000100.
/// - `go` calls the image as `entry(argc, argv)`, so `r0` counts the arguments after
///   the address, `r1` points to their array in U-Boot's memory, and `r2` is unspecified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootArgs {
    /// Value of `r0`.
    pub r0: u32,
    /// Value of `r1`.
    pub r1: u32,
    /// Value of `r2`.
    pub r2: u32,
    /// Address the image was started from, before any self-relocation. `main` always
    /// runs at the link address.
    pub load_addr: u32,
}

/// Declares the application's entry point.
///
/// The function must have the signature `fn(BootArgs) -> !`; anything else fails
/// to compile. Omitting `entry!` fails to link, on the symbol `__exynos_main`.
///
/// Before `main`, it turns the panel's backlight and LVDS transmitter off
/// ([`drivers::display::panel_off`]), which U-Boot leaves on: an application that uses the
/// display turns them on again.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        const _: () = {
            #[unsafe(no_mangle)]
            extern "C" fn __exynos_main(r0: u32, r1: u32, r2: u32, load_addr: u32) -> ! {
                let main: fn($crate::BootArgs) -> ! = $main;
                $crate::drivers::display::panel_off();
                main($crate::BootArgs { r0, r1, r2, load_addr })
            }
        };
    };
}
