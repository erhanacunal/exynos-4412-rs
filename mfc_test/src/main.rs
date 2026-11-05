// SPDX-License-Identifier: GPL-2.0-or-later
//! MFC v5.1 test, through `exynos_4412_mfc`: the codec's power domain and clocks as U-Boot left
//! them, its firmware started, then H.264 decoding of a stream whose pictures are known exactly.
//!
//! The stream, `data/pcm320x240.h264`, comes from `tools/h264_pcm.py`: I_PCM and P_Skip
//! macroblocks only, so a correct decoder gives exactly the pictures drawn, and
//! `data/pcm320x240.crc` holds their CRC-32s, luma and chroma (NV12) as visible. Each picture is
//! detiled from NV12MT and checked. If the first one does not match, the tiles of its luma plane
//! are looked up in `data/pcm320x240.ref0`, the first picture as NV12, to show where they went.
//!
//! The firmware comes from `MFC_FIRMWARE` at build time: see `build.rs`.
//!
//! Keys afterwards: `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

mod crc;

use exynos_4412_mfc::h264::{Config, Decoded, Decoder, Frame, Sequence};
use exynos_4412_mfc::tiles::{Plane, TILE_BYTES, TILE_HEIGHT, TILE_WIDTH};
use exynos_4412_mfc::{Banks, Mfc, regs};
use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mfc;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, print, println};

exynos_4412_rs::entry!(main);

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/s5p-mfc.fw"));
static STREAM: &[u8] = include_bytes!("../data/pcm320x240.h264");
static CRCS: &str = include_str!("../data/pcm320x240.crc");
static REF0: &[u8] = include_bytes!("../data/pcm320x240.ref0");

/// The two ports' memory, sized as Linux reserves it (`exynos-mfc-reserved-memory.dtsi`): 512 MiB
/// into DRAM, far above this program and uart_boot's stage at 0x50000000.
const BANKS: Banks = Banks { a: 0x6000_0000, a_len: 36 << 20, b: 0x6400_0000, b_len: 8 << 20 };

/// The stream has no reordering: each picture can be shown as soon as it is decoded.
const CONFIG: Config = Config { stream_bytes: 1 << 20, extra_frames: 2, display_delay: Some(0) };

fn print_state(when: &str) {
    let clocks = Cmu::new().mfc_clocks();
    println!(
        "{}: domain {:#x}, SRC_MFC {:#x}, DIV_MFC {:#x}, GATE_IP_MFC {:#x}, sclk_mfc {:?} Hz",
        when,
        mfc::power_domain_status(),
        clocks.src,
        clocks.div,
        clocks.gate,
        Cmu::new().sclk_mfc_hz()
    );
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

/// The NAL units of an Annex B stream: each from its start code to the next one.
struct Nals<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Nals<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: Self::next_start(data, 0) }
    }

    /// Where the start code at or after `from` begins, its leading zero byte included.
    fn next_start(data: &[u8], from: usize) -> usize {
        let mut i = from;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                return if i > from && data[i - 1] == 0 { i - 1 } else { i };
            }
            i += 1;
        }
        data.len()
    }
}

impl<'a> Iterator for Nals<'a> {
    /// The unit with its start code, and its type.
    type Item = (&'a [u8], u8);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.data.len() {
            return None;
        }
        let start = self.at;
        let header = start + if self.data[start + 2] == 1 { 3 } else { 4 };
        let end = Self::next_start(self.data, header);
        self.at = end;
        Some((&self.data[start..end], self.data.get(header).map_or(0, |byte| byte & 0x1F)))
    }
}

/// The sequence and picture parameter sets: everything before the first slice.
fn header(stream: &[u8]) -> &[u8] {
    let mut end = 0;
    for (nal, kind) in Nals::new(stream) {
        if kind == 1 || kind == 5 {
            break;
        }
        end = nal.as_ptr() as usize - stream.as_ptr() as usize + nal.len();
    }
    &stream[..end]
}

/// The pictures: here one slice each.
fn pictures(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    Nals::new(stream).filter(|&(_, kind)| kind == 1 || kind == 5).map(|(nal, _)| nal)
}

fn expected(number: usize) -> Option<(u32, u32)> {
    let line = CRCS.lines().nth(number)?;
    let (luma, chroma) = line.split_once(' ')?;
    Some((u32::from_str_radix(luma, 16).ok()?, u32::from_str_radix(chroma.trim(), 16).ok()?))
}

/// The CRC-32s of a picture's planes, detiled, as visible.
fn crcs(decoder: &Decoder, sequence: &Sequence, frame: &Frame) -> (u32, u32) {
    let (luma, chroma) = decoder.planes(frame);
    let luma_plane = Plane::new(sequence.buf_width, sequence.buf_height);
    let chroma_plane = Plane::new(sequence.buf_width, sequence.chroma_height);
    let mut y_crc = crc::START;
    for y in 0..sequence.height {
        luma_plane.row(luma, y, sequence.width, |piece| y_crc = crc::update(y_crc, piece));
    }
    let mut c_crc = crc::START;
    for y in 0..sequence.height / 2 {
        chroma_plane.row(chroma, y, sequence.width, |piece| c_crc = crc::update(c_crc, piece));
    }
    (crc::finish(y_crc), crc::finish(c_crc))
}

