//! MAX77686 test. Turns on the rails of the light sensor, the audio codec and the touch screen
//! at the voltages `exynos4412-mehmet.dts` gives them, checks that each device answers on I2C,
//! and then puts every rail back as it found them.
//!
//! The touch controller also needs a buck that U-Boot leaves too low. This looks for the least
//! that is enough, in one reset, with the bucks raised before the controller's own rails.
//!
//! The only registers written are the control registers of the four LDOs below, and the voltage
//! registers of BUCK6 and BUCK7 if the person at the console agrees when asked. The devices get
//! no data, only their addresses and the register to read from.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::gpio::{self, Pin, Pull};
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::{Error, LdoState, Max77686};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// `exynos4412-mehmet.dts:1011-1014`.
const LDO_TOUCH_LOGIC: (u8, u16) = (6, 1800);
/// `exynos4412-mehmet.dts:1152-1155`, lowest of the range.
const LDO_TOUCH_ANALOG: (u8, u16) = (26, 3000);
/// `exynos4412-mehmet.dts:1136-1139`.
const LDO_SENSOR: (u8, u16) = (24, 3000);
/// `exynos4412-mehmet.dts:1144-1147`.
const LDO_AUDIO: (u8, u16) = (25, 3300);
const LDOS: [(u8, u16); 4] = [LDO_TOUCH_LOGIC, LDO_TOUCH_ANALOG, LDO_SENSOR, LDO_AUDIO];
/// Longer than any of these rails needs, by a wide margin. Not measured.
const SETTLE_MS: u64 = 10;

/// `vdd_in235`, `exynos4412-mehmet.dts:1217`, and the value Linux sets on this board.
const BUCK7: (u8, u16) = (7, 3300);
/// `vdd_buck6`, and the value Linux sets on this board.
const BUCK6: (u8, u16) = (6, 2000);
/// What BUCK7 is set to after a power-on, as U-Boot leaves it. Linux changes it, and the PMIC
/// keeps it across a restart, so this tells which of the two states the board is in.
const BUCK7_POWER_ON_MILLIVOLTS: u16 = 2000;
const PROMPT_SECONDS: u64 = 60;

/// `exynos4412-mehmet.dts:1245`.
const CODEC: u8 = 0x1A;
/// `exynos4412-mehmet.dts`, `light-sensor@44`, which names no supply.
const LIGHT: u8 = 0x44;
/// `exynos4412-mehmet.dts:1293`.
const TOUCH: u8 = 0x4A;
/// `drivers/input/touchscreen/atmel_mxt_ts.c:668-669`: register addresses are two bytes, low
/// byte first. The information block is at 0, seven bytes, in `struct mxt_info` at
/// `atmel_mxt_ts.c:225-233`.
const TOUCH_INFO_ADDRESS: [u8; 2] = [0, 0];
const TOUCH_READS: u32 = 100;
/// Addresses outside this are reserved by the I2C specification.
const SCAN_FIRST: u8 = 0x08;
const SCAN_LAST: u8 = 0x77;

/// `willow/arch/arm/mach-exynos/include/mach/gpio-mehmet.h:121`, active low.
const TOUCH_RESET: Pin = gpio::GPB.pin(4);
/// `willow/arch/arm/mach-exynos/include/mach/gpio-mehmet.h:122`, `wake-gpios` in the DTS.
const TOUCH_XVDD_ENABLE: Pin = gpio::GPM3.pin(3);
/// `exynos4412-mehmet.dts:1315`, `atmel,irq-gpio`, driven by the controller, active low.
const TOUCH_CHG: Pin = gpio::GPX0.pin(4);

fn wait_ms(mct: Mct, ms: u64) {
    let end = mct.count() + ms * COUNTS_PER_MS;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "MISMATCH" }
}

fn bus(name: &str, bus: Bus, sda_delay_ns: u32) -> Option<I2c> {
    let i2c = I2c::new(bus);
    match i2c.init(100_000, sda_delay_ns) {
        Ok(_) => Some(i2c),
        Err(error) => {
            println!("{}: init ERROR {:?}", name, error);
            None
        }
    }
}

fn enable(pmic: Max77686, mct: Mct, (ldo, millivolts): (u8, u16)) -> Result<(), Error> {
    pmic.enable_ldo(ldo, millivolts)?;
    wait_ms(mct, SETTLE_MS);
    let state = pmic.ldo(ldo)?;
    println!(
        "LDO{} on: raw 0x{:02x}, mode {}, {} mV ({})",
        ldo,
        state.raw(),
        state.mode(),
        state.millivolts(),
        verdict(state.mode() == 3 && state.millivolts() == millivolts),
    );
    Ok(())
}

