// SPDX-License-Identifier: GPL-2.0-only
//! JPEG codec test, through `exynos_4412_jpeg`.
//!
//! Decoding: four baseline JPEGs of one 320x240 picture, in 4:2:0, 4:2:2, 4:4:4 and grey
//! (`data/`, from `tools/jpeg_testdata.py`), each into all twelve raw formats. Each result is
//! turned back into RGB by the CPU and compared with the RGB libjpeg decodes the same file to:
//! the codec's IDCT and chroma upsampling differ from libjpeg's, so the check is a PSNR of at
//! least [`MIN_PSNR`] dB, not equality. Formats the driver refuses for a stream are listed as
//! such. Which way round the codec puts R and B in RGB32 and RGB565 is found here: both are
//! tried, the closer one is printed.
//!
//! Encoding: that picture, as libjpeg decoded the 4:4:4 file, laid out by the CPU in each raw
//! format, encoded, then decoded again by the codec and compared with it.
//!
//! Then the panel shows the picture as decoded (left) and after a round trip through the
//! encoder (right), twice their size. Keys: `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_jpeg::{Error, Format, Jpeg, Layout, Picture, Quality, Subsampling, encode_bound, header};
use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS, Timing};
use exynos_4412_rs::drivers::jpeg;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

const WIDTH: usize = 320;
const HEIGHT: usize = 240;

/// Name, JPEG file, libjpeg's RGB of it.
static STREAMS: [(&str, &[u8], &[u8]); 4] = [
    ("4:2:0", include_bytes!("../data/test420.jpg"), include_bytes!("../data/test420.rgb")),
    ("4:2:2", include_bytes!("../data/test422.jpg"), include_bytes!("../data/test422.rgb")),
    ("4:4:4", include_bytes!("../data/test444.jpg"), include_bytes!("../data/test444.rgb")),
    ("grey", include_bytes!("../data/testgrey.jpg"), include_bytes!("../data/testgrey.rgb")),
];
/// The picture the encoder is given: libjpeg's decoding of the 4:4:4 file.
const SOURCE: usize = 2;

const FORMATS: [Format; 12] = [
    Format::Rgb32,
    Format::Rgb565,
    Format::Yuyv,
    Format::Yvyu,
    Format::Nv16,
    Format::Nv61,
    Format::Nv12,
    Format::Nv21,
    Format::Yuv420,
    Format::Nv24,
    Format::Nv42,
    Format::Grey,
];

/// Raw format and subsampling the encoder is tried with, at [`Quality::HIGHEST`].
const ENCODES: [(Format, Subsampling); 12] = [
    (Format::Rgb32, Subsampling::S420),
    (Format::Rgb32, Subsampling::S422),
    (Format::Rgb32, Subsampling::S444),
    (Format::Rgb565, Subsampling::S420),
    (Format::Yuyv, Subsampling::S422),
    (Format::Yvyu, Subsampling::S422),
    (Format::Nv16, Subsampling::S422),
    (Format::Nv12, Subsampling::S420),
    (Format::Nv21, Subsampling::S420),
    (Format::Yuv420, Subsampling::S420),
    (Format::Nv24, Subsampling::S444),
    (Format::Grey, Subsampling::Gray),
];

/// The least PSNR that passes, in dB.
const MIN_PSNR: f64 = 30.0;
/// The same for the lowest quality, whose tables are coarse.
const MIN_PSNR_LOWEST: f64 = 25.0;

/// The codec's buffers: 768 MiB into DRAM, clear of this program, of uart_boot's stage at
/// 0x50000000 and of the MFC's ports at 0x60000000.
const WORK: usize = 0x7000_0000;
const WORK_LEN: usize = 16 << 20;

const PANEL: Timing = LTN101AL03;
const PIXELS: usize = PANEL.width * PANEL.height;
const SCALE: usize = 2;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

