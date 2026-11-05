// SPDX-License-Identifier: GPL-2.0-or-later
//! MFC v5.1 bring-up, through `exynos_4412_mfc`: the codec's power domain and clocks as U-Boot
//! left them, then on, its firmware copied to port A and started, `SYS_INIT`, and the firmware's
//! version. Nothing is decoded yet.
//!
//! The firmware comes from `MFC_FIRMWARE` at build time: see `build.rs`.
//!
//! Keys afterwards: `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use exynos_4412_mfc::{Banks, Mfc};
use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mfc;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println};

exynos_4412_rs::entry!(main);

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/s5p-mfc.fw"));

/// The two ports' memory, sized as Linux reserves it (`exynos-mfc-reserved-memory.dtsi`): 512 MiB
/// into DRAM, far above this program and uart_boot's stage at 0x50000000.
const BANKS: Banks = Banks { a: 0x6000_0000, a_len: 36 << 20, b: 0x6400_0000, b_len: 8 << 20 };

fn print_state(when: &str) {
    let clocks = Cmu::new().mfc_clocks();
    println!(
        "{}: domain {:#x}, SRC_MFC {:#x}, DIV_MFC {:#x}, GATE_IP_MFC {:#x}, sclk_mfc {:?} Hz",
        when,
        mfc::power_domain_status(),
        clocks.src,
        clocks.div,
        clocks.gate,
        Cmu::new().sclk_mfc_hz()
    );
}

fn wait_keys() -> ! {
    println!("Keys: r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    loop {
        match uart.read_byte() {
            b'r' => power::restart(BootMode::Normal),
            b'f' => power::restart(BootMode::Fastboot),
            _ => {}
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nMFC v5.1 bring-up");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    print_state("As U-Boot left it");
    if FIRMWARE.is_empty() {
        println!("Built without MFC_FIRMWARE: stopping (MISMATCH)");
        wait_keys();
    }
    println!("Firmware {} bytes; port A {:#x}, port B {:#x}", FIRMWARE.len(), BANKS.a, BANKS.b);
    console::flush();
    match Mfc::init(BANKS, FIRMWARE, mfc::VENDOR_HZ, mct) {
        Ok((_mfc, started)) => {
            let (year, month, day) = started.version.date();
            println!(
                "Firmware running: version {:#08x} (20{:02x}-{:02x}-{:02x}); sclk_mfc {} Hz",
                started.version.0, year, month, day, started.hz
            );
            println!("  start {} us, SYS_INIT {} us: OK", started.start_us, started.init_us);
        }
        Err(error) => println!("MFC ERROR {:?} (MISMATCH)", error),
    }
    print_state("After");
    wait_keys();
}
