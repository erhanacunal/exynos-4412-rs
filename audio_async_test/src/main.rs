//! WM8758 and I2S0 through `pdma0`, as in `audio_dma_test`, but the CPU awaits the end of each
//! half of the ring on the controller's interrupt (GIC ID 67) in an Embassy task, instead of
//! polling for it.
//!
//! 1. The codec, the clocks and I2S0 as in `audio_dma_test`; the DMA starts on a ring of
//!    silence, and the codec's headphones at -30 dB.
//! 2. A wait for a half cut after 10 ms, right after a half began: it must time out, and the
//!    next wait must still end, within the 85 ms of a half.
//! 3. Silence for 1 s: the frame rate from the events, 48000 Hz; one interrupt per half; the
//!    wake-up time.
//! 4. 1 kHz at -12 dBFS, with a task ticking every 1 ms beside it.
//!
//! The wake-up time is how far the channel has read into the next half when the task looks:
//! the time from the event to the task, through the interrupt and the executor, in whole
//! frames of 21 us.
//!
//! Every 5 s: the frame rate, halves, interrupts, wake-up mean and maximum, late and resynced
//! services, FIFO underruns, the ticker, the jack.
//!
//! Keys: `+` `-` volume, `t` tone, `l` `r` `b` channels, `s` silence, `1`-`9` samples, `o`
//! speakers, `p` where the channel reads, `j` the jack pin with each pull, `q` stop. Load, all on the playing core: `w` 100
//! volume writes to the codec in a row, polled, about 60 ms of I2C; `h` the CPU busy for 50 ms,
//! under a half; `H` busy for 200 ms, over two halves, which must count as late.

#![no_std]
#![no_main]

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::gpio::Pull;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::i2s::{self, DmaPlayback, DmaProgram, DmaRing, I2s};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::pl330::{self, Pl330};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::wm8758::{self, MclkDiv, Wm8758};
use exynos_4412_rs::embassy::uart::{UartRx, buffer_console};
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
const HALF: usize = i2s::DMA_HALF_FRAMES;
/// A half, 85.3 ms, in microseconds.
const HALF_US: u64 = HALF as u64 * 1_000_000 / i2s::SAMPLE_RATE_HZ as u64;
const CUT_AFTER: Duration = Duration::from_millis(10);
const MEASURE: Duration = Duration::from_secs(1);
const REPORT: Duration = Duration::from_secs(5);
const VOLUME_WRITES: u32 = 100;
const SHORT_BUSY_US: u64 = 50_000;
const LONG_BUSY_US: u64 = 200_000;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

static mut RING: DmaRing = DmaRing::new();
static mut PROGRAM: DmaProgram = DmaProgram::new();

/// From the keys to the playback task.
#[derive(Clone, Copy, Debug)]
enum Command {
    NextTone,
    Channels { left: bool, right: bool },
    ToggleSilence,
    Sample(usize),
    Position,
    Stop,
}

static COMMANDS: Channel<CriticalSectionRawMutex, Command, 8> = Channel::new();
/// The keys task's settings, for the report.
static DB: AtomicI32 = AtomicI32::new(START_DB as i32);
static SPEAKERS: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU32 = AtomicU32::new(0);
static MAX_TICK_LATE_US: AtomicU32 = AtomicU32::new(0);

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

fn halt() -> ! {
    console::flush();
    cpu::halt();
}

/// Holds the CPU, running nothing else on it.
fn busy(us: u64) {
    let mct = Mct::new();
    let end = mct.count() + us * COUNTS_PER_US;
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

/// The rate `frames` in `us` give, in hundredths of Hz.
fn centihertz(frames: u64, us: u64) -> u64 {
    frames * 100_000_000 / us.max(1)
}

const fn frames_to_us(frames: u64) -> u64 {
    frames * 1_000_000 / i2s::SAMPLE_RATE_HZ as u64
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
    /// Set when a sample has ended, for the task to report.
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
    fn new() -> Self {
        let mut tone = Self {
            sample: None,
            finished: false,
            table: sine_table(),
            phase: 0,
            step: 0,
            index: FIRST_TONE,
            left: true,
            right: true,
            silent: false,
        };
        tone.set(FIRST_TONE);
        tone
    }

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
            self.finished = true;
        }
        let frame = self.frame();
        self.phase = self.phase.wrapping_add(self.step);
        frame
    }

    fn describe(&self) -> &'static str {
        match (self.silent, self.left, self.right) {
            (true, _, _) => "silence",
            (false, true, true) => "both",
            (false, true, false) => "left",
            _ => "right",
        }
    }
}