/// The framebuffer, in `.bss`, inside the first 16 MiB of DRAM that the uncached view covers.
#[repr(align(4096))]
struct Pixels(UnsafeCell<[u32; PIXELS]>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Pixels {}

static PIXEL_STORE: Pixels = Pixels(UnsafeCell::new([0; PIXELS]));

/// Page-aligned pieces of [`WORK`], each handed out once.
struct Arena {
    next: usize,
}

impl Arena {
    fn take(&mut self, len: usize) -> &'static mut [u8] {
        let start = self.next;
        self.next = (start + len).next_multiple_of(4096);
        assert!(self.next <= WORK + WORK_LEN, "WORK is full");
        // SAFETY: DRAM that nothing else in this program uses, identity-mapped and cached, and
        // each range is handed out once.
        unsafe { core::slice::from_raw_parts_mut(start as *mut u8, len) }
    }

    fn copy(&mut self, data: &[u8]) -> &'static mut [u8] {
        let buffer = self.take(data.len());
        buffer.copy_from_slice(data);
        buffer
    }
}

/// Which of R and B the codec puts in the high bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    /// RGB32 as `0x00RRGGBB` (the panel's), RGB565 as R in bits 11-15.
    Rgb,
    /// The other way round.
    Bgr,
}

fn wait_keys() -> ! {
    println!("Keys: r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    loop {
        match uart.read_byte() {
            b'r' => power::restart(BootMode::Normal),
            b'f' => power::restart(BootMode::Fastboot),
            _ => {}
        }
    }
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// JFIF's YCbCr to RGB, in 16.16 fixed point.
fn ycc_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let (y, cb, cr) = (i32::from(y) << 16, i32::from(cb) - 128, i32::from(cr) - 128);
    let half = 1 << 15;
    [
        clamp((y + 91_881 * cr + half) >> 16),
        clamp((y - 22_554 * cb - 46_802 * cr + half) >> 16),
        clamp((y + 116_130 * cb + half) >> 16),
    ]
}

/// JFIF's RGB to YCbCr.
fn rgb_to_ycc([r, g, b]: [u8; 3]) -> [u8; 3] {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let half = 1 << 15;
    [
        clamp((19_595 * r + 38_470 * g + 7_471 * b + half) >> 16),
        clamp(((-11_059 * r - 21_709 * g + 32_768 * b + half) >> 16) + 128),
        clamp(((32_768 * r - 27_439 * g - 5_329 * b + half) >> 16) + 128),
    ]
}

fn luma(rgb: [u8; 3]) -> [u8; 3] {
    let y = rgb_to_ycc(rgb)[0];
    [y, y, y]
}

/// Pixel (`x`, `y`) of a raw picture, as RGB.
fn sample(format: Format, layout: &Layout, data: &[u8], order: Order, x: usize, y: usize) -> [u8; 3] {
    let w = layout.width;
    let cb = layout.cb.unwrap_or(0);
    let pair = |at: usize, swap: bool| if swap { (data[at + 1], data[at]) } else { (data[at], data[at + 1]) };
    let swap = matches!(format, Format::Yvyu | Format::Nv61 | Format::Nv21 | Format::Nv42);
    let luma = data[y * w + x];
    let (u, v) = match format {
        Format::Rgb32 => {
            let p = u32::from_le_bytes(data[4 * (y * w + x)..][..4].try_into().unwrap());
            let (r, g, b) = ((p >> 16) as u8, (p >> 8) as u8, p as u8);
            return if order == Order::Rgb { [r, g, b] } else { [b, g, r] };
        }
        Format::Rgb565 => {
            let p = u16::from_le_bytes(data[2 * (y * w + x)..][..2].try_into().unwrap());
            let five = |v: u16| ((v << 3) | (v >> 2)) as u8;
            let (hi, g, lo) = (five(p >> 11), (((p >> 5) & 63) << 2 | ((p >> 9) & 3)) as u8, five(p & 31));
            return if order == Order::Rgb { [hi, g, lo] } else { [lo, g, hi] };
        }
        Format::Grey => return [luma, luma, luma],
        Format::Yuyv | Format::Yvyu => {
            let at = 2 * (y * w + (x & !1));
            let (u, v) = (data[at + 1], data[at + 3]);
            return ycc_to_rgb(data[at + 2 * (x & 1)], if swap { v } else { u }, if swap { u } else { v });
        }
        Format::Nv16 | Format::Nv61 => pair(cb + y * w + (x & !1), swap),
        Format::Nv12 | Format::Nv21 => pair(cb + (y / 2) * w + (x & !1), swap),
        Format::Yuv420 => {
            let at = (y / 2) * (w / 2) + x / 2;
            (data[cb + at], data[layout.cr.unwrap_or(0) + at])
        }
        Format::Nv24 | Format::Nv42 => pair(cb + 2 * (y * w + x), swap),
    };
    ycc_to_rgb(luma, u, v)
}

