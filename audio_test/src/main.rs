//! WM8758 and I2S0 test: a tone on the headphones, the FIFO filled by the CPU.
//!
//! 1. Turns on the codec's rail and checks that it answers on `i2c_1`.
//! 2. Runs the EPLL at 196.608 MHz and the audio subsystem's dividers, then I2S0 as clock
//!    master, sending silence. Counts the frames it takes in 1 s against the MCT: the frame
//!    rate, 48000 Hz if the clocks are right.
//! 3. Starts the codec's headphone path at -30 dB, and plays 1 kHz at -12 dBFS.
//!
//! The console is buffered, so that a report does not hold the CPU for the 7 ms a polled
//! line takes; the FIFO holds 1.3 ms. Every 5 s: the frame rate, underruns and the jack.
//!
//! Keys: `+` and `-` the volume by 3 dB, up to 0 dB; `t` the next tone of 250, 500, 1000
//! and 2000 Hz; `l`, `r`, `b` the left channel, the right one, both; `s` silence on and off;
//! `1` to `9` play the embedded WAV samples, if the build found them (see `build.rs`): a
//! mono one named `Left` or `Right` in that channel only, others in both;
//! `m` the codec's MCLK divider, /2 or /1; `o` the speakers on and off, at no more than
//! -30 dB; `q` stops the codec and I2S0. The volume keys set the headphones and speakers
//! alike.

#![no_std]
#![no_main]

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::i2s::{self, I2s};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::wm8758::{self, MclkDiv, Wm8758};
use exynos_4412_rs::embassy::uart::buffer_console;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::wav::Wav;
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println, soc};

include!(concat!(env!("OUT_DIR"), "/samples.rs"));

exynos_4412_rs::entry!(main);

const START_DB: i8 = -30;
/// The most this test sets, well below the codec's +6 dB.
const MAX_DB: i8 = 0;
const STEP_DB: i8 = 3;
/// -12 dBFS.
const AMPLITUDE: f64 = 8192.0;
const TABLE_BITS: u32 = 10;
const TABLE_LEN: usize = 1 << TABLE_BITS;
const TONES_HZ: [u32; 4] = [250, 500, 1000, 2000];
const FIRST_TONE: usize = 2;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const MEASURE_US: u64 = 1_000_000;
const REPORT_US: u64 = 5_000_000;

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

fn verdict(ok: bool) -> &'static str {
    if ok { "OK" } else { "MISMATCH" }
}

/// One period of a sine at [`AMPLITUDE`], by rotating a vector `TABLE_LEN` times.
fn sine_table() -> [i16; TABLE_LEN] {
    // cos and sin of 2 pi / 1024.
    const COS: f64 = 0.999_981_175_282_601_1;
    const SIN: f64 = 0.006_135_884_649_154_475;
    const _: () = assert!(TABLE_LEN == 1024);
    let mut table = [0; TABLE_LEN];
    let (mut c, mut s) = (1.0f64, 0.0f64);
    for value in table.iter_mut() {
        let rounded = if s >= 0.0 { s * AMPLITUDE + 0.5 } else { s * AMPLITUDE - 0.5 };
        *value = rounded as i16;
        (c, s) = (c * COS - s * SIN, s * COS + c * SIN);
    }
    table
}

/// A sample being played.
struct Playing {
    wav: Wav<'static>,
    frame: usize,
    left: bool,
    right: bool,
}

impl Playing {
    fn start(name: &str, file: &'static [u8]) -> Result<Self, &'static str> {
        let wav = Wav::parse(file).map_err(|_| "not a PCM WAV file")?;
        if wav.bits != 16 || wav.sample_rate != i2s::SAMPLE_RATE_HZ || wav.channels > 2 {
            return Err("not 16-bit mono or stereo at 48 kHz");
        }
        let mono_side = wav.channels == 1 && (name.contains("Left") || name.contains("Right"));
        let left = !mono_side || name.contains("Left");
        let right = !mono_side || name.contains("Right");
        Ok(Self { wav, frame: 0, left, right })
    }

    /// The next frame, `None` at the end.
    fn next(&mut self) -> Option<u32> {
        let left = self.wav.sample_i16(self.frame, 0)?;
        let right = self.wav.sample_i16(self.frame, 1).unwrap_or(left);
        self.frame += 1;
        Some(i2s::frame(if self.left { left } else { 0 }, if self.right { right } else { 0 }))
    }
}

struct Tone {
    sample: Option<Playing>,
    /// Set when a sample has ended, for the main loop to report.
    finished: bool,
    table: [i16; TABLE_LEN],
    phase: u32,
    step: u32,
    index: usize,
    left: bool,
    right: bool,
    silent: bool,
}

impl Tone {
    fn set(&mut self, index: usize) {
        self.index = index;
        self.step = ((u64::from(TONES_HZ[index]) << 32) / u64::from(i2s::SAMPLE_RATE_HZ)) as u32;
    }