/// What the halves of one window showed, timed from the event that opened it to the last.
struct Window {
    first: Instant,
    last: Instant,
    halves: u64,
    late: u32,
    resynced: u32,
    interrupts: u32,
    wakes: u32,
    wake_sum: u64,
    wake_max: usize,
    underruns: u32,
}

impl Window {
    fn new(at: Instant) -> Self {
        Self {
            first: at,
            last: at,
            halves: 0,
            late: 0,
            resynced: 0,
            interrupts: pl330::interrupts(),
            wakes: 0,
            wake_sum: 0,
            wake_max: 0,
            underruns: 0,
        }
    }

    fn add(&mut self, wake: Option<usize>, done: i2s::Serviced, underrun: bool) {
        self.last = Instant::now();
        self.halves += u64::from(done.refilled);
        self.late += done.late;
        self.resynced += done.resynced;
        if let Some(frames) = wake {
            self.wakes += 1;
            self.wake_sum += frames as u64;
            self.wake_max = self.wake_max.max(frames);
        }
        self.underruns += u32::from(underrun);
    }

    /// In hundredths of Hz.
    fn rate(&self) -> u64 {
        centihertz(self.halves * HALF as u64, (self.last - self.first).as_micros())
    }

    fn interrupts(&self) -> u32 {
        pl330::interrupts() - self.interrupts
    }

    /// Mean and maximum wake-up, in microseconds. A frame is 21 us: a wake-up shorter than
    /// that counts as 0.
    fn wake_us(&self) -> (u64, u64) {
        (frames_to_us(self.wake_sum) / u64::from(self.wakes.max(1)), frames_to_us(self.wake_max as u64))
    }
}

/// Awaits the next half, then refills it with `fill`.
async fn next_half(
    playback: &mut DmaPlayback,
    i2s: I2s,
    window: &mut Window,
    fill: impl FnMut(&mut [u32; HALF]),
) -> i2s::Serviced {
    playback.wait_half().await;
    // How far the channel has read into the half after the one that ended: the time since the
    // event. Only when exactly that one half ended.
    let position = playback.position();
    let done = playback.service(fill);
    let wake = position.filter(|_| done.refilled == 1 && done.late == 0 && done.resynced == 0).map(|frame| frame % HALF);
    window.add(wake, done, i2s.take_underrun());
    done
}