fn pixel(rgb: &[u8], x: usize, y: usize) -> [u8; 3] {
    let at = 3 * (y * WIDTH + x);
    [rgb[at], rgb[at + 1], rgb[at + 2]]
}

/// Lays `rgb` out in `format` into `data`: chroma of each 2x2 or 2x1 block averaged.
fn fill(format: Format, layout: &Layout, data: &mut [u8], rgb: &[u8], order: Order) {
    let w = layout.width;
    let cb = layout.cb.unwrap_or(0);
    let swap = matches!(format, Format::Yvyu | Format::Nv61 | Format::Nv21 | Format::Nv42);
    let (h_step, v_step) = match format.subsampling() {
        Subsampling::S420 => (2, 2),
        Subsampling::S422 => (2, 1),
        _ => (1, 1),
    };
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let p = pixel(rgb, x, y);
            match format {
                Format::Rgb32 => {
                    let [r, g, b] = if order == Order::Rgb { p } else { [p[2], p[1], p[0]] };
                    data[4 * (y * w + x)..][..4].copy_from_slice(&display::rgb(r, g, b).to_le_bytes());
                }
                Format::Rgb565 => {
                    let [hi, g, lo] = if order == Order::Rgb { p } else { [p[2], p[1], p[0]] };
                    let value = u16::from(hi >> 3) << 11 | u16::from(g >> 2) << 5 | u16::from(lo >> 3);
                    data[2 * (y * w + x)..][..2].copy_from_slice(&value.to_le_bytes());
                }
                Format::Yuyv | Format::Yvyu => data[2 * (y * w + x)] = rgb_to_ycc(p)[0],
                _ => data[y * w + x] = rgb_to_ycc(p)[0],
            }
        }
    }
    if format.subsampling() == Subsampling::Gray || matches!(format, Format::Rgb32 | Format::Rgb565) {
        return;
    }
    for by in (0..HEIGHT).step_by(v_step) {
        for bx in (0..WIDTH).step_by(h_step) {
            let (mut u, mut v) = (0u32, 0u32);
            for y in by..by + v_step {
                for x in bx..bx + h_step {
                    let [_, cb, cr] = rgb_to_ycc(pixel(rgb, x, y));
                    u += u32::from(cb);
                    v += u32::from(cr);
                }
            }
            let n = (h_step * v_step) as u32;
            let (u, v) = (((u + n / 2) / n) as u8, ((v + n / 2) / n) as u8);
            let (first, second) = if swap { (v, u) } else { (u, v) };
            match format {
                Format::Yuyv | Format::Yvyu => {
                    let at = 2 * (by * w + bx);
                    data[at + 1] = first;
                    data[at + 3] = second;
                }
                Format::Yuv420 => {
                    let at = (by / 2) * (w / 2) + bx / 2;
                    data[cb + at] = u;
                    data[layout.cr.unwrap_or(0) + at] = v;
                }
                Format::Nv12 | Format::Nv21 => {
                    let at = cb + (by / 2) * w + bx;
                    data[at] = first;
                    data[at + 1] = second;
                }
                Format::Nv16 | Format::Nv61 => {
                    let at = cb + by * w + bx;
                    data[at] = first;
                    data[at + 1] = second;
                }
                _ => {
                    let at = cb + 2 * (by * w + bx);
                    data[at] = first;
                    data[at + 1] = second;
                }
            }
        }
    }
}

/// Mean squared error of a raw picture against `reference`, over R, G and B; against its luma
/// for grey.
fn mse(format: Format, layout: &Layout, data: &[u8], order: Order, reference: &[u8]) -> f64 {
    let mut sum = 0u64;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let got = sample(format, layout, data, order, x, y);
            let want = if format == Format::Grey { luma(pixel(reference, x, y)) } else { pixel(reference, x, y) };
            for c in 0..3 {
                let d = i64::from(got[c]) - i64::from(want[c]);
                sum += (d * d) as u64;
            }
        }
    }
    sum as f64 / (3 * WIDTH * HEIGHT) as f64
}

