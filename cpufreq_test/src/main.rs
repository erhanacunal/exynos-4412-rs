//! CPU clock test: `cpufreq::set_arm_mhz` from U-Boot's rate up to 1.4 GHz and down again.
//!
//! 1. The CPU's clock registers and `vdd_arm` (BUCK2: its mode, ramp, eight DVS registers and
//!    the pins that choose one) as U-Boot left them. The rate is also measured: the core's
//!    cycle counter against the MCT, over 100 ms.
//! 2. A fixed integer workload, timed, its result kept as the reference.
//! 3. Each step of [`STEPS`]: the rate set, measured, the voltage read back, the workload timed
//!    and its result checked against the reference.
//! 4. [`SOAK_SECONDS`] at 1.4 GHz running the workload, every result checked.
//!
//! Keys afterwards: `u` and `d` 100 MHz up and down, `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use core::hint::black_box;

use exynos_4412_rs::drivers::cmu::{self, Cmu};
use exynos_4412_rs::drivers::gpio;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::{self, Max77686};
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, cpu, cpufreq, mmu, println, soc};

exynos_4412_rs::entry!(main);

/// Up in two steps, down to the lowest point through the MPLL's safe divider, up again.
const STEPS: [u32; 6] = [1200, 1400, 800, 200, 1000, 1400];
const SOAK_SECONDS: u64 = 10;
const WORK_ROUNDS: u32 = 4_000_000;
const TICKS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

fn wait_us(mct: Mct, us: u64) {
    let end = mct.count() + us * TICKS_PER_US;
    while mct.count() < end {}
}

/// The core clock, from the cycle counter over 100 ms of the MCT, in kHz.
fn measure_khz(mct: Mct) -> u64 {
    cpu::start_cycle_counter();
    let start = mct.count();
    let cycles0 = cpu::read_cycle_counter();
    wait_us(mct, 100_000);
    let cycles = cpu::read_cycle_counter().wrapping_sub(cycles0);
    let ticks = mct.count() - start;
    u64::from(cycles) * u64::from(soc::MCT_CLOCK_HZ) / 1000 / ticks
}

/// Integer work in registers: an xorshift and a multiply-accumulate per round.
fn work() -> u32 {
    let mut x: u32 = 0x1234_5678;
    let mut acc: u32 = 0;
    for _ in 0..black_box(WORK_ROUNDS) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        acc = acc.wrapping_mul(31).wrapping_add(x);
    }
    acc
}

/// Runs [`work`] and returns its result and how long it took, in microseconds.
fn timed_work(mct: Mct) -> (u32, u64) {
    let start = mct.count();
    let result = work();
    (result, (mct.count() - start) / TICKS_PER_US)
}

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

fn report_clocks(label: &str, mct: Mct) {
    let clocks = Cmu::new().arm_clocks();
    println!(
        "{}: APLL_CON0 {:#010x} SRC_CPU {:#010x} STAT {:#010x} DIV_CPU0 {:#010x} DIV_CPU1 {:#010x}",
        label, clocks.apll_con0, clocks.src, clocks.mux_stat, clocks.div0, clocks.div1
    );
    println!(
        "  fout_apll {} Hz, MPLL {} Hz, armclk {} Hz, measured {} kHz",
        clocks.fout_apll_hz(),
        clocks.mpll_hz,
        clocks.armclk_hz(),
        measure_khz(mct)
    );
}

