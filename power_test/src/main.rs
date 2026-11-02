//! Restart and power-off test. Prints what the last boot left in the registers U-Boot reads,
//! then waits for a request from the UART or the keys. Whether each one works shows in what the
//! board does next, so run it once for each.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println};

exynos_4412_rs::entry!(main);

fn report() {
    println!(
        "INFORM5 {:#x}, INFORM6 {:#x}, PS_HOLD_CONTROL {:#010x}",
        power::inform5(),
        power::inform6(),
        power::ps_hold_control(),
    );
    let (dc, usb) = power::charger_connected();
    println!("Charger: DC {}, USB {}", if dc { "yes" } else { "no" }, if usb { "yes" } else { "no" });
}

fn help() {
    println!("UART: r restart, f restart into fastboot, o power off, s state");
    println!("Keys, held 2 s then released: power = off, power + vol+ = restart, power + vol- = fastboot");
}

fn main(_args: BootArgs) -> ! {
    println!("\nRestart and power-off test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    report();
    help();

    let uart = console::uart();
    let mut keys = PowerKeys::new();
    loop {
        keys.service(mct);
        match uart.try_read_byte() {
            Some(b'r') => {
                println!("Restart");
                power::restart(BootMode::Normal);
            }
            Some(b'f') => {
                println!("Restart into fastboot");
                power::restart(BootMode::Fastboot);
            }
            Some(b'o') => {
                println!("Power off, the vendor kernel's way");
                power::power_off();
            }
            Some(b's') => report(),
            Some(b'\r' | b'\n') | None => {}
            Some(_) => help(),
        }
    }
}