/// Natural logarithm of a positive normal number: exponent, and an atanh series for the
/// mantissa in [1, 2).
fn ln(x: f64) -> f64 {
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7FF) as i64 - 1023;
    let mantissa = f64::from_bits((bits & !(0x7FF << 52)) | (1023 << 52));
    let t = (mantissa - 1.0) / (mantissa + 1.0);
    let (t2, mut term, mut sum) = (t * t, t, 0.0);
    for k in 0..12 {
        sum += term / f64::from(2 * k + 1);
        term *= t2;
    }
    2.0 * sum + exponent as f64 * core::f64::consts::LN_2
}

fn psnr(mse: f64) -> f64 {
    if mse <= 0.0 { 99.0 } else { 10.0 * ln(255.0 * 255.0 / mse) / core::f64::consts::LN_10 }
}

/// PSNR, and the order of R and B that gives it.
fn compare(format: Format, layout: &Layout, data: &[u8], reference: &[u8]) -> (f64, Order) {
    let rgb = mse(format, layout, data, Order::Rgb, reference);
    if !matches!(format, Format::Rgb32 | Format::Rgb565) {
        return (psnr(rgb), Order::Rgb);
    }
    let bgr = mse(format, layout, data, Order::Bgr, reference);
    if bgr < rgb { (psnr(bgr), Order::Bgr) } else { (psnr(rgb), Order::Rgb) }
}

#[derive(Default)]
struct Tally {
    ok: usize,
    bad: usize,
    refused: usize,
}

impl Tally {
    fn count(&mut self, ok: bool) -> &'static str {
        if ok {
            self.ok += 1;
            "OK"
        } else {
            self.bad += 1;
            "MISMATCH"
        }
    }
}

fn decode_all(codec: &mut Jpeg, arena: &mut Arena, order32: &mut Order) -> Tally {
    let mut tally = Tally::default();
    let out = arena.take((WIDTH * HEIGHT * 4).next_multiple_of(mmu::DCACHE_LINE));
    for (index, &(name, file, reference)) in STREAMS.iter().enumerate() {
        let stream = arena.copy(file);
        match header::parse(stream) {
            Ok(h) => println!("{} file, {} B: {}x{} {:?}, {} comp., SOS at {}", name, file.len(), h.width, h.height, h.subsampling, h.components, h.sos),
            Err(error) => println!("{} file: header {:?}", name, error),
        }
        for format in FORMATS {
            match codec.decode(stream, format, out) {
                Ok(decoded) => {
                    let (db, order) = compare(format, &decoded.layout, out, reference);
                    if index == 0 && format == Format::Rgb32 {
                        *order32 = order;
                    }
                    let size_ok = (decoded.decoded_width, decoded.decoded_height) == (WIDTH, HEIGHT);
                    let rb = if matches!(format, Format::Rgb32 | Format::Rgb565) { if order == Order::Rgb { " RGB" } else { " BGR" } } else { "" };
                    println!(
                        "  {:6} {:4} us, hw {}x{} {:?}, PSNR {:.1} dB{} {}",
                        format_name(format),
                        decoded.us,
                        decoded.decoded_width,
                        decoded.decoded_height,
                        decoded.decoded_subsampling,
                        db,
                        rb,
                        tally.count(db >= MIN_PSNR && size_ok)
                    );
                }
                Err(Error::Unsupported { .. }) => {
                    tally.refused += 1;
                    println!("  {:6} refused, as the Linux driver does", format_name(format));
                }
                Err(error) => println!("  {:6} {:?} {}", format_name(format), error, tally.count(false)),
            }
            console::flush();
        }
    }
    tally
}

fn format_name(format: Format) -> &'static str {
    match format {
        Format::Rgb32 => "RGB32",
        Format::Rgb565 => "RGB565",
        Format::Yuyv => "YUYV",
        Format::Yvyu => "YVYU",
        Format::Nv16 => "NV16",
        Format::Nv61 => "NV61",
        Format::Nv12 => "NV12",
        Format::Nv21 => "NV21",
        Format::Yuv420 => "YUV420",
        Format::Nv24 => "NV24",
        Format::Nv42 => "NV42",
        Format::Grey => "GREY",
    }
}

