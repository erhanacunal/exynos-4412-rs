// SPDX-License-Identifier: GPL-2.0-or-later
//! Plays an H.264 stream on the panel through the MFC (`exynos_4412_mfc`), over and over.
//!
//! The stream and the rate are chosen at build time (`build.rs`): `MFC_VIDEO`, in Annex B form,
//! `MFC_FPS`. The pictures are shown in display order, the decoder reordering them
//! ([`Config::display_delay`] `None`). The CPU converts each to RGB into one of three
//! framebuffers while the MFC decodes the next picture, then the FIMD switches to it at a vsync
//! (`Fimd::show`): one buffer on the panel, one waiting for its vsync, one being drawn. A
//! picture that fits the panel is scaled up by the largest whole number that still fits; one that
//! fits only turned, a portrait picture on the landscape panel, is turned a quarter turn
//! clockwise. One line per pass gives the times.
//!
//! Keys: `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_mfc::annexb::{header, pictures};
use exynos_4412_mfc::convert::{self, MAX_SCALE};
use exynos_4412_mfc::h264::{Config, Decoded, Decoder, Sequence};
use exynos_4412_mfc::{Banks, Error, Mfc, regs};
use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing, rgb};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mfc;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/s5p-mfc.fw"));
static STREAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/video.h264"));
const FPS: u64 = include!(concat!(env!("OUT_DIR"), "/fps.rs"));

/// The two ports' memory: port A as Linux reserves it (`exynos-mfc-reserved-memory.dtsi`), port B
/// larger than its 8 MiB, which holds only six 720p frame buffers. Far above this program and
/// uart_boot's stage at 0x50000000.
const BANKS: Banks = Banks { a: 0x6000_0000, a_len: 36 << 20, b: 0x6400_0000, b_len: 64 << 20 };

/// The largest picture here is far smaller than 1 MiB. Two frame buffers more than the stream
/// needs: one shown, one spare.
const CONFIG: Config = Config { stream_bytes: 1 << 20, extra_frames: 2, display_delay: None };

const PANEL: Timing = LTN101AL03;
const PIXELS: usize = PANEL.width * PANEL.height;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

/// The framebuffers, in `.bss`, inside the first 16 MiB of DRAM that the uncached view covers:
/// `memory.x` makes room.
#[repr(align(4096))]
struct Pixels(UnsafeCell<[[u32; PIXELS]; BUFFERS]>);

// SAFETY: `main` is the only user, and takes the one reference to them once.
unsafe impl Sync for Pixels {}

const BUFFERS: usize = 3;
static PIXEL_STORE: Pixels = Pixels(UnsafeCell::new([[0; PIXELS]; BUFFERS]));

fn key() {
    match console::uart().try_read_byte() {
        Some(b'r') => power::restart(BootMode::Normal),
        Some(b'f') => power::restart(BootMode::Fastboot),
        _ => {}
    }
}

fn wait_keys() -> ! {
    println!("Keys: r restart, f fastboot");
    console::flush();
    loop {
        key();
    }
}

/// Powers the panel on, black, showing the first framebuffer.
fn start_panel(mct: Mct) -> Result<[Framebuffer; BUFFERS], display::Error> {
    let fimd = Fimd::new();
    fimd.power_on()?;
    display::configure_pins();
    // SAFETY: the only reference to these buffers, taken once: see `Pixels`.
    let store: &'static mut [[u32; PIXELS]; BUFFERS] = unsafe { &mut *PIXEL_STORE.0.get() };
    let mut buffers = store.each_mut().map(|pixels| Framebuffer::new(pixels, PANEL.width, PANEL.height));
    if buffers.iter().any(Option::is_none) {
        return Err(display::Error::BadSize);
    }
    let mut framebuffers = buffers.each_mut().map(|buffer| buffer.take().expect("checked above"));
    for framebuffer in &mut framebuffers {
        framebuffer.fill_rect(0, 0, PANEL.width, PANEL.height, rgb(0, 0, 0));
        framebuffer.flush();
    }
    fimd.start(&PANEL, &framebuffers[0])?;
    display::set_lvds(true);
    let end = mct.count() + u64::from(LVDS_SETTLE_MS) * 1000 * COUNTS_PER_US;
    while mct.count() < end {}
    display::set_backlight(true);
    Ok(framebuffers)
}

