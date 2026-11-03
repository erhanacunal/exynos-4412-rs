//! WM8758 and I2S0 through `pdma0`: the FIFO filled by DMA from a ring of two halves, which
//! the CPU refills.
//!
//! 1. The codec's rail, its I2C, the jack pin, and the clocks, as in `audio_test`.
//! 2. `pdma0`: its configuration, and the channel program of [`DmaPlayback`], printed.
//! 3. Silence through the ring for 1 s, timed from one half's event to a later one: the frame
//!    rate, 48000 Hz if the clocks are right; no FIFO underrun.
//! 4. The codec's headphones at -30 dB, and 1 kHz at -12 dBFS.
//!
//! Every 5 s: the frame rate from the events, halves refilled late, FIFO underruns, the jack.
//!
//! Keys, as in `audio_test`: `+` `-` volume, `t` tone, `l` `r` `b` channels, `s` silence,
//! `1`-`9` samples, `m` MCLK divider, `o` speakers, `q` stop. Load: `w` 100 volume writes to
//! the codec in a row, about 60 ms of I2C, which stopped the CPU-filled FIFO; `h` the CPU busy
//! for 50 ms, under a half; `H` busy for 200 ms, over two halves, which must count as late;
//! `p` where the channel reads.

#![no_std]
#![no_main]

use core::ptr::addr_of_mut;

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::i2s::{self, DmaPlayback, DmaProgram, DmaRing, I2s};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::pl330::{self, Pl330};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::wm8758::{self, MclkDiv, Wm8758};
use exynos_4412_rs::embassy::uart::buffer_console;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::wav::Wav;
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, print, println, soc};

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
const VOLUME_WRITES: u32 = 100;
const SHORT_BUSY_US: u64 = 50_000;
const LONG_BUSY_US: u64 = 200_000;

static mut RING: DmaRing = DmaRing::new();
/// Room for the `DMALPFE` probe, a line of its own.
#[repr(C, align(32))]
struct Probe([u8; 32]);
static mut PROBE: Probe = Probe([0; 32]);
/// The event the probe raises, apart from [`i2s::DMA_EVENTS`].
const PROBE_EVENT: u8 = 2;
static mut PROGRAM: DmaProgram = DmaProgram::new();

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