/// Encodes the source picture laid out as `format`, checks the file's ends and header,
/// decodes it to RGB32 and compares. Returns the PSNR.
#[allow(clippy::too_many_arguments)]
fn round_trip(
    codec: &mut Jpeg,
    raw: &mut [u8],
    jpg: &mut [u8],
    back: &mut [u8],
    format: Format,
    subsampling: Subsampling,
    quality: Quality,
    order: Order,
) -> Result<(usize, u64, f64), &'static str> {
    let source = STREAMS[SOURCE].2;
    let layout = format.layout(WIDTH, HEIGHT);
    fill(format, &layout, raw, source, order);
    let encoded = codec.encode(&Picture { data: raw, format, width: WIDTH, height: HEIGHT }, subsampling, quality, jpg).map_err(|error| {
        println!("    {:?}", error);
        "encode error"
    })?;
    let file = &jpg[..encoded.len];
    if encoded.len < 4 || file[..2] != [0xFF, 0xD8] || file[encoded.len - 2..] != [0xFF, 0xD9] {
        println!("    {} B, starts {:02x?}, ends {:02x?}", encoded.len, &jpg[..4], &file[file.len().saturating_sub(2)..]);
        return Err("no SOI/EOI");
    }
    let expected = if format == Format::Grey {
        Subsampling::Gray
    } else if matches!(format, Format::Rgb32 | Format::Rgb565) {
        subsampling
    } else {
        subsampling.max(format.subsampling())
    };
    match header::parse(file) {
        Ok(h) if (h.width, h.height, h.subsampling) == (WIDTH, HEIGHT, expected) => {}
        Ok(h) => {
            println!("    header {}x{} {:?}, expected {:?}", h.width, h.height, h.subsampling, expected);
            return Err("header");
        }
        Err(error) => {
            println!("    header {:?}", error);
            return Err("header");
        }
    }
    let decoded = codec.decode(file, Format::Rgb32, back).map_err(|error| {
        println!("    decoding it: {:?}", error);
        "decode error"
    })?;
    let reference_is_grey = format == Format::Grey;
    let db = if reference_is_grey {
        // The grey file decodes to grey RGB: compare with the source's luma.
        let mut sum = 0u64;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let got = sample(Format::Rgb32, &decoded.layout, back, order, x, y);
                let want = luma(pixel(source, x, y));
                for c in 0..3 {
                    let d = i64::from(got[c]) - i64::from(want[c]);
                    sum += (d * d) as u64;
                }
            }
        }
        psnr(sum as f64 / (3 * WIDTH * HEIGHT) as f64)
    } else {
        psnr(mse(Format::Rgb32, &decoded.layout, back, order, source))
    };
    Ok((encoded.len, encoded.us, db))
}

fn encode_all(codec: &mut Jpeg, arena: &mut Arena, order: Order) -> (Tally, &'static [u8]) {
    let mut tally = Tally::default();
    let raw = arena.take(WIDTH * HEIGHT * 4);
    let jpg = arena.take(encode_bound(WIDTH, HEIGHT));
    let back = arena.take(WIDTH * HEIGHT * 4);
    let shown = arena.take(WIDTH * HEIGHT * 4);
    println!("Encoding the 4:4:4 file's picture, quality 3, decoding it back to RGB32:");
    for (format, subsampling) in ENCODES {
        match round_trip(codec, raw, jpg, back, format, subsampling, Quality::HIGHEST, order) {
            Ok((len, us, db)) => println!(
                "  {:6} -> {:?}: {:6} B in {:4} us, PSNR {:.1} dB {}",
                format_name(format),
                subsampling,
                len,
                us,
                db,
                tally.count(db >= MIN_PSNR)
            ),
            Err(what) => println!("  {:6} -> {:?}: {} {}", format_name(format), subsampling, what, tally.count(false)),
        }
        if (format, subsampling) == (Format::Rgb32, Subsampling::S420) {
            shown.copy_from_slice(back);
        }
        console::flush();
    }
    println!("RGB32 -> 4:2:0 by quality:");
    for level in 0..=3 {
        let quality = Quality::new(level).unwrap();
        match round_trip(codec, raw, jpg, back, Format::Rgb32, Subsampling::S420, quality, order) {
            Ok((len, us, db)) => println!(
                "  q{}: {:6} B in {:4} us, PSNR {:.1} dB {}",
                level,
                len,
                us,
                db,
                tally.count(db >= if level == 0 { MIN_PSNR_LOWEST } else { MIN_PSNR })
            ),
            Err(what) => println!("  q{}: {} {}", level, what, tally.count(false)),
        }
    }
    (tally, shown)
}