    fn frame(&self) -> u32 {
        let sample = if self.silent { 0 } else { self.table[(self.phase >> (32 - TABLE_BITS)) as usize] };
        i2s::frame(if self.left { sample } else { 0 }, if self.right { sample } else { 0 })
    }

    /// Fills the FIFO, from the sample if one plays, else with the tone. Returns the frames
    /// added.
    fn pump(&mut self, i2s: I2s) -> u64 {
        let mut frames = 0;
        loop {
            if let Some(playing) = &mut self.sample {
                let at = playing.frame;
                match playing.next() {
                    Some(frame) => {
                        if !i2s.try_write(frame) {
                            playing.frame = at;
                            return frames;
                        }
                    }
                    None => {
                        self.sample = None;
                        self.silent = true;
                        self.finished = true;
                        continue;
                    }
                }
            } else {
                if !i2s.try_write(self.frame()) {
                    return frames;
                }
                self.phase = self.phase.wrapping_add(self.step);
            }
            frames += 1;
        }
    }

    fn describe(&self) -> (&'static str, u32) {
        let channels = match (self.silent, self.left, self.right) {
            (true, _, _) => "silence",
            (false, true, true) => "both",
            (false, true, false) => "left",
            _ => "right",
        };
        (channels, TONES_HZ[self.index])
    }
}

/// The rate `frames` in `us` give, in hundredths of Hz.
fn centihertz(frames: u64, us: u64) -> u64 {
    frames * 100_000_000 / us.max(1)
}

fn print_rate(label: &str, frames: u64, us: u64) -> u64 {
    let rate = centihertz(frames, us);
    println!("{}: {} frames in {} us, {}.{:02} Hz", label, frames, us, rate / 100, rate % 100);
    rate
}

fn halt() -> ! {
    console::flush();
    cpu::halt();
}

fn codec(pmic: Max77686, mct: Mct) -> Wm8758 {
    match wm8758::power_up(pmic, mct) {
        Ok(codec) => {
            println!("Codec: answers at 0x{:02x} on i2c_1", wm8758::ADDRESS);
            codec
        }
        Err(error) => {
            println!("Codec: power-up ERROR {:?}", error);
            halt();
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nWM8758 and I2S0 test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();
    console::flush();
    buffer_console();

    let cmu = Cmu::new();
    let epll = cmu.epll();
    println!(
        "From U-Boot: EPLL con0 0x{:08x} con1 0x{:08x}, {} Hz; MAUDIO gates 0x{:08x}; sclk_epll from {}",
        epll.con0,
        epll.con1,
        epll.hz(),
        cmu.audio_gates(),
        if cmu.sclk_epll_is_epll() { "the EPLL" } else { "the 24 MHz input" }
    );

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("i2c_0: init ERROR {:?}", error);
        halt();
    }
    let codec = codec(Max77686::new(i2c), mct);

    // Out before each step that could stop the board, so that the log shows where.
    console::flush();
    let rates = match i2s::set_clocks(mct) {
        Ok(rates) => rates,
        Err(error) => {
            println!("EPLL did not lock: {:?}", error);
            halt();
        }
    };
    let epll = cmu.epll();
    let clocks = i2s::audio_clocks();
    println!(
        "EPLL con0 0x{:08x} con1 0x{:08x}, {} Hz ({}); dout_aud_bus {} Hz ({}), dout_i2s {} Hz",
        epll.con0,
        epll.con1,
        rates.epll_hz,
        verdict(rates.epll_hz == 196_608_001),
        rates.aud_bus_hz,
        verdict(rates.aud_bus_hz == i2s::RCLK_HZ),
        rates.i2s_hz
    );
    println!(
        "AUDSS src 0x{:x} div 0x{:x} gate 0x{:x}; MAUDIO gates 0x{:08x}",
        clocks.src,
        clocks.div,
        clocks.gate,
        cmu.audio_gates()
    );

    console::flush();
    i2s::take_pins();
    let i2s = I2s::new();
    i2s.configure();
    let mut tone = Tone { sample: None, finished: false, table: sine_table(), phase: 0, step: 0, index: FIRST_TONE, left: true, right: true, silent: true };
    tone.set(FIRST_TONE);
    let prefilled = tone.pump(i2s);
    i2s.start();
    println!("I2S0 started with {} frames of silence: {:x?}", prefilled, i2s.registers());

    // The frame rate: frames taken while the FIFO stays full, against the MCT.
    tone.pump(i2s);
    i2s.take_underrun();
    let begin = mct.count();
    let mut frames = 0;
    while mct.count() - begin < MEASURE_US * COUNTS_PER_US {
        frames += tone.pump(i2s);
    }
    let us = (mct.count() - begin) / COUNTS_PER_US;
    let rate = print_rate("Frame rate", frames, us);
    println!(
        "  expected 48000 Hz: {}; underrun meanwhile: {}",
        verdict(rate.abs_diff(4_800_000) < 100),
        i2s.take_underrun()
    );

    let mut mclk = MclkDiv::Div2;
    let mut db = START_DB;
    let mut speakers = false;
    // The FIFO runs dry during the 500 ms the codec's VMID takes: silence either way.
    match codec.start_headphones(mct, mclk, db) {
        Ok(()) => println!("Codec: headphones on at {} dB, MCLK {:?}", db, mclk),
        Err(error) => println!("Codec: start ERROR {:?}", error),
    }
    tone.pump(i2s);
    i2s.take_underrun();
    tone.silent = false;
    for (number, (name, file)) in SAMPLES.iter().enumerate() {
        match Wav::parse(file) {
            Ok(wav) => println!(
                "Sample {}: {}, {} channels, {} bits, {} Hz, {} frames",
                number + 1,
                name,
                wav.channels,
                wav.bits,
                wav.sample_rate,
                wav.frames()
            ),
            Err(error) => println!("Sample {}: {}, ERROR {:?}", number + 1, name, error),
        }
    }
    if SAMPLES.is_empty() {
        println!("No samples: the build did not find them, see build.rs");
    }
    println!("Playing {} Hz at -12 dBFS. Keys: + - volume, t tone, l r b channels, s silence, 1-9 samples, m MCLK divider, o speakers, q stop", TONES_HZ[tone.index]);

    let uart = console::uart();
    let mut underruns = 0u32;
    let mut window_start = mct.count();
    let mut window_frames = 0u64;
    loop {
        window_frames += tone.pump(i2s);
        if i2s.take_underrun() {
            underruns += 1;
        }

        let elapsed = (mct.count() - window_start) / COUNTS_PER_US;
        if elapsed >= REPORT_US {
            let rate = centihertz(window_frames, elapsed);
            let (channels, hz) = tone.describe();
            println!(
                "{}.{:02} Hz, {} underruns; {} Hz {}, {} dB, speakers {}, MCLK {:?}; jack {}",
                rate / 100,
                rate % 100,
                underruns,
                hz,
                channels,
                db,
                if speakers { "on" } else { "off" },
                mclk,
                if wm8758::jack_in() { "in" } else { "out" }
            );
            window_start = mct.count();
            window_frames = 0;
            underruns = 0;
        }

        if tone.finished {
            tone.finished = false;
            println!("Sample done; silence, s for the tone");
        }

        let Some(key) = uart.try_read_byte() else { continue };
        let result = match key {
            b'1'..=b'9' => {
                let index = usize::from(key - b'1');
                if let Some((name, file)) = SAMPLES.get(index) {
                    match Playing::start(name, file) {
                        Ok(playing) => {
                            println!(
                                "Playing {}, {} frames, in {}",
                                name,
                                playing.wav.frames(),
                                match (playing.left, playing.right) {
                                    (true, true) => "both channels",
                                    (true, false) => "the left channel",
                                    _ => "the right channel",
                                }
                            );
                            tone.sample = Some(playing);
                            tone.silent = false;
                        }
                        Err(why) => println!("{}: {}", name, why),
                    }
                }
                Ok(())
            }
            b'+' | b'=' | b'-' => {
                db = if key == b'-' { (db - STEP_DB).max(wm8758::OUT_MIN_DB) } else { (db + STEP_DB).min(MAX_DB) };
                codec
                    .set_headphone_db(db)
                    .and_then(|set| if speakers { codec.set_speaker_db(set) } else { Ok(set) })
                    .map(|set| println!("Volume {} dB", set))
            }
            b'o' => {
                speakers = !speakers;
                if speakers {
                    db = db.min(START_DB);
                }
                codec
                    .set_speakers(speakers, db)
                    .and_then(|set| codec.set_headphone_db(set))
                    .map(|set| println!("Speakers {} at {} dB", if speakers { "on" } else { "off" }, set))
            }
            b't' => {
                tone.set((tone.index + 1) % TONES_HZ.len());
                println!("Tone {} Hz", TONES_HZ[tone.index]);
                Ok(())
            }
            b'l' | b'r' | b'b' => {
                (tone.left, tone.right) = (key != b'r', key != b'l');
                println!("Channels: {}", tone.describe().0);
                Ok(())
            }
            b's' => {
                tone.silent = !tone.silent;
                println!("Silence {}", if tone.silent { "on" } else { "off" });
                Ok(())
            }
            b'm' => {
                mclk = if mclk == MclkDiv::Div2 { MclkDiv::Div1 } else { MclkDiv::Div2 };
                codec.set_mclk_div(mclk).map(|()| println!("MCLK divider {:?}", mclk))
            }
            b'q' => {
                let result = codec.stop();
                i2s.stop();
                println!("Codec and I2S0 stopped: {:?}. Restart the board to play again.", result);
                halt();
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            println!("Codec ERROR {:?}", error);
        }
    }
}
