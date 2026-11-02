//! Console test: prints the boot state, echoes UART input, and exercises the panic
//! and data abort paths on demand.

#![no_std]
#![no_main]

use core::arch::asm;

use exynos_4412_rs::cpu::{self, Mode};
use exynos_4412_rs::{BootArgs, console, print, println};

exynos_4412_rs::entry!(main);

fn main(args: BootArgs) -> ! {
    println!("\nMerhaba Exynos 4412");
    println!("r0={:#010x} r1={:#010x} r2={:#010x}", args.r0, args.r1, args.r2);
    println!("Baslatma adresi: {:#010x}", args.load_addr);
    println!("MPIDR: {:#010x}", cpu::read_mpidr());
    let cpsr = cpu::read_cpsr();
    println!("CPSR:  {:#010x} ({:?})", cpsr, Mode::from_psr(cpsr));
    println!("p: panic, a: data abort, digerleri: echo");

    let uart = console::uart();
    loop {
        match uart.read_byte() {
            b'p' => panic!("kasitli panic ('p' tusu)"),
            b'a' => data_abort(),
            b'\r' => println!(),
            byte => print!("{}", byte as char),
        }
    }
}

/// Faults with an unaligned load. `SCTLR.A` is set, so this is a precise alignment
/// fault, unlike a bad address, whose external abort may stay pending in `CPSR.A`.
fn data_abort() {
    let word = 0u32;
    let unaligned = (&raw const word as usize) + 1;
    // SAFETY: the load only reads inside `word`, and it is meant to fault before that.
    unsafe {
        asm!("ldr {0}, [{0}]", inout(reg) unaligned => _, options(nostack, readonly));
    }
}
