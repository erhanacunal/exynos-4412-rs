//! GPIO test, on pins whose wiring `exynos4412-mehmet.dts` gives. Reads pins that U-Boot
//! must have configured, drives the backlight PWM pin and reads it back, changes the pull of
//! a key, then watches the three keys by polling and through their interrupts.
//!
//! The key interrupts take both paths to the GIC: the power key has its own, and the two
//! volume keys share one.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::gpio::{self, Eint, Function, Pin, Pull, Trigger};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// `exynos4412-mehmet.dts:72`, `power-key`, active low.
const POWER: Pin = gpio::GPX1.pin(3);
/// `exynos4412-mehmet.dts:80`, `volume-up`, active low.
const VOLUME_UP: Pin = gpio::GPX3.pin(4);
/// `exynos4412-mehmet.dts:87`, `volume-down`, active low.
const VOLUME_DOWN: Pin = gpio::GPX3.pin(3);
/// `exynos4412-pinctrl.dtsi:299`, `pwm1_out`, the PWM of the backlight:
/// `exynos4412-mehmet.dts:57-59`.
const BACKLIGHT_PWM: Pin = gpio::GPD0.pin(1);

const KEY_NAMES: [&str; 3] = ["power", "vol+", "vol-"];
const KEYS: [Pin; 3] = [POWER, VOLUME_UP, VOLUME_DOWN];
const VOLUME_UP_INDEX: usize = 1;

/// Rising and falling edges counted by the handler, per key.
static EDGES: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
/// Key interrupts with no pending key behind them.
static EMPTY_IRQS: AtomicU32 = AtomicU32::new(0);
static WRONG_IRQS: AtomicU32 = AtomicU32::new(0);

fn eint(key: usize) -> Eint {
    KEYS[key].eint().expect("all keys are on GPX banks")
}

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if irq.id() == eint(0).irq() || irq.id() == eint(1).irq() {
        let mut found = false;
        for (key, edges) in EDGES.iter().enumerate() {
            let source = eint(key);
            if source.irq() == irq.id() && source.is_pending() {
                source.clear();
                edges.fetch_add(1, Ordering::Relaxed);
                found = true;
            }
        }
        if !found {
            EMPTY_IRQS.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        WRONG_IRQS.fetch_add(1, Ordering::Relaxed);
    }
    gic.end_of_interrupt(irq);
}

fn wait_ms(mct: Mct, ms: u64) {
    let end = mct.count() + ms * COUNTS_PER_MS;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "MISMATCH" }
}

/// U-Boot uses both of these, so their function must be the one the DTS gives. Covers the
/// bank arithmetic of two controllers without changing anything.
fn read_only() {
    let expected = [
        ("gpa1-0 UART2 RX", gpio::GPA1.pin(0), Function::Alt(2)),
        ("gpa1-1 UART2 TX", gpio::GPA1.pin(1), Function::Alt(2)),
        ("gpk0-0 eMMC CLK", gpio::GPK0.pin(0), Function::Alt(3)),
    ];
    for (name, pin, function) in expected {
        let found = pin.function();
        println!("{}: {:?} ({})", name, found, verdict(found == function));
    }
    for (name, pin) in [("gpd0-1 PWM1", BACKLIGHT_PWM), ("power", POWER), ("vol+", VOLUME_UP), ("vol-", VOLUME_DOWN)] {
        println!("{}: from U-Boot {:?}, {:?}, level {}", name, pin.function(), pin.pull(), u8::from(pin.read()));
    }
}

/// Drives the pin and reads the level back from `DAT`, then puts everything back. If the
/// backlight is on, its brightness follows.
fn output(mct: Mct) {
    let pin = BACKLIGHT_PWM;
    let (function, pull, level) = (pin.function(), pin.pull(), pin.read());

    println!("Output: gpd0-1 low for 1 s, high for 1 s, 3 times. If the screen is on, watch it.");
    let mut good = 0;
    pin.set_output(false);
    for _ in 0..3 {
        pin.write(false);
        wait_ms(mct, 1000);
        good += u32::from(!pin.read());
        pin.write(true);
        wait_ms(mct, 1000);
        good += u32::from(pin.read());
    }
    pin.write(level);
    pin.set_pull(pull);
    pin.set_function(function);
    println!("Readback {}/6 ({}), pin restored: {:?}", good, verdict(good == 6), pin.function());
}