/// Where and how a picture goes on the panel.
#[derive(Clone, Copy, Debug)]
struct Layout {
    x: usize,
    y: usize,
    /// 0: turned a quarter turn, at its size.
    scale: usize,
}

impl Layout {
    fn new(sequence: &Sequence) -> Self {
        let (width, height) = (sequence.width, sequence.height);
        let scale = (PANEL.width / width).min(PANEL.height / height).min(MAX_SCALE);
        if scale >= 1 {
            return Self { x: (PANEL.width - width * scale) / 2, y: (PANEL.height - height * scale) / 2, scale };
        }
        if height <= PANEL.width && width <= PANEL.height {
            return Self { x: (PANEL.width - height) / 2, y: (PANEL.height - width) / 2, scale: 0 };
        }
        // Too large either way: the top left corner, clipped.
        Self { x: 0, y: 0, scale: 1 }
    }
}

/// Times of one pass.
#[derive(Default)]
struct Pass {
    decoded: u64,
    shown: u64,
    /// How long the CPU waited for the decoder: the decoding that drawing did not hide.
    wait_us: u64,
    wait_max: u64,
    draw_us: u64,
    draw_max: u64,
    late: u64,
    warnings: u64,
}

/// What stays from picture to picture.
struct Player {
    framebuffers: [Framebuffer; BUFFERS],
    /// The one to draw into next, and the one shown last, which may still wait for its vsync.
    back: usize,
    pending: Option<usize>,
    mct: Mct,
    /// When the next picture is due, in MCT counts.
    due: u64,
}

impl Player {
    /// Draws the picture the decoder handed out, if any, gives it back, and shows it at its
    /// time. The decoder may be decoding the next picture meanwhile.
    fn show(&mut self, decoder: &mut Decoder, sequence: &Sequence, layout: Layout, decoded: &Decoded, pass: &mut Pass) -> Result<(), display::Error> {
        let Some(frame) = decoded.frame else {
            return Ok(());
        };
        let begin = self.mct.count();
        let framebuffer = &mut self.framebuffers[self.back];
        let (luma, chroma) = decoder.planes(&frame);
        if layout.scale == 0 {
            convert::draw_rotated(luma, chroma, sequence, framebuffer, layout.x, layout.y);
        } else {
            convert::draw(luma, chroma, sequence, framebuffer, layout.x, layout.y, layout.scale);
        }
        framebuffer.flush();
        decoder.release(frame);
        let us = (self.mct.count() - begin) / COUNTS_PER_US;
        pass.draw_us += us;
        pass.draw_max = pass.draw_max.max(us);

        if self.mct.count() > self.due {
            pass.late += 1;
        }
        while self.mct.count() < self.due {
            key();
        }
        self.due = self.due.max(self.mct.count()) + 1_000_000 * COUNTS_PER_US / FPS;
        let fimd = Fimd::new();
        // The last one shown must be on the panel before another takes its place, or it would
        // never be seen; then the buffer after this one is free: neither on the panel nor waiting.
        if let Some(pending) = self.pending {
            fimd.wait_shown(&self.framebuffers[pending])?;
        }
        fimd.show(&self.framebuffers[self.back])?;
        self.pending = Some(self.back);
        self.back = (self.back + 1) % BUFFERS;
        pass.shown += 1;
        Ok(())
    }

    fn account(decoded: &Decoded, pass: &mut Pass) {
        pass.decoded += 1;
        pass.warnings += u64::from(decoded.warning != 0);
    }

