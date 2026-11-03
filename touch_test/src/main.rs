//! Touch screen test: powers up the mXT1664S, reads its object table and configuration, then
//! calibrates it against the panel and draws where it is touched.
//!
//! Calibration: five crosses are drawn one after another, near the corners and in the middle.
//! Touch each one and lift the finger; the cross turns green. Then the map from the
//! controller's positions to pixels is fitted to the five, and how far each lands from its
//! cross is printed, with the plain scaling of the controller's range for comparison. The map
//! is only printed, not stored.
//!
//! After that every touch draws on the screen, a color per finger. Keys on the UART:
//! `c` calibrate again, `x` clear, `k` make the controller take its untouched reference again
//! (nothing may touch the screen), `i` its configuration, `r` restart, `f` fastboot.
//!
//! The controller gets only what `drivers::mxt` writes: its calibrate command and, if what it
//! stored would report nothing, run settings in RAM. Its stored configuration is never
//! changed. BUCK6 and BUCK7 are set to the values Linux uses: see the README's MAX77686
//! section. The panel supply is not touched.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing, rgb};
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::mxt::{self, Message, Mxt, Touch};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::touch::{Calibration, Point, Sample};
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

const PANEL: Timing = LTN101AL03;
const PIXELS: usize = PANEL.width * PANEL.height;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

const BACKGROUND: u32 = rgb(16, 16, 24);
const TARGET: u32 = rgb(255, 255, 255);
const TAKEN: u32 = rgb(0, 220, 0);
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

/// Targets 10 % in from the edges, and the middle.
const MARGIN_X: i32 = PANEL.width as i32 / 10;
const MARGIN_Y: i32 = PANEL.height as i32 / 10;
const TARGETS: [Point; 5] = [
    Point::new(MARGIN_X, MARGIN_Y),
    Point::new(PANEL.width as i32 - 1 - MARGIN_X, MARGIN_Y),
    Point::new(PANEL.width as i32 - 1 - MARGIN_X, PANEL.height as i32 - 1 - MARGIN_Y),
    Point::new(MARGIN_X, PANEL.height as i32 - 1 - MARGIN_Y),
    Point::new(PANEL.width as i32 / 2, PANEL.height as i32 / 2),
];
const CROSS_ARM: usize = 24;
const CROSS_WIDTH: usize = 3;
/// Positions of a touch left out of its mean: the finger is still settling.
const SETTLING_SAMPLES: u32 = 2;
/// Fewer kept than this, and the touch is taken again.
const MIN_SAMPLES: u32 = 3;
/// After a target is taken, touches are ignored this long, so that one tap does not take two.
const TARGET_GAP_MS: u64 = 400;
const DOT: usize = 6;

