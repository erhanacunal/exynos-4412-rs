//! I2C test on the devices `exynos4412-mehmet.dts` names. It only sends device addresses and
//! register numbers, and it reads no register that clears on reading, so no device changes
//! state. In particular it reads nothing but the device ID of the PMIC.
//!
//! The time of many identical reads gives the bus rate, independently of the clock arithmetic.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::gpio::{self, Pin, Pull};
use exynos_4412_rs::drivers::i2c::{Bus, Error, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, cpu, mmu, println, soc};

exynos_4412_rs::entry!(main);

const REPEATS: u32 = 1000;
/// Bus clocks of one register read, not counting start and stop: 9 each for the address, the
/// register, the address again and every data byte.
const fn read_clocks(bytes: u64) -> u64 {
    9 * (3 + bytes)
}

/// `exynos4412-mehmet.dts:962`, `max77686: pmic@9`, on `i2c_0`, 400 kHz, SDA delay 100 ns.
const PMIC: u8 = 0x09;
/// `include/linux/mfd/max77686-private.h:20`.
const PMIC_DEVICE_ID: u8 = 0x00;
/// LDO control registers, one per LDO from this one up:
/// `include/linux/mfd/max77686-private.h:80-105`. They do not clear on reading.
const PMIC_LDO1_CTRL1: u8 = 0x40;
/// `drivers/regulator/max77686-regulator.c:58`: the mode is in the top two bits.
const PMIC_OPMODE_SHIFT: u32 = 6;
/// LDOs that feed the devices tested below, from `exynos4412-mehmet.dts`:
/// `vdd_tsp_1v8` and `vdd_tsp` for the touch screen, `vdd_sensor` for the gyroscope,
/// `vdd_audio` for the codec.
const PMIC_LDOS: [(u8, &str); 4] = [(6, "vdd_tsp_1v8"), (26, "vdd_tsp"), (24, "vdd_sensor"), (25, "vdd_audio")];
/// `exynos4412-mehmet.dts:1272`, `max17040: fuel-gauge@36`, on `i2c_3`, 100 kHz, 100 ns.
const GAUGE: u8 = 0x36;
/// Nothing in the DTS on `i2c_3` has this address, so it should not answer.
const GAUGE_ABSENT: u8 = 0x37;
/// `drivers/power/supply/max17040_battery.c:22`.
const GAUGE_VCELL: u8 = 0x02;
/// `drivers/power/supply/max17040_battery.c:23`.
const GAUGE_SOC: u8 = 0x04;
/// `exynos4412-mehmet.dts:1330`, `gyroscope@68`, on `i2c_6`, 100 kHz, 100 ns.
const GYRO: u8 = 0x68;
/// `include/linux/iio/common/st_sensors.h:43` `ST_SENSORS_DEFAULT_WAI_ADDRESS`.
const GYRO_WHO_AM_I: u8 = 0x0F;
/// `drivers/iio/gyro/st_gyro_core.c:70`, for `L3G4200D_GYRO_DEV_NAME`.
const GYRO_ID: u8 = 0xD3;

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "MISMATCH" }
}

fn init(name: &str, bus: Bus, hz: u32, sda_delay_ns: u32) -> Option<(I2c, u32)> {
    let i2c = I2c::new(bus);
    match i2c.init(hz, sda_delay_ns) {
        Ok(rate) => {
            println!("{}: {} Hz requested, {} Hz selected", name, hz, rate);
            Some((i2c, rate))
        }
        Err(error) => {
            println!("{}: init ERROR {:?}", name, error);
            None
        }
    }
}

fn read_reg<const N: usize>(i2c: I2c, address: u8, register: u8) -> Result<[u8; N], Error> {
    let mut value = [0; N];
    i2c.write_read(address, &[register], &mut value)?;
    Ok(value)
}

/// Repeats one read, counts errors, and gives the rate that the time taken implies.
fn stress(mct: Mct, i2c: I2c, rate: u32, address: u8, register: u8, bytes: usize) {
    let mut buffer = [0; 2];
    let start = mct.count();
    let mut errors = 0;
    for _ in 0..REPEATS {
        if i2c.write_read(address, &[register], &mut buffer[..bytes]).is_err() {
            errors += 1;
        }
    }
    let counts = mct.count() - start;
    let clocks = read_clocks(bytes as u64) * u64::from(REPEATS);
    let measured = clocks * u64::from(soc::MCT_CLOCK_HZ) / counts.max(1);
    println!(
        "  {} reads, {} errors ({}), {} us per read, measured rate at least {} Hz, configured {} Hz",
        REPEATS,
        errors,
        verdict(errors == 0),
        counts * 1_000_000 / u64::from(soc::MCT_CLOCK_HZ) / u64::from(REPEATS),
        measured,
        rate,
    );
}