/// Returns false if `vdd_arm` cannot be read or is off.
fn report_supply(pmic: Max77686) -> bool {
    let buck = match pmic.dvs_buck(cpufreq::ARM_BUCK) {
        Ok(buck) => buck,
        Err(error) => {
            println!("BUCK2 ERROR {:?}", error);
            return false;
        }
    };
    println!(
        "BUCK2 vdd_arm: CTRL1 {:#04x}, mode {} (3 on), ramp {} uV/us",
        buck.ctrl1,
        buck.mode(),
        buck.ramp_microvolts_per_us()
    );
    for half in 0..2 {
        let uv = |i: usize| buck.microvolts(half * 4 + i);
        println!("  DVS{}-{}: {} {} {} {} uV", half * 4 + 1, half * 4 + 4, uv(0), uv(1), uv(2), uv(3));
    }
    let mut pins = [b'?'; 3];
    for (number, text) in pins.iter_mut().enumerate() {
        let pin = gpio::GPL0.pin(number as u8);
        *text = match (pin.function(), pin.read()) {
            (gpio::Function::Output, true) => b'1',
            (gpio::Function::Output, false) => b'0',
            (gpio::Function::Input, true) => b'h',
            (gpio::Function::Input, false) => b'l',
            _ => b'x',
        };
    }
    let dvs = max77686::selected_dvs();
    println!(
        "  pins gpl0-0..2: {} (1/0 out, h/l in): DVS{}, {} uV",
        core::str::from_utf8(&pins).unwrap_or("?"),
        dvs + 1,
        buck.microvolts(dvs)
    );
    buck.mode() == 3
}

/// Sets `mhz` and checks it: rate, measured rate, voltage, workload. Returns whether all held.
fn step(pmic: Max77686, mct: Mct, mhz: u32, reference: u32) -> bool {
    let change = match cpufreq::set_arm_mhz(pmic, mct, mhz) {
        Ok(change) => change,
        Err(error) => {
            println!("{} MHz: ERROR {:?}", mhz, error);
            report_clocks("  now", mct);
            return false;
        }
    };
    let khz = measure_khz(mct);
    let microvolts = cpufreq::arm_microvolts(pmic);
    let (result, us) = timed_work(mct);
    let rate_ok = change.to_hz == mhz * 1_000_000 && khz.abs_diff(u64::from(mhz) * 1000) <= u64::from(mhz) * 10;
    let volt_ok = microvolts == Ok(change.to_microvolts);
    let work_ok = result == reference;
    println!(
        "{:>4} MHz: {} -> {} Hz, measured {} kHz {}; DVS{} {} -> {:?} uV {}; work {} us {}",
        mhz,
        change.from_hz,
        change.to_hz,
        khz,
        verdict(rate_ok),
        change.dvs + 1,
        change.from_microvolts,
        microvolts,
        verdict(volt_ok),
        us,
        verdict(work_ok)
    );
    rate_ok && volt_ok && work_ok
}

fn soak(mct: Mct, reference: u32) {
    let end = mct.count() + SOAK_SECONDS * u64::from(soc::MCT_CLOCK_HZ);
    let (mut rounds, mut bad) = (0u32, 0u32);
    while mct.count() < end {
        rounds += 1;
        if work() != reference {
            bad += 1;
        }
    }
    println!("Soak {} s at {} kHz: {} runs, {} wrong {}", SOAK_SECONDS, measure_khz(mct), rounds, bad, verdict(bad == 0));
}

fn keys(pmic: Max77686, mct: Mct, reference: u32) -> ! {
    println!("Keys: u/d 100 MHz up/down, r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    loop {
        let delta: i32 = match uart.try_read_byte() {
            Some(b'u') => 100,
            Some(b'd') => -100,
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => continue,
        };
        let now = (cpufreq::arm_hz() / 1_000_000) as i32;
        let lowest = cmu::ARM_POINTS[0].mhz as i32;
        let highest = cmu::ARM_POINTS[cmu::ARM_POINTS.len() - 1].mhz as i32;
        let target = (now + delta).clamp(lowest, highest) as u32;
        step(pmic, mct, target, reference);
        console::flush();
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nCPU clock test: APLL, CPU dividers and vdd_arm");
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

    report_clocks("As U-Boot left it", mct);
    let supply_ok = report_supply(pmic);
    let (reference, us) = timed_work(mct);
    println!("Work at start: {:#010x} in {} us", reference, us);
    if !supply_ok {
        println!("vdd_arm unknown or off: the clock stays (MISMATCH)");
        keys(pmic, mct, reference);
    }
    console::flush();

    let mut all = true;
    for mhz in STEPS {
        all &= step(pmic, mct, mhz, reference);
        console::flush();
        if !all {
            break;
        }
    }
    if all {
        soak(mct, reference);
    }
    report_clocks("Now", mct);
    report_supply(pmic);
    println!("Result: {}", verdict(all));
    keys(pmic, mct, reference);
}