/// The pull of a key that nobody is pressing. The pulled-down level shows whether the board
/// has its own pull-up on this line.
fn pull(mct: Mct) {
    let pin = VOLUME_UP;
    pin.set_input(Pull::Down);
    wait_ms(mct, 5);
    let down = (pin.pull(), pin.read());
    pin.set_pull(Pull::Up);
    wait_ms(mct, 5);
    let up = (pin.pull(), pin.read());
    println!(
        "Pull vol+ (do not press): {:?} level {}, {:?} level {} ({})",
        down.0,
        u8::from(down.1),
        up.0,
        u8::from(up.1),
        verdict(down.0 == Pull::Down && up.0 == Pull::Up && up.1),
    );
}

/// Sampled every 5 ms, which also hides contact bounce. Ends when every key has been
/// pressed and released, or on any byte from the UART.
fn polling(mct: Mct) {
    println!("Polling: briefly press and release all 3 keys (do not long-press the power key). Press a UART key to skip.");
    for key in KEYS {
        key.set_input(Pull::Up);
    }
    wait_ms(mct, 5);

    let uart = console::uart();
    let mut pressed = KEYS.map(|key| !key.read());
    let mut released = [false; 3];
    while released != [true; 3] {
        if uart.try_read_byte().is_some() {
            println!("Polling skipped");
            return;
        }
        for (index, key) in KEYS.iter().enumerate() {
            let now = !key.read();
            if now != pressed[index] {
                pressed[index] = now;
                println!("{} {}", KEY_NAMES[index], if now { "pressed" } else { "released" });
                released[index] |= !now;
            }
        }
        wait_ms(mct, 5);
    }
    println!("Polling OK");
}

fn interrupts() -> ! {
    let gic = Gic::new();
    for (key, pin) in KEYS.iter().enumerate() {
        pin.set_pull(Pull::Up);
        let source = eint(key);
        source.listen(Trigger::BothEdges);
        gic.enable(source.irq());
    }
    println!("Interrupts: GIC {} power, {} vol+/vol-. Each press + release makes 2 edges, any more are contact bounce.", eint(0).irq(), eint(1).irq());
    println!("m: toggles the vol+ mask. Other keys echo as <x>.");
    cpu::enable_irq();

    let uart = console::uart();
    let mut seen = [0; 3];
    let mut masked = false;
    loop {
        for (key, edges) in EDGES.iter().enumerate() {
            let now = edges.load(Ordering::Relaxed);
            if now != seen[key] {
                seen[key] = now;
                println!(
                    "{}: {} edges, level {}, empty {} wrong {}",
                    KEY_NAMES[key],
                    now,
                    u8::from(KEYS[key].read()),
                    EMPTY_IRQS.load(Ordering::Relaxed),
                    WRONG_IRQS.load(Ordering::Relaxed),
                );
            }
        }
        match uart.try_read_byte() {
            Some(b'm') => {
                masked = !masked;
                if masked {
                    eint(VOLUME_UP_INDEX).mask();
                } else {
                    // What came while masked is dropped, so that a count shows only
                    // edges seen with the mask off.
                    eint(VOLUME_UP_INDEX).clear();
                    eint(VOLUME_UP_INDEX).unmask();
                }
                println!("vol+ mask: {}", if masked { "on (edges must not be counted)" } else { "off" });
            }
            Some(b'\r') => println!(),
            Some(byte) => print!("<{}>", byte as char),
            None => {}
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nGPIO test");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();

    read_only();
    output(mct);
    pull(mct);
    polling(mct);
    interrupts();
}