/// The framebuffer, in `.bss`, as in `display_test`; `memory.x` makes room.
#[repr(align(4096))]
struct Pixels(UnsafeCell<[u32; PIXELS]>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Pixels {}

static PIXEL_STORE: Pixels = Pixels(UnsafeCell::new([0; PIXELS]));

fn wait_ms(mct: Mct, ms: u64) {
    let end = mct.count() + ms * COUNTS_PER_MS;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

fn fail(what: &str, error: impl core::fmt::Debug) -> ! {
    println!("{} ERROR {:?}", what, error);
    cpu::halt();
}

/// What the main loop and the calibration share.
struct Board {
    mct: Mct,
    keys: PowerKeys,
    mxt: Mxt,
    screen: Framebuffer,
    /// Messages from objects other than T6 and T9, counted.
    other_messages: u32,
}

/// A key from the UART that the caller has to act on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    None,
    Pressed(u8),
}

impl Board {
    /// Serves the power keys and the UART, and takes one touch message if the controller has
    /// one. Status messages are printed here. `r` and `f` restart at once.
    fn poll(&mut self) -> (Option<Touch>, Key) {
        self.keys.service(self.mct);
        let key = match console::uart().try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            Some(byte) => Key::Pressed(byte),
            None => Key::None,
        };
        if mxt::CHG.read() {
            return (None, key);
        }
        match self.mxt.read_message() {
            Ok(Some(Message::Touch(touch))) => (Some(touch), key),
            Ok(Some(Message::Status { status, config_crc })) => {
                print_status(status, config_crc);
                (None, key)
            }
            Ok(Some(Message::Other { .. })) => {
                self.other_messages += 1;
                (None, key)
            }
            Ok(None) => (None, key),
            Err(error) => {
                println!("Message read ERROR {:?}", error);
                (None, key)
            }
        }
    }

    fn clear(&mut self) {
        let (width, height) = (self.screen.width(), self.screen.height());
        self.screen.fill_rect(0, 0, width, height, BACKGROUND);
        self.screen.flush();
    }

    fn cross(&mut self, at: Point, color: u32) {
        let (x, y) = (at.x as usize, at.y as usize);
        let half = CROSS_WIDTH / 2;
        let left = x.saturating_sub(CROSS_ARM);
        let top = y.saturating_sub(CROSS_ARM);
        self.screen.fill_rect(left, y - half, 2 * CROSS_ARM + 1, CROSS_WIDTH, color);
        self.screen.fill_rect(x - half, top, CROSS_WIDTH, 2 * CROSS_ARM + 1, color);
        self.screen.flush_rect(left, top, 2 * CROSS_ARM + 1, 2 * CROSS_ARM + 1);
    }

    fn dot(&mut self, at: Point, color: u32) {
        let (width, height) = (self.screen.width() as i32, self.screen.height() as i32);
        let half = DOT as i32 / 2;
        if at.x < half || at.y < half || at.x + half > width || at.y + half > height {
            return;
        }
        let (x, y) = ((at.x - half) as usize, (at.y - half) as usize);
        self.screen.fill_rect(x, y, DOT, DOT, color);
        self.screen.flush_rect(x, y, DOT, DOT);
    }

    /// Waits for one touch from press to release, and returns the mean of its positions
    /// after the first few. Touches too short are taken again. `None` if `c` was pressed.
    fn take_touch(&mut self) -> Option<(Point, u32)> {
        let mut finger = None;
        let (mut sum_x, mut sum_y, mut seen, mut kept) = (0i64, 0i64, 0u32, 0u32);
        loop {
            let (touch, key) = self.poll();
            if key == Key::Pressed(b'c') {
                return None;
            }
            let Some(touch) = touch else { continue };
            if finger.is_none() && touch.status & mxt::STATUS_PRESS != 0 {
                finger = Some(touch.id);
            }
            if finger != Some(touch.id) {
                continue;
            }
            if touch.down() {
                seen += 1;
                if seen > SETTLING_SAMPLES {
                    sum_x += i64::from(touch.x);
                    sum_y += i64::from(touch.y);
                    kept += 1;
                }
            }
            if touch.status & mxt::STATUS_RELEASE != 0 || !touch.down() {
                if kept >= MIN_SAMPLES {
                    let mean = Point::new((sum_x / i64::from(kept)) as i32, (sum_y / i64::from(kept)) as i32);
                    return Some((mean, kept));
                }
                println!("  touch too short ({} positions), touch the cross again", seen);
                finger = None;
                (sum_x, sum_y, seen, kept) = (0, 0, 0, 0);
            }
        }
    }

    /// Takes the five targets and fits the map. `None` if `c` was pressed meanwhile, or the
    /// fit failed.
    fn calibrate(&mut self, plain: &Calibration) -> Option<Calibration> {
        self.clear();
        println!("Calibration: touch each white cross and lift the finger. c starts again.");
        let mut samples = [Sample::default(); TARGETS.len()];
        for (index, (target, sample)) in TARGETS.iter().zip(samples.iter_mut()).enumerate() {
            self.cross(*target, TARGET);
            let (raw, kept) = self.take_touch()?;
            *sample = Sample { raw, screen: *target };
            self.cross(*target, TAKEN);
            println!(
                "  target {} at ({}, {}): raw ({}, {}), mean of {} positions",
                index + 1,
                target.x,
                target.y,
                raw.x,
                raw.y,
                kept
            );
            let end = self.mct.count() + TARGET_GAP_MS * COUNTS_PER_MS;
            while self.mct.count() < end {
                self.poll();
            }
        }
        let Some(fitted) = Calibration::fit(&samples) else {
            println!("Fit failed: the touches lie on one line. c to try again.");
            return None;
        };
        println!(
            "Map: x = {:.5} * rx + {:.5} * ry + {:.1}",
            fitted.x[0], fitted.x[1], fitted.x[2]
        );
        println!(
            "     y = {:.5} * rx + {:.5} * ry + {:.1}",
            fitted.y[0], fitted.y[1], fitted.y[2]
        );
        let (mut worst_plain, mut worst_fit) = (0, 0);
        for (index, sample) in samples.iter().enumerate() {
            let by_plain = plain.residual(sample);
            let by_fit = fitted.residual(sample);
            worst_plain = worst_plain.max(by_plain.x.abs()).max(by_plain.y.abs());
            worst_fit = worst_fit.max(by_fit.x.abs()).max(by_fit.y.abs());
            println!(
                "  target {}: off by ({}, {}) px with the plain scaling, ({}, {}) px with the map",
                index + 1,
                by_plain.x,
                by_plain.y,
                by_fit.x,
                by_fit.y
            );
        }
        println!(
            "Worst: {} px with the plain scaling, {} px with the map",
            worst_plain, worst_fit
        );
        Some(fitted)
    }
}