#[embassy_executor::task]
async fn ticker() {
    loop {
        let begin = Instant::now();
        Timer::after(Duration::from_millis(1)).await;
        let late = begin.elapsed().as_micros().saturating_sub(1000) as u32;
        MAX_TICK_LATE_US.fetch_max(late, Ordering::Relaxed);
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
}

#[embassy_executor::task]
async fn playback_task(mut playback: DmaPlayback, i2s: I2s) {
    // The halves that ended while the codec started, before the executor ran.
    let skipped = playback.service(|half| half.fill(0));
    i2s.take_underrun();
    println!(
        "Halves passed before the executor: {} refilled, {} late, {} resynced; channel in half {:?}, half {} next",
        skipped.refilled,
        skipped.late,
        skipped.resynced,
        playback.reading_half(),
        playback.next_half()
    );

    // A wait dropped part way, then one more.
    let mut window = Window::new(Instant::now());
    next_half(&mut playback, i2s, &mut window, |half| half.fill(0)).await;
    let begin = Instant::now();
    let cut = with_timeout(CUT_AFTER, playback.wait_half()).await;
    let after_cut = begin.elapsed().as_micros();
    next_half(&mut playback, i2s, &mut window, |half| half.fill(0)).await;
    let next = begin.elapsed().as_micros();
    println!(
        "Wait cut after {} ms: {} after {} us; the next one ended {} us after the cut began, a half is {} us ({})",
        CUT_AFTER.as_millis(),
        if cut.is_err() { "timed out" } else { "ended" },
        after_cut,
        next,
        HALF_US,
        verdict(cut.is_err() && next <= HALF_US + 1000)
    );

    // Silence for about 1 s, from one event to the last.
    let mut window = Window::new(Instant::now());
    while window.last - window.first < MEASURE {
        next_half(&mut playback, i2s, &mut window, |half| half.fill(0)).await;
    }
    let rate = window.rate();
    let (mean, max) = window.wake_us();
    let interrupts = window.interrupts();
    println!(
        "Frame rate: {} halves in {} us, {}.{:02} Hz ({}); {} interrupts ({}); wake-up {} us mean, {} us max; late {}, resynced {}, FIFO underruns {}",
        window.halves,
        (window.last - window.first).as_micros(),
        rate / 100,
        rate % 100,
        // The events are timed when the task runs, to a frame: over 1 s, about 1 Hz.
        verdict(window.halves > 0 && rate.abs_diff(4_800_000) < 300),
        interrupts,
        verdict(u64::from(interrupts) == window.halves),
        mean,
        max,
        window.late,
        window.resynced,
        window.underruns
    );

    let mut tone = Tone::new();
    println!(
        "Playing {} Hz at -12 dBFS.\nKeys: + - volume, t tone, l r b channels, s silence, 1-9 samples, o speakers, w h H load, p position, j jack pin, q stop",
        TONES_HZ[tone.index]
    );
    MAX_TICK_LATE_US.store(0, Ordering::Relaxed);
    let mut ticks = TICKS.load(Ordering::Relaxed);
    let mut window = Window::new(Instant::now());
    loop {
        let command = match select(playback.wait_half(), COMMANDS.receive()).await {
            Either::First(()) => None,
            Either::Second(command) => Some(command),
        };
        let Some(command) = command else {
            next_half(&mut playback, i2s, &mut window, |half| {
                for frame in half.iter_mut() {
                    *frame = tone.next_frame();
                }
            })
            .await;
            if tone.finished {
                tone.finished = false;
                println!("Sample done; back to the tone");
            }
            if window.last - window.first >= REPORT {
                let rate = window.rate();
                let (mean, max) = window.wake_us();
                let now_ticks = TICKS.load(Ordering::Relaxed);
                println!(
                    "{}.{:02} Hz, {} halves, {} interrupts, wake-up {} us mean {} us max, {} late, {} resynced, {} FIFO underruns\n  ticker {} ticks, {} us late at most; {} Hz {}, {} dB, speakers {}; jack {}",
                    rate / 100,
                    rate % 100,
                    window.halves,
                    window.interrupts(),
                    mean,
                    max,
                    window.late,
                    window.resynced,
                    window.underruns,
                    now_ticks.wrapping_sub(ticks),
                    MAX_TICK_LATE_US.swap(0, Ordering::Relaxed),
                    TONES_HZ[tone.index],
                    tone.describe(),
                    DB.load(Ordering::Relaxed),
                    if SPEAKERS.load(Ordering::Relaxed) { "on" } else { "off" },
                    if wm8758::jack_in() { "in" } else { "out" }
                );
                ticks = now_ticks;
                window = Window::new(window.last);
            }
            continue;
        };
        match command {
            Command::NextTone => {
                tone.set((tone.index + 1) % TONES_HZ.len());
                println!("Tone {} Hz", TONES_HZ[tone.index]);
            }
            Command::Channels { left, right } => {
                (tone.left, tone.right) = (left, right);
                println!("Channels: {}", tone.describe());
            }
            Command::ToggleSilence => {
                tone.silent = !tone.silent;
                println!("Silence {}", if tone.silent { "on" } else { "off" });
            }
            Command::Sample(index) => {
                if let Some((name, file)) = SAMPLES.get(index) {
                    match Playing::start(name, file) {
                        Ok(playing) => {
                            println!("Playing {}, {} frames", name, playing.wav.frames());
                            tone.sample = Some(playing);
                        }
                        Err(why) => println!("{}: {}", name, why),
                    }
                }
            }
            Command::Position => {
                println!("Reading frame {:?} of the ring, half {} next; {:x?}", playback.position(), playback.next_half(), playback.channel());
            }
            Command::Stop => {
                let stopped = playback.stop(i2s, Mct::new());
                println!("DMA and I2S0 stopped: {:?}; {} pdma0 interrupts in all. Restart the board to play again.", stopped, pl330::interrupts());
                halt();
            }
        }
    }
}

/// The jack pin read with each pull, 1 ms after setting it, then left without one, as
/// `wm8758::power_up` sets it and the DTS asks: does the line float?
fn jack_probe() {
    let pin = wm8758::JACK_DETECT;
    let mut levels = [0u8; 3];
    for (level, pull) in levels.iter_mut().zip([Pull::None, Pull::Up, Pull::Down]) {
        pin.set_pull(pull);
        busy(1000);
        *level = u8::from(pin.read());
    }
    pin.set_pull(Pull::None);
    println!("Jack pin gpx1-6: none {}, up {}, down {}", levels[0], levels[1], levels[2]);
}

#[embassy_executor::task]
async fn keys_task(codec: Wm8758) {
    let rx = UartRx::new(console::uart().id()).expect("the console UART has an interrupt");
    let mut db = START_DB;
    let mut speakers = false;
    loop {
        let key = rx.read_byte().await;
        let command = match key {
            b'1'..=b'9' => Some(Command::Sample(usize::from(key - b'1'))),
            b't' => Some(Command::NextTone),
            b'l' | b'r' | b'b' => Some(Command::Channels { left: key != b'r', right: key != b'l' }),
            b's' => Some(Command::ToggleSilence),
            b'p' => Some(Command::Position),
            _ => None,
        };
        if let Some(command) = command {
            COMMANDS.send(command).await;
            continue;
        }
        let result = match key {
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
                let begin = Instant::now();
                let mut result = Ok(db);
                for _ in 0..VOLUME_WRITES {
                    result = codec.set_headphone_db(db);
                    if result.is_err() {
                        break;
                    }
                }
                println!("{} volume writes in {} us", VOLUME_WRITES, begin.elapsed().as_micros());
                result.map(|_| ())
            }
            b'h' | b'H' => {
                let us = if key == b'h' { SHORT_BUSY_US } else { LONG_BUSY_US };
                busy(us);
                println!("CPU busy for {} ms", us / 1000);
                Ok(())
            }
            b'j' => {
                jack_probe();
                Ok(())
            }
            b'q' => {
                println!("Codec stopped: {:?}", codec.stop());
                COMMANDS.send(Command::Stop).await;
                Ok(())
            }
            _ => Ok(()),
        };
        DB.store(i32::from(db), Ordering::Relaxed);
        SPEAKERS.store(speakers, Ordering::Relaxed);
        if let Err(error) = result {
            println!("Codec ERROR {:?}", error);
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nWM8758 and I2S0 through pdma0, the halves awaited on its interrupt");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    pl330::init_interrupt();
    cpu::enable_irq();
    console::flush();
    buffer_console();

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("i2c_0: init ERROR {:?}", error);
        halt();
    }
    let codec = match wm8758::power_up(Max77686::new(i2c), mct) {
        Ok(codec) => {
            println!("Codec: answers at 0x{:02x} on i2c_1", wm8758::ADDRESS);
            codec
        }
        Err(error) => {
            println!("Codec: power-up ERROR {:?}", error);
            halt();
        }
    };
    // Before the outputs are on the pin reads low whatever is in the socket: shown, not
    // judged.
    jack_probe();

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
    println!("pdma0: CR0 0x{:08x}; interrupt GIC ID {}", dma.config().cr0, pl330::PDMA0_IRQ);

    // SAFETY: the only references to these, taken once.
    let (ring, program) = unsafe { (&mut *addr_of_mut!(RING), &mut *addr_of_mut!(PROGRAM)) };
    let mut playback = DmaPlayback::new(ring, program);
    console::flush();
    if let Err(error) = playback.start(i2s, mct) {
        println!("DMA start ERROR {:?}", error);
        halt();
    }
    // The ring of silence plays on during the 500 ms the codec's VMID takes.
    match codec.start_headphones(mct, MclkDiv::Div2, START_DB) {
        Ok(()) => {
            println!("Codec: headphones on at {} dB; jack {}", START_DB, if wm8758::jack_in() { "in" } else { "out" });
            jack_probe();
        }
        Err(error) => println!("Codec: start ERROR {:?}", error),
    }
    for (number, (name, file)) in SAMPLES.iter().enumerate() {
        if let Ok(wav) = Wav::parse(file) {
            println!("Sample {}: {}, {} frames", number + 1, name, wav.frames());
        }
    }

    embassy::run(|spawner| {
        spawner.spawn(ticker().unwrap());
        spawner.spawn(playback_task(playback, i2s).unwrap());
        spawner.spawn(keys_task(codec).unwrap());
    })
}
