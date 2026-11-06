//! MAX8903 charger test. Prints the charger's pins as U-Boot left them, then once more each time
//! one changes, polled every 100 ms: plug and unplug the DC and USB cables to see `dc` and `usb`
//! follow. UART commands enable, disable and restart charging.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max8903::{Max8903, Status};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

const POLL_MS: u64 = 100;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn report(mct: Mct, status: Status) {
    let enabled = match status.enabled {
        Some(true) => "on",
        Some(false) => "off",
        None => "pin not output",
    };
    println!(
        "{:>8} ms: DC {}, USB {}, charging {}, fault {}, enable {}: {:?}",
        mct.count() / COUNTS_PER_MS,
        yes(status.dc),
        yes(status.usb),
        yes(status.charging),
        yes(status.fault),
        enabled,
        status.state(),
    );
}

fn help() {
    println!("UART: e enable charging, d disable, r restart (clears a fault), s state");
}

fn main(_args: BootArgs) -> ! {
    println!("\nMAX8903 charger test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let charger = Max8903::new();
    let mut last = charger.status();
    report(mct, last);
    help();

    let uart = console::uart();
    let mut next = mct.count();
    loop {
        match uart.try_read_byte() {
            Some(b'e') => {
                println!("Enable charging");
                charger.set_charging(true);
            }
            Some(b'd') => {
                println!("Disable charging");
                charger.set_charging(false);
            }
            Some(b'r') => {
                println!("Restart charging");
                charger.restart_charging(mct);
            }
            Some(b's') => report(mct, charger.status()),
            Some(b'\r' | b'\n') | None => {}
            Some(_) => help(),
        }
        if mct.count() >= next {
            next += POLL_MS * COUNTS_PER_MS;
            let status = charger.status();
            if status != last {
                report(mct, status);
                last = status;
            }
        }
    }
}