fn print_status(status: u8, config_crc: u32) {
    print!("T6 status {:#04x}, config CRC {:#08x}:", status, config_crc);
    for (bit, name) in [
        (mxt::T6_RESET, "reset"),
        (mxt::T6_OVERFLOW, "overflow"),
        (mxt::T6_SIGNAL_ERROR, "signal error"),
        (mxt::T6_CALIBRATING, "calibrating"),
        (mxt::T6_CONFIG_ERROR, "config error"),
        (mxt::T6_COMMS_ERROR, "I2C checksum error"),
    ] {
        if status & bit != 0 {
            print!(" {}", name);
        }
    }
    println!("{}", if status == 0 { " running" } else { "" });
}

fn print_config(mxt: &Mxt) {
    match (mxt.touch_config(), mxt.power_config()) {
        (Ok(touch), Ok(power)) => {
            println!(
                "T9: CTRL {:#04x}, ORIENT {:#04x}{}{}{}, {} touches, range {} x {}",
                touch.ctrl,
                touch.orient,
                if touch.orient & mxt::ORIENT_SWITCH != 0 { " switch" } else { "" },
                if touch.orient & mxt::ORIENT_INVERT_X != 0 { " invert-x" } else { "" },
                if touch.orient & mxt::ORIENT_INVERT_Y != 0 { " invert-y" } else { "" },
                touch.touches,
                touch.x_range,
                touch.y_range
            );
            println!("T7: idle {} ms, active {} ms", power.idle_ms, power.active_ms);
        }
        (touch, power) => println!("Config read ERROR {:?} {:?}", touch.err(), power.err()),
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
    screen.fill_rect(0, 0, PANEL.width, PANEL.height, BACKGROUND);
    screen.flush();
    if let Err(error) = fimd.start(&PANEL, &screen) {
        fail("Display start", error);
    }
    display::set_lvds(true);
    wait_ms(mct, u64::from(LVDS_SETTLE_MS));
    display::set_backlight(true);
    screen
}

fn main(_args: BootArgs) -> ! {
    println!("\nTouch test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let pmic_bus = I2c::new(Bus::I2c0);
    if let Err(error) = pmic_bus.init(400_000, 100) {
        fail("PMIC bus", error);
    }
    let pmic = Max77686::new(pmic_bus);
    let start = mct.count();
    match mxt::power_up(pmic, mct) {
        Ok(resets) => println!(
            "Controller up in {} ms, {} reset(s); CHG {}",
            (mct.count() - start) / COUNTS_PER_MS,
            resets,
            if mxt::CHG.read() { "high" } else { "low" }
        ),
        Err(error) => fail("Power-up", error),
    }

    let mxt = match Mxt::open(I2c::new(Bus::I2c5)) {
        Ok(mxt) => mxt,
        Err(error) => fail("Object table", error),
    };
    let info = mxt.info();
    println!(
        "Family {:#04x}, variant {:#04x}, version {:#04x}, build {:#04x}, matrix {} x {}, {} objects, CRC OK",
        info.family, info.variant, info.version, info.build, info.matrix_x, info.matrix_y, info.objects
    );
    print!("Objects (type@address size x instances, report IDs):");
    for (index, object) in mxt.objects().iter().enumerate() {
        if index % 6 == 0 {
            print!("\n ");
        }
        print!(" T{}@{} {}x{}", object.kind, object.address, object.size, object.instances);
        if object.first_report_id != 0 {
            let last = u16::from(object.first_report_id) + u16::from(object.report_ids) * u16::from(object.instances) - 1;
            print!(" #{}-{}", object.first_report_id, last);
        }
    }
    println!();
    print_config(&mxt);
    match mxt.wake() {
        Ok((false, false)) => println!("Stored T7 and T9 already measure and report: nothing written"),
        Ok((power, touch)) => println!("Written to RAM only: T7 {}, T9 CTRL {}", power, touch),
        Err(error) => fail("Wake", error),
    }
    let config = match mxt.touch_config() {
        Ok(config) => config,
        Err(error) => fail("T9 read", error),
    };

    let screen = start_panel(mct);
    let mut board = Board { mct, keys: PowerKeys::new(), mxt, screen, other_messages: 0 };
    let plain = Calibration::scale(config.x_range, config.y_range, PANEL.width, PANEL.height);
    let mut map = plain;
    let mut calibrating = true;
    let mut down = [false; FINGERS.len()];
    let mut moves = [0u32; FINGERS.len()];

    loop {
        if calibrating {
            if let Some(fitted) = board.calibrate(&plain) {
                map = fitted;
                calibrating = false;
                board.clear();
                println!(
                    "Draw on the screen. Keys: c calibrate, x clear, k controller reference, i config, r restart, f fastboot."
                );
            }
            continue;
        }
        let (touch, key) = board.poll();
        match key {
            Key::Pressed(b'c') => calibrating = true,
            Key::Pressed(b'x') => board.clear(),
            Key::Pressed(b'k') => match board.mxt.calibrate() {
                Ok(()) => println!("Calibrate command sent: hands off the screen"),
                Err(error) => println!("Calibrate ERROR {:?}", error),
            },
            Key::Pressed(b'i') => {
                print_config(&board.mxt);
                println!("{} messages from other objects so far", board.other_messages);
            }
            Key::Pressed(b'\r' | b'\n') => println!(),
            Key::Pressed(_) => println!("Keys: c calibrate, x clear, k controller reference, i config, r, f"),
            Key::None => {}
        }
        let Some(touch) = touch else { continue };
        let id = usize::from(touch.id);
        let Some(&color) = FINGERS.get(id) else { continue };
        let raw = Point::new(i32::from(touch.x), i32::from(touch.y));
        let at = map.map(raw);
        if touch.down() {
            board.dot(at, color);
            if !down[id] {
                down[id] = true;
                moves[id] = 0;
                println!(
                    "Finger {} down at ({}, {}), raw ({}, {}), area {}, amplitude {}",
                    id, at.x, at.y, raw.x, raw.y, touch.area, touch.amplitude
                );
            } else {
                moves[id] += 1;
            }
        } else if down[id] {
            down[id] = false;
            println!(
                "Finger {} up at ({}, {}) after {} positions{}",
                id,
                at.x,
                at.y,
                moves[id],
                if touch.status & mxt::STATUS_SUPPRESS != 0 { ", suppressed" } else { "" }
            );
        }
    }
}
