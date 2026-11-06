//! MAX17040 fuel gauge test. Prints the gauge's version and `CONFIG`, then every 5 s the
//! battery's voltage and charge beside the MAX8903 charger's state. Plug and unplug the charger to see the voltage follow.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max17040::{self, Max17040};
use exynos_4412_rs::drivers::max8903::Max8903;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, println, soc};

exynos_4412_rs::entry!(main);

const PERIOD_MS: u64 = 5000;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

fn report(mct: Mct, gauge: Max17040, charger: Max8903) {
    let seconds = mct.count() / (1000 * COUNTS_PER_MS);
    let status = charger.status();
    match gauge.reading() {
        Ok(r) => println!(
            "{:>6} s: {} mV, SOC {:#06x}: {}.{}%; charger {:?}",
            seconds,
            r.millivolts,
            r.raw_soc,
            r.permille() / 10,
            r.permille() % 10,
            status.state(),
        ),
        Err(error) => println!("{:>6} s: gauge ERROR {:?}; charger {:?}", seconds, error, status.state()),
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nMAX17040 fuel gauge test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let i2c = I2c::new(Bus::I2c3);
    if let Err(error) = i2c.init(max17040::BUS_HZ, max17040::BUS_SDA_DELAY_NS) {
        println!("I2C ERROR {:?}", error);
        cpu::halt();
    }
    let gauge = Max17040::new(i2c);
    match (gauge.version(), gauge.config()) {
        (Ok(version), Ok(config)) => println!("VERSION {:#06x}, CONFIG {:#06x}", version, config),
        (version, config) => println!("ERROR {:?} {:?}", version, config),
    }
    let charger = Max8903::new();
    println!("UART: s reading now");

    let uart = console::uart();
    let mut next = mct.count();
    loop {
        if uart.try_read_byte() == Some(b's') {
            report(mct, gauge, charger);
        }
        if mct.count() >= next {
            next += PERIOD_MS * COUNTS_PER_MS;
            report(mct, gauge, charger);
        }
    }
}