fn gauge(mct: Mct) {
    let Some((i2c, rate)) = init("i2c_3 fuel gauge", Bus::I2c3, 100_000, 100) else {
        return;
    };
    println!("  absent 0x{:02x}: {:?} (AddressNack expected)", GAUGE_ABSENT, i2c.probe(GAUGE_ABSENT));
    println!("  0x{:02x} probe: {:?}", GAUGE, i2c.probe(GAUGE));
    match (read_reg::<2>(i2c, GAUGE, GAUGE_VCELL), read_reg::<2>(i2c, GAUGE, GAUGE_SOC)) {
        (Ok(vcell), Ok(soc)) => {
            // `max17040_battery.c:67-69,186`: 12 bits at the top, 1250 uV per step.
            let millivolts = u32::from(u16::from_be_bytes(vcell) >> 4) * 1250 / 1000;
            // The vendor kernel halves it: `willow/drivers/power/max17040_battery.c:178`.
            println!(
                "  battery {} mV ({}), charge {}% (raw {})",
                millivolts,
                verdict((2800..=4500).contains(&millivolts)),
                soc[0] >> 1,
                soc[0],
            );
        }
        (vcell, soc) => println!("  read ERROR {:?} {:?}", vcell, soc),
    }
    stress(mct, i2c, rate, GAUGE, GAUGE_VCELL, 2);
}

fn pmic(mct: Mct) {
    let Some((i2c, rate)) = init("i2c_0 PMIC", Bus::I2c0, 400_000, 100) else {
        return;
    };
    match read_reg::<1>(i2c, PMIC, PMIC_DEVICE_ID) {
        Ok(id) => println!("  device ID 0x{:02x}", id[0]),
        Err(error) => println!("  read ERROR {:?}", error),
    }
    // Modes 1 to 3: max77686-regulator.c:45-56.
    // Mode 0 is off: `max77686-regulator.c:264`, `regulator_disable_regmap` with no `disable_val`.
    for (ldo, name) in PMIC_LDOS {
        match read_reg::<1>(i2c, PMIC, PMIC_LDO1_CTRL1 + ldo - 1) {
            Ok(value) => println!("  LDO{} {}: raw 0x{:02x}, mode {}", ldo, name, value[0], value[0] >> PMIC_OPMODE_SHIFT),
            Err(error) => println!("  LDO{} read ERROR {:?}", ldo, error),
        }
    }
    stress(mct, i2c, rate, PMIC, PMIC_DEVICE_ID, 1);
}

/// The level of the two lines of a bus before the controller takes them. As inputs with
/// their pull-ups, a line that stays at 0 is being held low by something on the board.
fn line_levels(name: &str, lines: [Pin; 2], mct: Mct) {
    for line in lines {
        line.set_input(Pull::Up);
    }
    let end = mct.count() + u64::from(soc::MCT_CLOCK_HZ) / 1000;
    while mct.count() < end {
        core::hint::spin_loop();
    }
    println!("{} lines: {} {}", name, u8::from(lines[0].read()), u8::from(lines[1].read()));
}

fn gyro(mct: Mct) {
    line_levels("i2c_6", [gpio::GPC1.pin(3), gpio::GPC1.pin(4)], mct);
    let Some((i2c, _)) = init("i2c_6 gyroscope", Bus::I2c6, 100_000, 100) else {
        return;
    };
    match read_reg::<1>(i2c, GYRO, GYRO_WHO_AM_I) {
        Ok(id) => println!("  WHO_AM_I 0x{:02x}, expected 0x{:02x} ({})", id[0], GYRO_ID, verdict(id[0] == GYRO_ID)),
        Err(error) => println!("  read: {:?} (U-Boot may not have enabled the supply)", error),
    }
}

/// These only get their address. An error may just mean that U-Boot left the device off.
fn probes(mct: Mct) {
    // `exynos4412-mehmet.dts`: `codec@1a`, `touchscreen@4a`, `light-sensor@44`.
    // Pins: `exynos4412-pinctrl.dtsi:349,225,314`.
    let devices = [
        ("i2c_1 audio", Bus::I2c1, 100, 0x1A, [gpio::GPD1.pin(2), gpio::GPD1.pin(3)]),
        ("i2c_5 touchscreen", Bus::I2c5, 100, 0x4A, [gpio::GPB.pin(2), gpio::GPB.pin(3)]),
        ("i2c_7 light sensor", Bus::I2c7, 5, 0x44, [gpio::GPD0.pin(2), gpio::GPD0.pin(3)]),
    ];
    for (name, bus, sda_delay_ns, address, lines) in devices {
        line_levels(name, lines, mct);
        if let Some((i2c, _)) = init(name, bus, 100_000, sda_delay_ns) {
            println!("  0x{:02x} probe: {:?}", address, i2c.probe(address));
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nI2C test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let cmu = Cmu::new();
    println!("MPLL {} Hz, aclk100 {:?} Hz", cmu.fout_mpll_hz(), cmu.aclk100_hz());

    gauge(mct);
    pmic(mct);
    gyro(mct);
    probes(mct);

    println!("Test finished");
    cpu::halt();
}
