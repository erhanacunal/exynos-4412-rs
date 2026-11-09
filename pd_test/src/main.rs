//! Power domain test. Prints every domain as U-Boot left it, turns each one on and off and times
//! it, checks the GPU still answers after its domain was cut, checks that LCD0 is not cut under
//! TV, and leaves every domain but LCD0 off. UART: `r` prints the domains again, `l` turns LCD0
//! off or on.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mali::{self, Mali};
use exynos_4412_rs::drivers::pd::{Domain, Error};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

fn report(title: &str) {
    println!("{}:", title);
    for domain in Domain::ALL {
        let state = if domain.is_on() {
            "on"
        } else if domain.is_off() {
            "off"
        } else {
            "partial"
        };
        println!("  {:<9} conf {:#x} status {:#x} {}", domain.name(), domain.config(), domain.status(), state);
    }
}

/// Runs `step` and prints how long it took, or what failed.
fn timed(mct: Mct, label: &str, domain: Domain, step: impl FnOnce() -> Result<(), Error>) -> bool {
    let begin = mct.count();
    let result = step();
    let us = (mct.count() - begin) / COUNTS_PER_US;
    match result {
        Ok(()) => {
            println!("  {:<9} {:<3} {:>5} us, status {:#x}", domain.name(), label, us, domain.status());
            true
        }
        Err(error) => {
            println!("  {:<9} {:<3} ERROR {:?}", domain.name(), label, error);
            false
        }
    }
}

/// Off, on and off again, or on and off for a domain that was off.
fn cycle(mct: Mct, domain: Domain) -> bool {
    let mut ok = true;
    if !domain.is_off() {
        ok &= timed(mct, "off", domain, || domain.power_off(mct));
    }
    ok &= timed(mct, "on", domain, || domain.power_on(mct));
    ok &= timed(mct, "off", domain, || domain.power_off(mct));
    ok
}

fn gpu_identity(mct: Mct, label: &str) -> bool {
    let gpu = Mali::new();
    if let Err(error) = gpu.power_on(mali::SAFE_HZ, mct) {
        println!("  GPU {}: power on ERROR {:?}", label, error);
        return false;
    }
    match gpu.identify() {
        Ok(id) => {
            println!("  GPU {}: GP {:#010x} {}, PP0 {:#010x}, L2 {:#010x}", label, id.gp, id.gp_name(), id.pp[0], id.l2_size);
            id.gp >> 16 == 0x0B07
        }
        Err(error) => {
            println!("  GPU {}: ERROR {:?}", label, error);
            false
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nPower domain test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    report("As U-Boot left them");

    println!("GPU across a cut of G3D:");
    let mut ok = gpu_identity(mct, "before");
    ok &= timed(mct, "off", Domain::G3d, || Domain::G3d.power_off(mct));
    let accessible = Mali::new().is_accessible();
    println!("  GPU accessible while off: {}", accessible);
    ok &= !accessible;
    ok &= gpu_identity(mct, "after");

    println!("Cycles:");
    for domain in Domain::ALL {
        if domain != Domain::Lcd0 {
            ok &= cycle(mct, domain);
        }
    }

    println!("TV under LCD0:");
    ok &= timed(mct, "on", Domain::Tv, || Domain::Tv.power_on(mct));
    match Domain::Lcd0.power_off(mct) {
        Err(Error::ChildOn(Domain::Tv)) => println!("  LCD0 off refused while TV is on: ok"),
        other => {
            println!("  LCD0 off with TV on: {:?}, FAILED", other);
            ok = false;
        }
    }
    ok &= timed(mct, "off", Domain::Tv, || Domain::Tv.power_off(mct));

    report("Left");
    println!("Result: {}", if ok { "PASS" } else { "FAIL" });
    println!("UART: r domains, l LCD0 off/on");

    let uart = console::uart();
    loop {
        match uart.try_read_byte() {
            Some(b'r') => report("Now"),
            Some(b'l') => {
                if Domain::Lcd0.is_on() {
                    timed(mct, "off", Domain::Lcd0, || Domain::Lcd0.power_off(mct));
                } else {
                    timed(mct, "on", Domain::Lcd0, || Domain::Lcd0.power_on(mct));
                }
            }
            _ => {}
        }
    }
}