    /// Plays the stream once: each picture drawn while the next one decodes.
    fn pass(&mut self, mfc: &mut Mfc, number: u32) -> Result<(), PlayError> {
        let begin = self.mct.count();
        let mut decoder = Decoder::open(mfc, CONFIG)?;
        let sequence = decoder.header(header(STREAM))?;
        let layout = Layout::new(&sequence);
        if number == 1 {
            println!(
                "{}x{}, {} buffers needed, {} made; {:?}",
                sequence.width, sequence.height, sequence.needed, sequence.frames, layout
            );
            console::flush();
        }
        let mut pass = Pass::default();
        let mut pictures = pictures(STREAM);
        let Some(first) = pictures.next() else {
            return Ok(());
        };
        decoder.start(first)?;
        let mut flushes = 0;
        loop {
            let waited = self.mct.count();
            let decoded = decoder.wait()?;
            let waited = (self.mct.count() - waited) / COUNTS_PER_US;
            pass.wait_us += waited;
            pass.wait_max = pass.wait_max.max(waited);
            let finishing = flushes > 0;
            if finishing && decoded.status == regs::DEC_STATUS_DECODING_EMPTY {
                break;
            }
            if !finishing {
                Self::account(&decoded, &mut pass);
            }
            match pictures.next() {
                Some(picture) => decoder.start(picture)?,
                None if flushes < 2 * exynos_4412_mfc::h264::MAX_FRAMES => {
                    flushes += 1;
                    decoder.start_finish()?;
                }
                None => break,
            }
            self.show(&mut decoder, &sequence, layout, &decoded, &mut pass)?;
        }
        drop(decoder);
        let ms = (self.mct.count() - begin) / COUNTS_PER_US / 1000;
        let (decoded, shown) = (pass.decoded.max(1), pass.shown.max(1));
        println!(
            "Pass {}: {}/{} shown, {} ms; wait {}/{} us, draw {}/{} us (avg/max); late {}, warn {}",
            number,
            pass.shown,
            pass.decoded,
            ms,
            pass.wait_us / decoded,
            pass.wait_max,
            pass.draw_us / shown,
            pass.draw_max,
            pass.late,
            pass.warnings
        );
        console::flush();
        Ok(())
    }
}

/// What can stop the playing; printed with `Debug`, which the dead code lint does not count.
#[derive(Debug)]
#[allow(dead_code)]
enum PlayError {
    Mfc(Error),
    Display(display::Error),
}

impl From<Error> for PlayError {
    fn from(error: Error) -> Self {
        Self::Mfc(error)
    }
}

impl From<display::Error> for PlayError {
    fn from(error: display::Error) -> Self {
        Self::Display(error)
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nMFC play");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    if FIRMWARE.is_empty() {
        println!("Built without MFC_FIRMWARE: stopping (MISMATCH)");
        wait_keys();
    }
    println!("Stream {} bytes, {} pictures, {} fps", STREAM.len(), pictures(STREAM).count(), FPS);
    console::flush();
    let mut mfc = match Mfc::init(BANKS, FIRMWARE, mfc::VENDOR_HZ, mct) {
        Ok((mfc, started)) => {
            println!("Firmware {:#08x} running, sclk_mfc {} Hz", started.version.0, started.hz);
            mfc
        }
        Err(error) => {
            println!("MFC ERROR {:?} (MISMATCH)", error);
            wait_keys();
        }
    };
    let framebuffers = match start_panel(mct) {
        Ok(framebuffers) => framebuffers,
        Err(error) => {
            println!("PANEL ERROR {:?} (MISMATCH)", error);
            wait_keys();
        }
    };
    println!("Playing; keys: r restart, f fastboot");
    console::flush();
    let mut player = Player { framebuffers, back: 1, pending: None, mct, due: mct.count() };
    for number in 1.. {
        if let Err(error) = player.pass(&mut mfc, number) {
            println!("PLAY ERROR {:?} (MISMATCH)", error);
            break;
        }
    }
    wait_keys();
}