/// Powers the panel on, black.
fn start_panel(mct: Mct) -> Result<Framebuffer, display::Error> {
    let fimd = Fimd::new();
    fimd.power_on()?;
    display::configure_pins();
    // SAFETY: the only reference to this buffer, taken once: see `Pixels`.
    let pixels: &'static mut [u32] = unsafe { &mut *PIXEL_STORE.0.get() };
    let mut framebuffer = Framebuffer::new(pixels, PANEL.width, PANEL.height).ok_or(display::Error::BadSize)?;
    framebuffer.fill_rect(0, 0, PANEL.width, PANEL.height, 0);
    framebuffer.flush();
    fimd.start(&PANEL, &framebuffer)?;
    display::set_lvds(true);
    let end = mct.count() + u64::from(LVDS_SETTLE_MS) * 1000 * COUNTS_PER_US;
    while mct.count() < end {}
    display::set_backlight(true);
    Ok(framebuffer)
}

/// An RGB32 picture from the codec, [`SCALE`] times its size, with its top left at (`left`, `top`).
fn draw(framebuffer: &mut Framebuffer, picture: &[u8], order: Order, left: usize, top: usize) {
    let layout = Format::Rgb32.layout(WIDTH, HEIGHT);
    let mut line = [0u32; WIDTH * SCALE];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let [r, g, b] = sample(Format::Rgb32, &layout, picture, order, x, y);
            line[x * SCALE..][..SCALE].fill(display::rgb(r, g, b));
        }
        for row in 0..SCALE {
            framebuffer.write_span(left, top + y * SCALE + row, &line);
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nJPEG codec test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    let cmu = Cmu::new();
    println!(
        "As U-Boot left it: CAM domain {:#x}, GATE_IP_CAM {:#x}, aclk160 {:?} Hz, accessible {}",
        jpeg::power_domain_status(),
        cmu.cam_gates(),
        cmu.aclk160_hz(),
        jpeg::is_accessible()
    );
    let mut codec = match Jpeg::init(mct) {
        Ok(codec) => codec,
        Err(error) => {
            println!("INIT ERROR {:?} (MISMATCH)", error);
            wait_keys();
        }
    };
    println!("On: CAM domain {:#x}, GATE_IP_CAM {:#x}, aclk160 {:?} Hz", jpeg::power_domain_status(), cmu.cam_gates(), codec.aclk160_hz());
    console::flush();

    let mut arena = Arena { next: WORK };
    let mut order = Order::Rgb;
    let decoded = decode_all(&mut codec, &mut arena, &mut order);
    println!("Decoding: {} OK, {} MISMATCH, {} refused; RGB32 is {:?}", decoded.ok, decoded.bad, decoded.refused, order);
    console::flush();
    let (encoded, round) = encode_all(&mut codec, &mut arena, order);
    let all = decoded.bad == 0 && encoded.bad == 0;
    println!(
        "Encoding: {} OK, {} MISMATCH. All: {}",
        encoded.ok,
        encoded.bad,
        if all { "OK" } else { "MISMATCH" }
    );
    console::flush();

    let mut framebuffer = match start_panel(mct) {
        Ok(framebuffer) => framebuffer,
        Err(error) => {
            println!("PANEL ERROR {:?}", error);
            wait_keys();
        }
    };
    let first = arena.take((WIDTH * HEIGHT * 4).next_multiple_of(mmu::DCACHE_LINE));
    match codec.decode(arena.copy(STREAMS[0].1), Format::Rgb32, first) {
        Ok(_) => {
            let top = (PANEL.height - HEIGHT * SCALE) / 2;
            draw(&mut framebuffer, first, order, 0, top);
            draw(&mut framebuffer, round, order, PANEL.width - WIDTH * SCALE, top);
            framebuffer.flush();
            println!("Panel: left the 4:2:0 file decoded, right RGB32 -> 4:2:0 q3 -> RGB32");
        }
        Err(error) => println!("Decoding for the panel: {:?}", error),
    }
    wait_keys();
}