/// For a first picture that does not match: where each tile in memory is in the picture, found
/// by its first row among the whole tiles of `REF0`; `-` where none matches.
fn diagnose(decoder: &Decoder, sequence: &Sequence, frame: &Frame) {
    let (luma, _) = decoder.planes(frame);
    let plane = Plane::new(sequence.buf_width, sequence.buf_height);
    let columns = sequence.width / TILE_WIDTH;
    let rows = sequence.height / TILE_HEIGHT;
    println!("Luma bytes 0..8 {:02x?}, expected {:02x?}", &luma[..8], &REF0[..8]);
    println!("Tiles in memory -> (column,row) in the picture; formula's tile 0 is {}", plane.tile_index(0, 0));
    for k in 0..plane.columns * plane.rows {
        let first_row = &luma[k * TILE_BYTES..k * TILE_BYTES + TILE_WIDTH];
        let place = (0..rows).flat_map(|ty| (0..columns).map(move |tx| (tx, ty))).find(|&(tx, ty)| {
            let start = ty * TILE_HEIGHT * sequence.width + tx * TILE_WIDTH;
            &REF0[start..start + TILE_WIDTH] == first_row
        });
        match place {
            Some((tx, ty)) => print!("{}:{},{} ", k, tx, ty),
            None => print!("{}:- ", k),
        }
        if k % 12 == 11 {
            println!();
        }
    }
    println!();
}

/// Counts of the run.
#[derive(Default)]
struct Tally {
    shown: usize,
    matched: usize,
    decode_us: u64,
    decodes: usize,
}

/// Checks a picture the decoder handed out, as picture `tally.shown`, and gives it back.
fn take(decoder: &mut Decoder, sequence: &Sequence, decoded: &Decoded, tally: &mut Tally) {
    let Some(frame) = decoded.frame else {
        return;
    };
    let number = tally.shown;
    let got = crcs(decoder, sequence, &frame);
    let ok = expected(number) == Some(got);
    println!(
        "  pic {:2}: buf {:2}, type {}, {:5} us, Y {:08x} C {:08x} {}{}",
        number,
        frame.index,
        decoded.frame_type,
        decoded.us,
        got.0,
        got.1,
        if ok { "OK" } else { "MISMATCH" },
        if decoded.warning != 0 { " (warning)" } else { "" },
    );
    if !ok && number == 0 {
        diagnose(decoder, sequence, &frame);
    }
    tally.shown += 1;
    tally.matched += usize::from(ok);
    decoder.release(frame);
}

fn decode_all(mfc: &mut Mfc) -> Result<Tally, exynos_4412_mfc::Error> {
    let mut decoder = Decoder::open(mfc, CONFIG)?;
    let header = header(STREAM);
    let sequence = decoder.header(header)?;
    println!(
        "Instance {}: {}x{}, {} buffers needed, {} made ({}x{} luma), header {} of {} bytes used",
        decoder.instance(),
        sequence.width,
        sequence.height,
        sequence.needed,
        sequence.frames,
        sequence.buf_width,
        sequence.buf_height,
        sequence.consumed,
        header.len()
    );
    console::flush();
    let mut tally = Tally::default();
    for (i, picture) in pictures(&STREAM[header.len()..]).enumerate() {
        let decoded = decoder.decode(picture).inspect_err(|_| println!("  at picture {} ({} bytes)", i, picture.len()))?;
        if decoded.frame.is_none() {
            println!("  in {:2}: status {}, type {}, {} us", i, decoded.status, decoded.frame_type, decoded.us);
        }
        tally.decode_us += decoded.us;
        tally.decodes += 1;
        take(&mut decoder, &sequence, &decoded, &mut tally);
        console::flush();
    }
    for _ in 0..MAX_FLUSH {
        let decoded = decoder.finish()?;
        if decoded.status == regs::DEC_STATUS_DECODING_EMPTY {
            break;
        }
        take(&mut decoder, &sequence, &decoded, &mut tally);
    }
    Ok(tally)
}

/// More than the frame buffers there can be.
const MAX_FLUSH: usize = 40;

fn main(_args: BootArgs) -> ! {
    println!("\nMFC v5.1 test: H.264 decode");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    print_state("As U-Boot left it");
    if FIRMWARE.is_empty() {
        println!("Built without MFC_FIRMWARE: stopping (MISMATCH)");
        wait_keys();
    }
    println!("Firmware {} bytes; port A {:#x}, port B {:#x}", FIRMWARE.len(), BANKS.a, BANKS.b);
    console::flush();
    let mut mfc = match Mfc::init(BANKS, FIRMWARE, mfc::VENDOR_HZ, mct) {
        Ok((mfc, started)) => {
            let (year, month, day) = started.version.date();
            println!(
                "Firmware running: version {:#08x} (20{:02x}-{:02x}-{:02x}); sclk_mfc {} Hz",
                started.version.0, year, month, day, started.hz
            );
            println!("  start {} us, SYS_INIT {} us: OK", started.start_us, started.init_us);
            mfc
        }
        Err(error) => {
            println!("MFC ERROR {:?} (MISMATCH)", error);
            wait_keys();
        }
    };
    let pictures = pictures(STREAM).count();
    println!("Stream: {} bytes, {} pictures", STREAM.len(), pictures);
    console::flush();
    match decode_all(&mut mfc) {
        Ok(tally) => {
            let average = if tally.decodes > 0 { tally.decode_us / tally.decodes as u64 } else { 0 };
            let all = tally.shown == pictures && tally.matched == pictures;
            println!(
                "Decoded {} pictures, {} shown, {} match, {} us each on average: {}",
                tally.decodes,
                tally.shown,
                tally.matched,
                average,
                if all { "OK" } else { "MISMATCH" }
            );
        }
        Err(error) => println!("DECODE ERROR {:?} (MISMATCH)", error),
    }
    let (a, b) = mfc.free();
    println!("Free after closing: port A {} KiB, port B {} KiB", a >> 10, b >> 10);
    print_state("After");
    wait_keys();
}
