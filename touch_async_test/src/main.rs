//! Async touch test: the mXT1664S read with `await`, on the interrupt of its CHG line, while
//! a task ticks every 1 ms on the same core.
//!
//! After the power-up, the touch task first reads CHG's data bit with the pin in its
//! interrupt function, empties the controller's messages, and reads the bit again. Then it
//! waits for messages and draws every touch on the panel, mapped with the five-point fit
//! `touch_test` measured on this board.
//!
//! Every 5 s, if anything changed, a line gives the messages read, the CHG interrupts, the
//! wake-ups that found no message, and the ticker: how many ticks, and the latest one.
//! With nothing touching, the interrupts must stay still. Keys on the UART: `x` clear,
//! `k` controller calibrate command (nothing may touch the screen), `r` restart,
//! `f` fastboot.
//!
//! The controller gets only what `drivers::mxt` writes: see `touch_test`.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::select::{Either, select};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing, rgb};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::gpio::Pull;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::mxt::{self, Message, Mxt};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::eint::EintInput;
use exynos_4412_rs::embassy::uart::UartRx;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::touch::{Calibration, Point};
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println, soc};

exynos_4412_rs::entry!(main);

const PANEL: Timing = LTN101AL03;
const PIXELS: usize = PANEL.width * PANEL.height;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// The map `touch_test` fitted on this board: see the README's Touch screen section.
const BOARD_MAP: Calibration = Calibration { x: [0.31574, 0.00030, -2.7], y: [0.00097, 0.19425, -2.7] };

const BACKGROUND: u32 = rgb(16, 16, 24);
/// One per finger, by its T9 ID.
const FINGERS: [u32; 10] = [
    rgb(255, 64, 64),
    rgb(64, 160, 255),
    rgb(255, 220, 0),
    rgb(0, 230, 160),
    rgb(230, 80, 255),
    rgb(255, 140, 0),
    rgb(160, 255, 64),
    rgb(0, 220, 255),
    rgb(255, 128, 192),
    rgb(200, 200, 200),
];
const DOT: usize = 6;
const REPORT_PERIOD: Duration = Duration::from_secs(5);

#[repr(align(4096))]
struct Pixels(UnsafeCell<[u32; PIXELS]>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Pixels {}

static PIXEL_STORE: Pixels = Pixels(UnsafeCell::new([0; PIXELS]));

static TICKS: AtomicU32 = AtomicU32::new(0);
static MAX_LATE_US: AtomicU32 = AtomicU32::new(0);
static MESSAGES: AtomicU32 = AtomicU32::new(0);
static EMPTY_WAKES: AtomicU32 = AtomicU32::new(0);
static READ_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Time spent reading messages over I2C, in µs: the executor runs nothing else meanwhile.
static READ_US: AtomicU32 = AtomicU32::new(0);
static CLEAR: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if !embassy::on_interrupt(irq.id()) {
        println!("unexpected interrupt {} on CPU{}", irq.id(), cpu::id());
    }
    gic.end_of_interrupt(irq);
}

fn fail(what: &str, error: impl core::fmt::Debug) -> ! {
    println!("{} ERROR {:?}", what, error);
    cpu::halt();
}