/// Lists every address that answers. Only addresses go out, which no device acts on. This also
/// keeps to the address alone for the bootloader at 0x26, whose protocol can erase the firmware.
fn scan(i2c: I2c) {
    print!("  i2c_5 scan:");
    for address in SCAN_FIRST..=SCAN_LAST {
        if i2c.probe(address).is_ok() {
            print!(" 0x{:02x}", address);
        }
    }
    println!();
}

fn sensor_and_codec(pmic: Max77686, mct: Mct) -> Result<(), Error> {
    let light = bus("i2c_7", Bus::I2c7, 5);
    let codec = bus("i2c_1", Bus::I2c1, 100);
    if let Some(i2c) = light {
        println!("Light sensor, rail off: {:?}", i2c.probe(LIGHT));
    }
    enable(pmic, mct, LDO_SENSOR)?;
    if let Some(i2c) = light {
        println!("  light sensor: {:?}", i2c.probe(LIGHT));
    }
    if let Some(i2c) = codec {
        println!("Codec, rail off: {:?}", i2c.probe(CODEC));
    }
    enable(pmic, mct, LDO_AUDIO)?;
    if let Some(i2c) = codec {
        println!("  codec: {:?}", i2c.probe(CODEC));
    }
    Ok(())
}

/// One power-up of the controller, with one reset, in the order and times of
/// `drivers/input/touchscreen/atmel_mxt_ts.c:3200-3243`. Whether it answered in application mode.
fn touch_power_up(pmic: Max77686, mct: Mct) -> Result<bool, Error> {
    TOUCH_RESET.set_output(false);
    TOUCH_XVDD_ENABLE.set_output(true);
    enable(pmic, mct, LDO_TOUCH_ANALOG)?;
    enable(pmic, mct, LDO_TOUCH_LOGIC)?;
    wait_ms(mct, 70);
    TOUCH_RESET.write(true);
    wait_ms(mct, 300);

    println!("  CHG level {}", u8::from(TOUCH_CHG.read()));
    let Some(i2c) = bus("i2c_5", Bus::I2c5, 100) else {
        return Ok(false);
    };
    scan(i2c);
    let mut info = [0; 7];
    if let Err(error) = i2c.write_read(TOUCH, &TOUCH_INFO_ADDRESS, &mut info) {
        println!("  information block: {:?}", error);
        return Ok(false);
    }
    println!(
        "  family 0x{:02x}, variant 0x{:02x}, version 0x{:02x}, build 0x{:02x}, matrix {} x {}, {} objects",
        info[0], info[1], info[2], info[3], info[4], info[5], info[6],
    );
    let mut again = [0; 7];
    let (mut errors, mut different) = (0, 0);
    for _ in 0..TOUCH_READS {
        match i2c.write_read(TOUCH, &TOUCH_INFO_ADDRESS, &mut again) {
            Ok(()) if again == info => {}
            Ok(()) => different += 1,
            Err(_) => errors += 1,
        }
    }
    println!("  {} more reads: {} errors, {} different ({})", TOUCH_READS, errors, different, verdict(errors == 0 && different == 0));
    Ok(true)
}

fn touch_power_down(pmic: Max77686, mct: Mct, touch_rails: [Option<LdoState>; 2]) -> Result<(), Error> {
    TOUCH_RESET.write(false);
    TOUCH_XVDD_ENABLE.write(false);
    wait_ms(mct, 10);
    for state in touch_rails.into_iter().flatten() {
        pmic.restore_ldo(state)?;
    }
    wait_ms(mct, SETTLE_MS);
    Ok(())
}

fn confirmed(mct: Mct) -> bool {
    let uart = console::uart();
    let end = mct.count() + PROMPT_SECONDS * u64::from(soc::MCT_CLOCK_HZ);
    while mct.count() < end {
        if let Some(byte) = uart.try_read_byte() {
            return byte == b'y';
        }
    }
    false
}

fn raise_buck(pmic: Max77686, mct: Mct, (number, millivolts): (u8, u16)) -> Result<(), Error> {
    let before = pmic.buck_millivolts(number)?;
    pmic.set_buck_millivolts(number, millivolts)?;
    wait_ms(mct, SETTLE_MS);
    println!("BUCK{}: {} -> {} mV", number, before, pmic.buck_millivolts(number)?);
    Ok(())
}

