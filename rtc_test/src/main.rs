//! MAX77686 RTC test. Reads the time, then waits for commands on the UART:
//!
//! - `set YYYY-MM-DD HH:MM:SS`: sets the RTC to that time, in UTC, and reads it back;
//! - `read`, or an empty line: reads the time again;
//! - `r`: restart; `f`: restart into fastboot.
//!
//! Nothing is written to the RTC without a `set` command.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Rtc;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::time::DateTime;
use exynos_4412_rs::{BootArgs, console, print, println};

exynos_4412_rs::entry!(main);

/// Shown next to UTC: Turkey is UTC+3 all year.
const UTC_OFFSET_SECONDS: i64 = 3 * 3600;
const LINE_MAX: usize = 40;

fn show(rtc: Rtc, mct: Mct) -> Option<DateTime> {
    match rtc.read(mct) {
        Ok(reading) => {
            print!(
                "RTC: CONTROL {:#04x}, registers {:02x?}: ",
                reading.control, reading.raw
            );
            match reading.time() {
                Some(time) => {
                    println!(
                        "{} UTC, {} UTC+3",
                        time,
                        time.add_seconds(UTC_OFFSET_SECONDS)
                    );
                    Some(time)
                }
                None => {
                    println!("no valid time");
                    None
                }
            }
        }
        Err(error) => {
            println!("RTC ERROR {:x?}", error);
            None
        }
    }
}

/// The decimal number in `text[start..start + digits]`, all digits.
fn number(text: &[u8], start: usize, digits: usize) -> Option<u16> {
    text.get(start..start + digits)?
        .iter()
        .try_fold(0u16, |value, byte| {
            byte.is_ascii_digit()
                .then(|| value * 10 + u16::from(byte - b'0'))
        })
}

/// `YYYY-MM-DD HH:MM:SS`, exactly.
fn parse(text: &[u8]) -> Option<DateTime> {
    if text.len() != 19
        || text[4] != b'-'
        || text[7] != b'-'
        || text[10] != b' '
        || text[13] != b':'
        || text[16] != b':'
    {
        return None;
    }
    let time = DateTime {
        year: number(text, 0, 4)?,
        month: number(text, 5, 2)? as u8,
        day: number(text, 8, 2)? as u8,
        hour: number(text, 11, 2)? as u8,
        minute: number(text, 14, 2)? as u8,
        second: number(text, 17, 2)? as u8,
    };
    time.is_valid().then_some(time)
}

fn set(rtc: Rtc, mct: Mct, text: &[u8]) {
    let Some(time) = parse(text) else {
        println!("Not a valid time: type set YYYY-MM-DD HH:MM:SS, in UTC");
        return;
    };
    match rtc.set(time, mct) {
        Ok(()) => {
            println!("Set to {} UTC", time);
            if let Some(read) = show(rtc, mct) {
                let off = read.to_unix() - time.to_unix();
                println!(
                    "Reads back {} s after the time written ({})",
                    off,
                    if (0..=1).contains(&off) {
                        "OK"
                    } else {
                        "MISMATCH"
                    }
                );
            }
        }
        Err(error) => println!("Set ERROR {:x?}: nothing written", error),
    }
}

fn command(rtc: Rtc, mct: Mct, line: &[u8]) {
    match line {
        b"" | b"read" => {
            show(rtc, mct);
        }
        b"r" => power::restart(BootMode::Normal),
        b"f" => power::restart(BootMode::Fastboot),
        [b's', b'e', b't', b' ', rest @ ..] => set(rtc, mct, rest),
        _ => println!("Commands: set YYYY-MM-DD HH:MM:SS (UTC), read, r, f"),
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nRTC test");
    let mct = Mct::new();
    mct.start();

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("I2C ERROR {:x?}", error);
    }
    let rtc = Rtc::new(i2c);
    show(rtc, mct);
    println!("Commands: set YYYY-MM-DD HH:MM:SS (UTC), read, r, f. Keys: as in power_test.");
    print!("> ");

    let uart = console::uart();
    let mut keys = PowerKeys::new();
    let mut line = [0u8; LINE_MAX];
    let mut length = 0;
    let mut last = 0;
    loop {
        keys.service(mct);
        let Some(byte) = uart.try_read_byte() else {
            continue;
        };
        let previous = core::mem::replace(&mut last, byte);
        match byte {
            // A terminal may end a line with both.
            b'\n' if previous == b'\r' => {}
            b'\r' | b'\n' => {
                println!();
                command(rtc, mct, &line[..length]);
                length = 0;
                print!("> ");
            }
            // Backspace and delete.
            0x08 | 0x7F if length > 0 => {
                length -= 1;
                print!("\x08 \x08");
            }
            0x20..=0x7E if length < LINE_MAX => {
                line[length] = byte;
                length += 1;
                print!("{}", byte as char);
            }
            _ => {}
        }
    }
}