#[embassy_executor::task]
async fn ticker() {
    loop {
        let begin = Instant::now();
        Timer::after(Duration::from_millis(1)).await;
        let late = begin.elapsed().as_micros().saturating_sub(1000) as u32;
        MAX_LATE_US.fetch_max(late, Ordering::Relaxed);
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
}

fn dot(screen: &mut Framebuffer, at: Point, color: u32) {
    let (width, height) = (screen.width() as i32, screen.height() as i32);
    let half = DOT as i32 / 2;
    if at.x < half || at.y < half || at.x + half > width || at.y + half > height {
        return;
    }
    let (x, y) = ((at.x - half) as usize, (at.y - half) as usize);
    screen.fill_rect(x, y, DOT, DOT, color);
    screen.flush_rect(x, y, DOT, DOT);
}

fn clear(screen: &mut Framebuffer) {
    let (width, height) = (screen.width(), screen.height());
    screen.fill_rect(0, 0, width, height, BACKGROUND);
    screen.flush();
}

/// One message read, timed, and counted.
fn read_one(mxt: &Mxt) -> Option<Message> {
    let begin = Instant::now();
    let result = mxt.read_message();
    READ_US.fetch_add(begin.elapsed().as_micros() as u32, Ordering::Relaxed);
    match result {
        Ok(Some(message)) => {
            MESSAGES.fetch_add(1, Ordering::Relaxed);
            Some(message)
        }
        Ok(None) => None,
        Err(error) => {
            READ_ERRORS.fetch_add(1, Ordering::Relaxed);
            println!("Message read ERROR {:?}", error);
            None
        }
    }
}

#[embassy_executor::task]
async fn touch_task(mxt: Mxt, chg: EintInput, mut screen: Framebuffer) {
    let before = chg.pin().read();
    let mut drained = 0;
    while let Some(message) = read_one(&mxt) {
        drained += 1;
        if let Message::Status { status, config_crc } = message {
            println!("T6 status {:#04x}, config CRC {:#08x}", status, config_crc);
        }
    }
    println!(
        "CHG data bit in the interrupt function: {} with messages waiting, {} after reading {} ({})",
        u8::from(before),
        u8::from(chg.pin().read()),
        drained,
        if drained > 0 && !before && chg.pin().read() {
            "follows the line"
        } else {
            "does not follow the line, or no message was waiting"
        }
    );
    println!("Draw on the screen. Keys: x clear, k controller calibrate command, r restart, f fastboot.");

    let mut down = [false; FINGERS.len()];
    let mut positions = [0u32; FINGERS.len()];
    loop {
        // What `Mxt::next_message` does, with the wake-ups that find nothing counted.
        // While messages keep coming, CHG never goes high and the loop below never awaits:
        // other tasks get their turn here, between messages.
        yield_now().await;
        let mut woken = false;
        let message = loop {
            if let Some(message) = read_one(&mxt) {
                break Some(message);
            }
            if woken {
                EMPTY_WAKES.fetch_add(1, Ordering::Relaxed);
            }
            match select(chg.wait_low(), CLEAR.wait()).await {
                Either::First(()) => woken = true,
                Either::Second(()) => break None,
            }
        };
        let Some(message) = message else {
            clear(&mut screen);
            continue;
        };
        match message {
            Message::Touch(touch) => {
                let id = usize::from(touch.id);
                let Some(&color) = FINGERS.get(id) else { continue };
                let raw = Point::new(i32::from(touch.x), i32::from(touch.y));
                let at = BOARD_MAP.map(raw);
                if touch.down() {
                    dot(&mut screen, at, color);
                    positions[id] += 1;
                    if !down[id] {
                        down[id] = true;
                        positions[id] = 0;
                        println!("Finger {} down at ({}, {})", id, at.x, at.y);
                    }
                } else if down[id] {
                    down[id] = false;
                    println!("Finger {} up at ({}, {}) after {} positions", id, at.x, at.y, positions[id]);
                }
            }
            Message::Status { status, config_crc } => {
                println!("T6 status {:#04x}, config CRC {:#08x}", status, config_crc);
            }
            Message::Other { .. } => {}
        }
    }
}

/// Every [`REPORT_PERIOD`], the counts of the period, if a message or an interrupt came.
#[embassy_executor::task]
async fn report_task(chg: EintInput) {
    let (mut messages, mut interrupts, mut empty, mut ticks, mut read_us) = (0, 0, 0, 0, 0);
    MAX_LATE_US.store(0, Ordering::Relaxed);
    loop {
        Timer::after(REPORT_PERIOD).await;
        let now = (
            MESSAGES.load(Ordering::Relaxed),
            chg.interrupts(),
            EMPTY_WAKES.load(Ordering::Relaxed),
            TICKS.load(Ordering::Relaxed),
            READ_US.load(Ordering::Relaxed),
        );
        let late = MAX_LATE_US.swap(0, Ordering::Relaxed);
        if now.0 != messages || now.1 != interrupts {
            println!(
                "{} s: {} messages, {} CHG interrupts, {} wake-ups with none; {} ms reading; ticker {} ticks, latest {} us late; {} read errors in all",
                REPORT_PERIOD.as_secs(),
                now.0 - messages,
                now.1 - interrupts,
                now.2 - empty,
                (now.4 - read_us) / 1000,
                now.3 - ticks,
                late,
                READ_ERRORS.load(Ordering::Relaxed)
            );
        }
        (messages, interrupts, empty, ticks, read_us) = now;
    }
}

#[embassy_executor::task]
async fn keys_task() {
    let mut keys = PowerKeys::new();
    let mct = Mct::new();
    loop {
        keys.service(mct);
        Timer::after(Duration::from_millis(10)).await;
    }
}

#[embassy_executor::task]
async fn console_task(mxt: Mxt) {
    let rx = UartRx::new(console::uart().id()).expect("the console UART has an interrupt");
    loop {
        match rx.read_byte().await {
            b'r' => power::restart(BootMode::Normal),
            b'f' => power::restart(BootMode::Fastboot),
            b'x' => CLEAR.signal(()),
            b'k' => match mxt.calibrate() {
                Ok(()) => println!("Calibrate command sent: hands off the screen"),
                Err(error) => println!("Calibrate ERROR {:?}", error),
            },
            b'\r' | b'\n' => println!(),
            _ => println!("Keys: x clear, k controller calibrate command, r restart, f fastboot"),
        }
    }
}

fn start_panel(mct: Mct) -> Framebuffer {
    let fimd = Fimd::new();
    if let Err(error) = fimd.power_on() {
        fail("Display power on", error);
    }
    display::configure_pins();
    // SAFETY: the only reference to this buffer, taken once: see `Pixels`.
    let pixels: &'static mut [u32] = unsafe { &mut *PIXEL_STORE.0.get() };
    let Some(mut screen) = Framebuffer::new(pixels, PANEL.width, PANEL.height) else {
        fail("Framebuffer", "too small");
    };
    clear(&mut screen);
    if let Err(error) = fimd.start(&PANEL, &screen) {
        fail("Display start", error);
    }
    display::set_lvds(true);
    let end = mct.count() + u64::from(LVDS_SETTLE_MS) * COUNTS_PER_MS;
    while mct.count() < end {
        core::hint::spin_loop();
    }
    display::set_backlight(true);
    screen
}

fn main(_args: BootArgs) -> ! {
    println!("\nAsync touch test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();

    let pmic_bus = I2c::new(Bus::I2c0);
    if let Err(error) = pmic_bus.init(400_000, 100) {
        fail("PMIC bus", error);
    }
    match mxt::power_up(Max77686::new(pmic_bus), mct) {
        Ok(resets) => println!("Controller up, {} reset(s)", resets),
        Err(error) => fail("Power-up", error),
    }
    let mxt = match Mxt::open(I2c::new(Bus::I2c5)) {
        Ok(mxt) => mxt,
        Err(error) => fail("Object table", error),
    };
    let info = mxt.info();
    println!(
        "Family {:#04x}, version {:#04x}, build {:#04x}, {} objects, CRC OK",
        info.family, info.version, info.build, info.objects
    );
    match mxt.wake() {
        Ok((false, false)) => {}
        Ok((power, touch)) => println!("Written to RAM only: T7 {}, T9 CTRL {}", power, touch),
        Err(error) => fail("Wake", error),
    }
    // The controller drives CHG; `power_up` leaves the pin an input without a pull.
    let Some(chg) = EintInput::new(mxt::CHG, Pull::None) else {
        fail("CHG", "not a GPX pin");
    };
    println!("CHG on GIC interrupt {}, level low", mxt::CHG.eint().map_or(0, |eint| eint.irq()));
    let screen = start_panel(mct);

    embassy::run(|spawner| {
        spawner.spawn(ticker().unwrap());
        spawner.spawn(keys_task().unwrap());
        spawner.spawn(report_task(chg).unwrap());
        spawner.spawn(console_task(mxt.clone()).unwrap());
        spawner.spawn(touch_task(mxt, chg, screen).unwrap());
    })
}
