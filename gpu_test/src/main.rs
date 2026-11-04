//! GPU bring-up: the Mali-400 MP4, as far as its registers. Nothing is rendered.
//!
//! 1. The supply: BUCK4 `vdd_g3d` on the PMIC, its mode and its eight voltage registers, and the
//!    SoC pins that choose among them.
//! 2. The power domain and the clocks as U-Boot left them.
//! 3. Domain on, gate open, `sclk_g3d` at 160 MHz from the MPLL, the lowest operating point;
//!    only if the supply is at least that point's 875 mV.
//! 4. What the GP, the four PPs and the L2 cache say they are, before and after the GPU's own
//!    power unit brings its blocks up.
//! 5. A soft reset of the GP and of each PP, its completion left raised on the block's interrupt
//!    line: the GIC must see it pending, then not once cleared. A reset of each MMU, after a
//!    check that it takes a table address. The L2 cache emptied.
//!
//! Keys afterwards: `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::gpio;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mali::{self, Identity, Mali};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println};

exynos_4412_rs::entry!(main);

/// BUCK4 is `vdd_g3d`: `exynos4412-mehmet.dts:1197-1198`.
const G3D_BUCK: u8 = 4;
/// `exynos4412-mehmet.dts:755-781`: `gpl0-0` high and the rest low select `BUCKxDVS2`, so
/// `gpl0-0` is the lowest bit of the register's number; taken here that `gpl0-1` and `gpl0-2`
/// are the next two. On the board U-Boot leaves all seven inputs, read low: `DVS1`.
/// `gpl0-0` to `gpl0-6`, the PMIC's DVS pins: `exynos4412-mehmet.dts:755-781`.
const DVS_PINS: u8 = 7;

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

fn wait_keys() -> ! {
    console::flush();
    let uart = console::uart();
    loop {
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => core::hint::spin_loop(),
        }
    }
}

/// Step 1. Returns the voltage of the register the pins choose, if it could be read.
fn supply() -> Option<u32> {
    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("I2C ERROR {:?}", error);
        return None;
    }
    let buck = match Max77686::new(i2c).dvs_buck(G3D_BUCK) {
        Ok(buck) => buck,
        Err(error) => {
            println!("BUCK4 ERROR {:?}", error);
            return None;
        }
    };
    println!("BUCK4 vdd_g3d: CTRL1 {:#04x}, mode {} (3 on)", buck.ctrl1, buck.mode());
    for half in 0..2 {
        let mut line = [0u32; 4];
        for (i, uv) in line.iter_mut().enumerate() {
            *uv = buck.microvolts(half * 4 + i);
        }
        println!(
            "  DVS{}-{}: {} {} {} {} uV",
            half * 4 + 1,
            half * 4 + 4,
            line[0],
            line[1],
            line[2],
            line[3]
        );
    }
    let mut pins = [b'?'; DVS_PINS as usize];
    let mut chosen = 0;
    for (number, text) in pins.iter_mut().enumerate() {
        let pin = gpio::GPL0.pin(number as u8);
        if number < 3 && pin.read() {
            chosen |= 1 << number;
        }
        *text = match (pin.function(), pin.read()) {
            (gpio::Function::Output, true) => b'1',
            (gpio::Function::Output, false) => b'0',
            (gpio::Function::Input, true) => b'h',
            (gpio::Function::Input, false) => b'l',
            _ => b'x',
        };
    }
    println!(
        "  DVS pins gpl0-0..6: {} (1/0 output, h/l input, x other); DTS wants 1000000",
        core::str::from_utf8(&pins).unwrap_or("?")
    );
    let active = buck.microvolts(chosen);
    println!("  DVS{}, which the pins choose: {} uV (the DTS would choose DVS2, {} uV)", chosen + 1, active, buck.microvolts(1));
    Some(if buck.mode() == 3 { active } else { 0 })
}

fn print_clocks(label: &str) {
    let cmu = Cmu::new();
    let clocks = cmu.g3d_clocks();
    println!(
        "{}: domain {:#x}; SRC_G3D {:#x}, DIV_G3D {:#x}, GATE_IP_G3D {:#x}; sclk_g3d {:?} Hz",
        label,
        mali::power_domain_status(),
        clocks.src,
        clocks.div,
        clocks.gate,
        cmu.sclk_g3d_hz()
    );
}