fn busy(mct: Mct, us: u64) {
    let end = mct.count() + us * COUNTS_PER_US;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

/// Frames played over a window of whole halves, timed from event to event.
struct Window {
    start: u64,
    last: u64,
    halves: u64,
    late: u32,
}

impl Window {
    fn new(now: u64) -> Self {
        Self { start: now, last: now, halves: 0, late: 0 }
    }

    /// In hundredths of Hz.
    fn rate(&self) -> u64 {
        let us = (self.last - self.start) / COUNTS_PER_US;
        centihertz(self.halves * i2s::DMA_HALF_FRAMES as u64, us)
    }
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

    /// The next frame: from the sample if one plays, else the tone.
    fn next_frame(&mut self) -> u32 {
        if let Some(playing) = &mut self.sample {
            if let Some(frame) = playing.next() {
                return frame;
            }
            self.sample = None;
            self.silent = true;
            self.finished = true;
        }
        let frame = self.frame();
        self.phase = self.phase.wrapping_add(self.step);
        frame
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
    println!("\nWM8758 and I2S0 through pdma0");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();
    console::flush();
    buffer_console();

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("i2c_0: init ERROR {:?}", error);
        halt();
    }
    let codec = codec(Max77686::new(i2c), mct);
    println!("Jack: {}", if wm8758::jack_in() { "in" } else { "out" });

    // Out before each step that could stop the board, so that the log shows where.
    console::flush();
    match i2s::set_clocks(mct) {
        Ok(rates) => println!(
            "Clocks: EPLL {} Hz, dout_aud_bus {} Hz ({})",
            rates.epll_hz,
            rates.aud_bus_hz,
            verdict(rates.aud_bus_hz == i2s::RCLK_HZ)
        ),
        Err(error) => {
            println!("EPLL did not lock: {:?}", error);
            halt();
        }
    }
    console::flush();
    i2s::take_pins();
    let i2s = I2s::new();
    i2s.configure();

    Cmu::new().open_pdma0_gate();
    let dma = Pl330::pdma0();
    let config = dma.config();
    println!(
        "pdma0: CR0 0x{:08x}, {} channels, {} peripheral requests, {} events, manager {}; manager state {:?}",
        config.cr0,
        config.channels,
        config.peripherals,
        config.events,
        if config.manager_ns { "non-secure" } else { "secure" },
        dma.manager()
    );
    println!("  channel {} before: {:x?}", i2s::DMA_CHANNEL, dma.channel(i2s::DMA_CHANNEL));

    probe_forever(dma, mct);

    // SAFETY: the only references to these, taken once.
    let (ring, program) = unsafe { (&mut *addr_of_mut!(RING), &mut *addr_of_mut!(PROGRAM)) };
    let ring_address = ring.0.as_ptr() as usize;
    let mut playback = DmaPlayback::new(ring, program);
    print!("  program at 0x{:08x}, ring at 0x{:08x}:", program_address(&playback), ring_address);
    for byte in playback.program() {
        print!(" {:02x}", byte);
    }
    println!();

    console::flush();
    if let Err(error) = playback.start(i2s, mct) {
        println!("DMA start ERROR {:?}", error);
        halt();
    }
    // The first 10 ms, every 1 ms: where the channel reads, and the FIFO. At 48 kHz the
    // channel moves 48 frames a millisecond, and the FIFO stays near full.
    let start = mct.count();
    for ms in 1..=10u64 {
        while mct.count() - start < ms * 1000 * COUNTS_PER_US {}
        let channel = playback.channel();
        let registers = i2s.registers();
        println!(
            "  {:2} ms: frame {:?}, {:?}, pc 0x{:x}, fault {:x?}; FIFO {} frames, CON 0x{:08x}; INTSTATUS 0x{:x}",
            ms,
            playback.position(),
            channel.state,
            channel.pc,
            channel.fault,
            i2s.tx_level(),
            registers.con,
            dma.events()
        );
    }
    i2s.take_underrun();

    // The frame rate, from the first event to the last within about 1 s.
    let mut first = None;
    let mut window = Window::new(0);
    let begin = mct.count();
    while mct.count() - begin < MEASURE_US * COUNTS_PER_US + 200_000 * COUNTS_PER_US {
        let done = playback.service(|half| half.fill(0));
        if done.refilled > 0 {
            let now = mct.count();
            match first {
                None => {
                    first = Some(());
                    window = Window::new(now);
                }
                Some(()) => {
                    window.halves += u64::from(done.refilled);
                    window.late += done.late;
                    window.last = now;
                }
            }
        }
    }
    let rate = window.rate();
    println!(
        "Frame rate: {} halves of {} frames in {} us, {}.{:02} Hz ({}); late {}, FIFO underrun {}",
        window.halves,
        i2s::DMA_HALF_FRAMES,
        (window.last - window.start) / COUNTS_PER_US,
        rate / 100,
        rate % 100,
        verdict(window.halves > 0 && rate.abs_diff(4_800_000) < 100),
        window.late,
        i2s.take_underrun()
    );
    println!("  then: frame {:?}, {:x?}; manager {:?}", playback.position(), playback.channel(), dma.manager());

    let mut mclk = MclkDiv::Div2;
    let mut db = START_DB;
    let mut speakers = false;
    // The DMA plays the ring of silence on during the 500 ms the codec's VMID takes.
    match codec.start_headphones(mct, mclk, db) {
        Ok(()) => println!("Codec: headphones on at {} dB, MCLK {:?}", db, mclk),
        Err(error) => println!("Codec: start ERROR {:?}", error),
    }
    let mut tone = Tone { sample: None, finished: false, table: sine_table(), phase: 0, step: 0, index: FIRST_TONE, left: true, right: true, silent: false };
    tone.set(FIRST_TONE);
    let skipped = playback.service(|half| half.fill(0));
    i2s.take_underrun();
    println!("  halves passed meanwhile: {} refilled, {} late", skipped.refilled, skipped.late);
    for (number, (name, file)) in SAMPLES.iter().enumerate() {
        if let Ok(wav) = Wav::parse(file) {
            println!("Sample {}: {}, {} frames", number + 1, name, wav.frames());
        }
    }
    println!("Playing {} Hz at -12 dBFS. Keys: + - volume, t tone, l r b channels, s silence, 1-9 samples, m MCLK divider, o speakers, w h H load, p position, q stop", TONES_HZ[tone.index]);

    let uart = console::uart();
    let mut underruns = 0u32;
    let mut window = Window::new(mct.count());
    loop {
        let done = playback.service(|half| {
            for frame in half.iter_mut() {
                *frame = tone.next_frame();
            }
        });
        if done.refilled > 0 {
            window.halves += u64::from(done.refilled);
            window.late += done.late;
            window.last = mct.count();
        }
        if i2s.take_underrun() {
            underruns += 1;
        }

        if (mct.count() - window.start) / COUNTS_PER_US >= REPORT_US + 500_000 && window.halves == 0 {
            // No half in all that time: the channel has stopped.
            let channel = playback.channel();
            println!(
                "No half ended for 5 s: frame {:?}, {:?}, pc 0x{:x}, fault {:x?}; FIFO {} frames, CON 0x{:08x}; manager {:?}",
                playback.position(),
                channel.state,
                channel.pc,
                channel.fault,
                i2s.tx_level(),
                i2s.registers().con,
                dma.manager()
            );
            window = Window::new(mct.count());
        }
        if (mct.count() - window.start) / COUNTS_PER_US >= REPORT_US && done.refilled > 0 {
            let rate = window.rate();
            let (channels, hz) = tone.describe();
            println!(
                "{}.{:02} Hz, {} late, {} FIFO underruns; {} Hz {}, {} dB, speakers {}, MCLK {:?}; jack {}",
                rate / 100,
                rate % 100,
                window.late,
                underruns,
                hz,
                channels,
                db,
                if speakers { "on" } else { "off" },
                mclk,
                if wm8758::jack_in() { "in" } else { "out" }
            );
            window = Window::new(window.last);
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
                            println!("Playing {}, {} frames", name, playing.wav.frames());
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
            b'w' => {
                let begin = mct.count();
                let mut result = Ok(db);
                for _ in 0..VOLUME_WRITES {
                    result = codec.set_headphone_db(db);
                    if result.is_err() {
                        break;
                    }
                }
                println!("{} volume writes in {} us", VOLUME_WRITES, (mct.count() - begin) / COUNTS_PER_US);
                result.map(|_| ())
            }
            b'h' | b'H' => {
                let us = if key == b'h' { SHORT_BUSY_US } else { LONG_BUSY_US };
                busy(mct, us);
                println!("CPU busy for {} ms", us / 1000);
                Ok(())
            }
            b'p' => {
                println!("Reading frame {:?} of the ring; {:x?}", playback.position(), playback.channel());
                Ok(())
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
                let stopped = playback.stop(i2s, mct);
                println!("Codec stopped: {:?}; DMA and I2S0 stopped: {:?}. Restart the board to play again.", result, stopped);
                halt();
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            println!("Codec ERROR {:?}", error);
        }
    }
}

/// Runs `DMASEV; DMALPFE` back to the `DMASEV`, with the loop counter bit clear and then set,
/// for 100 us each: the encoding that loops raises the event and does not fault.
fn probe_forever(dma: Pl330, mct: Mct) {
    // SAFETY: the only reference to it, here only.
    let probe = unsafe { &mut *addr_of_mut!(PROBE) };
    let channel = i2s::DMA_CHANNEL;
    dma.enable_interrupt(PROBE_EVENT);
    for lpend in [0x28u8, 0x28 | pl330::LPFE_COUNTER_BIT] {
        probe.0[..4].copy_from_slice(&[0x34, PROBE_EVENT << 3, lpend, 2]);
        mmu::clean_dcache_range(probe.0.as_ptr() as usize, probe.0.len());
        dma.take_event(PROBE_EVENT);
        let started = dma.start(channel, probe.0.as_ptr() as usize, mct);
        busy(mct, 100);
        let report = dma.channel(channel);
        let raised = dma.take_event(PROBE_EVENT);
        busy(mct, 10);
        let again = dma.take_event(PROBE_EVENT);
        let killed = dma.kill(channel, mct);
        busy(mct, 10);
        println!(
            "DMALPFE as 0x{:02x}: start {:?}, {:?}, pc 0x{:x}, fault {:x?}; event {}, raised again {}; kill {:?}, then {:?}",
            lpend,
            started,
            report.state,
            report.pc,
            report.fault,
            raised,
            again,
            killed,
            dma.channel(channel).state
        );
    }
    dma.disable_interrupt(PROBE_EVENT);
}

fn program_address(playback: &DmaPlayback) -> usize {
    playback.program().as_ptr() as usize
}
