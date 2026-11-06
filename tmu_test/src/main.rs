//! TMU test. Prints the TMU's registers as U-Boot left them and its supply, turns the sensor on,
//! then prints the chip's temperature every second. UART commands change the CPU clock, so that
//! the temperature can be seen to follow the load: CPU0 polls the UART all the time.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::tmu::{Registers, Tmu};
use exynos_4412_rs::{BootArgs, console, cpu, cpufreq, mmu, println, soc};

exynos_4412_rs::entry!(main);

const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;
/// `vtmu-supply`: `exynos4412-mehmet.dts:1578`.
const LDO_TMU: u8 = 10;

fn report_registers(title: &str) {
    let r = Registers::read();
    println!(
        "{}: gate {}, TRIMINFO {:#010x}, CON2 {:#x}, CONTROL {:#010x}, STATUS {:#x}, CURRENT_TEMP {:#x}, INTEN {:#x}",
        title,
        if r.gate_open { "open" } else { "closed" },
        r.triminfo,
        r.triminfo_con2,
        r.control,
        r.status,
        r.current_temp,
        r.inten,
    );
    if r.gate_open {
        println!("  sensor {}, hardware trip {}", if r.enabled() { "on" } else { "off" }, if r.trip_enabled() { "on" } else { "off" });
    }
}

fn set_mhz(pmic: Max77686, mct: Mct, mhz: u32) {
    match cpufreq::set_arm_mhz(pmic, mct, mhz) {
        Ok(_) => println!("CPU {} MHz", cpufreq::arm_hz() / 1_000_000),
        Err(error) => println!("CPU clock ERROR {:?}", error),
    }
}

fn help() {
    println!("UART: f 1400 MHz, m 800 MHz, l 200 MHz, r registers");
}

fn main(_args: BootArgs) -> ! {
    println!("\nTMU test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("I2C ERROR {:?}", error);
        cpu::halt();
    }
    let pmic = Max77686::new(i2c);
    match pmic.ldo(LDO_TMU) {
        Ok(ldo) => println!("LDO10 (vtmu): mode {}, {} mV", ldo.mode(), ldo.millivolts()),
        Err(error) => println!("LDO10 ERROR {:?}", error),
    }

    report_registers("As U-Boot left it");
    let tmu = match Tmu::new(mct) {
        Ok(tmu) => tmu,
        Err(error) => {
            println!("TMU ERROR {:?}", error);
            report_registers("After");
            cpu::halt();
        }
    };
    report_registers("On");
    let cal = tmu.calibration();
    println!("Calibration: 25 C code {} ({}), 85 C code {}", cal.code_25, if cal.fused { "fused" } else { "fallback" }, cal.code_85);
    match cal.reload_us {
        Some(us) => println!("Reload: request bit cleared after {} us", us),
        None => println!("Reload: request bit still set after the timeout"),
    }
    println!("CPU {} MHz", cpufreq::arm_hz() / 1_000_000);
    help();

    let uart = console::uart();
    let mut next = mct.count();
    loop {
        match uart.try_read_byte() {
            Some(b'f') => set_mhz(pmic, mct, 1400),
            Some(b'm') => set_mhz(pmic, mct, 800),
            Some(b'l') => set_mhz(pmic, mct, 200),
            Some(b'r') => report_registers("Now"),
            Some(b'\r' | b'\n') | None => {}
            Some(_) => help(),
        }
        let now = mct.count();
        if now >= next {
            next += 1000 * COUNTS_PER_MS;
            println!(
                "{:>6} s: {} C (code {}), CPU {} MHz",
                now / (1000 * COUNTS_PER_MS),
                tmu.celsius(),
                tmu.code(),
                cpufreq::arm_hz() / 1_000_000,
            );
        }
    }
}