/// Tries with the rails of the controller alone, then with BUCK7 raised, then BUCK6 too.
fn touch(pmic: Max77686, mct: Mct, touch_rails: [Option<LdoState>; 2]) -> Result<(), Error> {
    TOUCH_CHG.set_input(Pull::None);

    println!("Touch screen, its own rails only:");
    if touch_power_up(pmic, mct)? {
        println!("Touch answered with: its own rails only");
        return Ok(());
    }
    touch_power_down(pmic, mct, touch_rails)?;

    println!("Press y within {} s to raise BUCK7, and then BUCK6 if needed, anything else to stop.", PROMPT_SECONDS);
    if !confirmed(mct) {
        println!("  stopped");
        return Ok(());
    }
    raise_buck(pmic, mct, BUCK7)?;
    println!("Touch screen, BUCK7 raised before its rails:");
    if touch_power_up(pmic, mct)? {
        println!("Touch answered with: BUCK7");
        return Ok(());
    }
    touch_power_down(pmic, mct, touch_rails)?;

    raise_buck(pmic, mct, BUCK6)?;
    println!("Touch screen, BUCK7 and BUCK6 raised before its rails:");
    if touch_power_up(pmic, mct)? {
        println!("Touch answered with: BUCK7 and BUCK6");
    } else {
        println!("Touch did not answer with one reset");
    }
    Ok(())
}

fn main(_args: BootArgs) -> ! {
    println!("\nMAX77686 test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("i2c_0: init ERROR {:?}", error);
        cpu::halt();
    }
    let pmic = Max77686::new(i2c);
    println!("Device ID: {:?}", pmic.device_id());

    // Everything needed to undo this test. Nothing has been changed if this fails.
    let mut saved: [Option<LdoState>; 4] = [None; 4];
    for (slot, (ldo, _)) in saved.iter_mut().zip(LDOS) {
        match pmic.ldo(ldo) {
            Ok(state) => {
                println!("LDO{} at start: raw 0x{:02x}, mode {}, {} mV", ldo, state.raw(), state.mode(), state.millivolts());
                *slot = Some(state);
            }
            Err(error) => {
                println!("LDO{} read ERROR {:?}, nothing changed", ldo, error);
                cpu::halt();
            }
        }
    }
    let (buck6, buck7) = match (pmic.buck_millivolts(BUCK6.0), pmic.buck_millivolts(BUCK7.0)) {
        (Ok(buck6), Ok(buck7)) => (buck6, buck7),
        (buck6, buck7) => {
            println!("Buck read ERROR {:?} {:?}, nothing changed", buck6, buck7);
            cpu::halt();
        }
    };
    println!("BUCK6 at start: {} mV, BUCK7 at start: {} mV", buck6, buck7);
    if buck7 == BUCK7_POWER_ON_MILLIVOLTS {
        println!("PMIC state: from power-on");
    } else {
        println!("PMIC state: NOT from power-on, kept from Linux. Results below do not count.");
    }

    let touch_pins = [TOUCH_RESET, TOUCH_XVDD_ENABLE, TOUCH_CHG].map(|pin| (pin, pin.function(), pin.pull(), pin.read()));
    let touch_rails = [saved[1], saved[0]];

    if let Err(error) = sensor_and_codec(pmic, mct).and_then(|()| touch(pmic, mct, touch_rails)) {
        println!("Stopped by ERROR {:?}", error);
    }

    println!("Restoring");
    // Same order as `willow/arch/arm/mach-exynos/mach-mehmet.c:2483-2514`, signals first.
    if let Err(error) = touch_power_down(pmic, mct, touch_rails) {
        println!("  touch rails restore ERROR {:?}", error);
    }
    for state in [saved[3], saved[2]].into_iter().flatten() {
        if let Err(error) = pmic.restore_ldo(state) {
            println!("  raw 0x{:02x} restore ERROR {:?}", state.raw(), error);
        }
    }
    for (number, millivolts) in [(BUCK7.0, buck7), (BUCK6.0, buck6)] {
        if let Err(error) = pmic.set_buck_millivolts(number, millivolts) {
            println!("  BUCK{} restore ERROR {:?}", number, error);
        }
    }
    for (pin, function, pull, level) in touch_pins {
        pin.write(level);
        pin.set_pull(pull);
        pin.set_function(function);
    }
    println!("Final state: LDO6 {:?}", pmic.ldo(6).map(|state| state.raw()));
    println!("             LDO26 {:?}", pmic.ldo(26).map(|state| state.raw()));
    println!("             LDO24 {:?}", pmic.ldo(24).map(|state| state.raw()));
    println!("             LDO25 {:?}", pmic.ldo(25).map(|state| state.raw()));
    println!("             BUCK6 {:?} mV, BUCK7 {:?} mV", pmic.buck_millivolts(6), pmic.buck_millivolts(7));

    println!("Test done");
    cpu::halt();
}