fn print_identity(label: &str, id: &Identity) {
    let (major, minor) = Identity::revision(id.gp);
    println!("{}: GP {:#010x} {} r{}p{}", label, id.gp, id.gp_name(), major, minor);
    for index in 0..mali::PP_COUNT {
        let (major, minor) = Identity::revision(id.pp[index]);
        println!("  PP{} {:#010x} {} r{}p{}", index, id.pp[index], id.pp_name(index), major, minor);
    }
    let (kib, ways, line, bus) = id.l2();
    println!("  L2 SIZE {:#010x}: {} KiB, {}-way, {} B lines, {}-bit bus", id.l2_size, kib, ways, line, bus);
}

/// Step 5 for one block: the reset, then the GIC's view of its line, then cleared.
fn check_line(name: &str, irq: u16, reset: impl FnOnce() -> Result<u64, mali::Error>) -> bool {
    let gic = Gic::new();
    let before = gic.state(irq).0;
    let us = match reset() {
        Ok(us) => us,
        Err(error) => {
            println!("{} reset ERROR {:?}", name, error);
            return false;
        }
    };
    let raised = gic.state(irq).0;
    let cleared = Mali::new().clear_interrupts().is_ok() && !gic.state(irq).0;
    let good = !before && raised && cleared;
    println!(
        "{} reset in {} us; GIC {}: pending before {}, after {}, cleared {}: {}",
        name,
        us,
        irq,
        before,
        raised,
        cleared,
        verdict(good)
    );
    good
}

fn main(_args: BootArgs) -> ! {
    println!("\nGPU bring-up: Mali-400 MP4");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();

    let volts = supply();
    print_clocks("As U-Boot left it");

    let Some(microvolts) = volts else {
        println!("Supply unknown: the GPU stays off (MISMATCH)");
        wait_keys();
    };
    if microvolts < mali::SAFE_MICROVOLTS {
        println!("Supply {} uV, below the {} uV of 160 MHz: the GPU stays off (MISMATCH)", microvolts, mali::SAFE_MICROVOLTS);
        wait_keys();
    }

    let gpu = Mali::new();
    match gpu.power_on(mali::SAFE_HZ, mct) {
        Ok(hz) => println!("Powered on: sclk_g3d {} Hz", hz),
        Err(error) => {
            println!("Power on ERROR {:?}", error);
            print_clocks("After");
            wait_keys();
        }
    }
    print_clocks("Now");
    console::flush();

    let mut good = true;
    match gpu.identify() {
        Ok(id) => print_identity("Before the GPU's PMU", &id),
        Err(error) => println!("Identify ERROR {:?}", error),
    }
    console::flush();
    match gpu.pmu_status() {
        Ok(status) => println!("GPU PMU status {:#x} (a set bit is a block off; {:#x} all)", status, mali::PMU_ALL),
        Err(error) => println!("GPU PMU ERROR {:?}", error),
    }
    match gpu.pmu_power_up(mali::PMU_ALL, mct) {
        Ok(us) => println!("GPU PMU: blocks up in {} us, status now {:#x}", us, gpu.pmu_status().unwrap_or(u32::MAX)),
        Err(error) => println!("GPU PMU power up ERROR {:?}", error),
    }
    console::flush();
    match gpu.identify() {
        Ok(id) => {
            print_identity("After", &id);
            let all_pp = (0..mali::PP_COUNT).all(|index| id.pp_name(index) == "Mali-400");
            let named = id.gp_name() == "Mali-400" && all_pp;
            println!("Mali-400 GP and four Mali-400 PPs: {}", verdict(named));
            good &= named;
        }
        Err(error) => {
            println!("Identify ERROR {:?}", error);
            good = false;
        }
    }
    println!("GP STATUS {:#x}", gpu.gp_status().unwrap_or(u32::MAX));
    console::flush();

    good &= check_line("GP", mali::GP_IRQ, || gpu.reset_gp(mct, true));
    for index in 0..mali::PP_COUNT {
        let name = ["PP0", "PP1", "PP2", "PP3"][index];
        good &= check_line(name, mali::PP_IRQ[index], || gpu.reset_pp(index, mct, true));
    }
    console::flush();

    for pp in [None, Some(0), Some(1), Some(2), Some(3)] {
        match gpu.reset_mmu(pp, mct) {
            Ok(status) => println!("MMU {:?}: takes a table address, reset; STATUS {:#x}", pp, status),
            Err(error) => {
                println!("MMU {:?} ERROR {:?}", pp, error);
                good = false;
            }
        }
    }
    match gpu.clear_l2(mct) {
        Ok(us) => println!("L2 cleared in {} us", us),
        Err(error) => {
            println!("L2 ERROR {:?}", error);
            good = false;
        }
    }
    println!("GPU test: {}. Keys: r restart, f fastboot", verdict(good));
    wait_keys();
}
