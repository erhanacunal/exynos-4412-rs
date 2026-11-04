//! A joined [`Bcm4334`] as an `embassy-net` device, served by an async task that sleeps until the
//! chip's host wake line pulses.
//!
//! [`new`] takes the chip, joined to its network ([`Bcm4334::connect`]) with its host wake line on
//! ([`Bcm4334::enable_host_wake`]), and the handshake state that joined it. It returns the
//! [`Device`] for `embassy_net::new` and the [`Runner`], whose [`run`](Runner::run) is the only
//! thing that touches the chip from then on. It moves frames between the chip and the stack's
//! channels, and answers the access point's key handshakes as they come.
//!
//! # When the chip is read
//!
//! Each read that finds nothing still costs a read of the chip's interrupt status over SDIO, so
//! the runner reads only when there may be something:
//!
//! - after a rising edge on [`HOST_WAKE`](bcm4334::HOST_WAKE), which the pin's wake-up interrupt
//!   latches: the firmware pulses the line, shorter than a look at its level can catch;
//! - while frames the chip announced are not all read, or an event was kept;
//! - every [`BACKSTOP`] regardless: what such a read finds counts as missed by the line;
//! - every [`ROOM_POLL`] while the stack has a frame to send and the firmware no room for it: the
//!   room grows only as frames are read.
//!
//! Between those it awaits the edge, a frame from the stack, room in the stack's receive queue,
//! or the next deadline, and the core sleeps.
//!
//! # Sharing the core
//!
//! Reads and writes of the chip are polled. So that the other tasks on the core wait no longer
//! than about [`SLICE`], the runner returns to the executor once a slice has passed, and goes on
//! where it stopped when it is polled again.
//!
//! # Cores
//!
//! The channels between the runner and the stack are not shared across cores: run the runner and
//! `embassy_net`'s runner on the same executor. The host wake interrupt goes to CPU0.

use core::cell::Cell;
use core::future::pending;

use embassy_futures::select::select4;
use embassy_net_driver_channel as ch;
use embassy_net_driver_channel::driver::{HardwareAddress, LinkState};
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_time::{Duration, Instant, Timer};

use crate::drivers::bcm4334::{self, Activity, Bcm4334, Error};
use crate::drivers::gpio::{Pull, Trigger};
use crate::embassy::eint::EintInput;
use crate::wpa::Supplicant;

/// Bytes of the longest Ethernet frame on a network with the usual packet size, 1500 bytes.
pub const MTU: usize = 1514;
/// The longest the chip goes unread: see [When the chip is read](self#when-the-chip-is-read).
pub const BACKSTOP: Duration = Duration::from_millis(50);
/// How often the chip is read while a frame waits for room in the firmware.
pub const ROOM_POLL: Duration = Duration::from_micros(200);

/// How long the runner reads and writes before it lets the other tasks run: see
/// [Sharing the core](self#sharing-the-core). A frame takes about 170 us to read, 115 us to write.
pub const SLICE: Duration = Duration::from_micros(500);

/// The channels' storage: `RX` frames from the chip to the stack, `TX` the other way.
pub type State<const RX: usize, const TX: usize> = ch::State<MTU, RX, TX>;
/// The device `embassy_net::new` takes.
pub type Device<'d> = ch::Device<'d, MTU>;

/// What the runner has done since it started. Times are in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames passed to the stack.
    pub frames_in: u32,
    /// Frames sent for the stack.
    pub frames_out: u32,
    /// Frames of the chip's own: events and the handshake.
    pub chip_frames: u32,
    /// Reads that found nothing: one ends each run of frames.
    pub empty_reads: u32,
    /// Edges on the host wake line taken.
    pub edges: u32,
    /// Reads after [`BACKSTOP`] without an edge.
    pub backstops: u32,
    /// Of those, the ones that found something: what the line missed.
    pub missed: u32,
    /// Reads while a frame waited for room in the firmware.
    pub room_polls: u32,
    /// Times the stack's receive queue was full before the chip was read empty.
    pub rx_full: u32,
    /// Time in reads of the chip.
    pub read_us: u64,
    /// Time in writes to it.
    pub write_us: u64,
}

impl Stats {
    const ZERO: Self = Self {
        frames_in: 0,
        frames_out: 0,
        chip_frames: 0,
        empty_reads: 0,
        edges: 0,
        backstops: 0,
        missed: 0,
        room_polls: 0,
        rx_full: 0,
        read_us: 0,
        write_us: 0,
    };
}

static STATS: CriticalSectionMutex<Cell<Stats>> = CriticalSectionMutex::new(Cell::new(Stats::ZERO));

/// What the runner has done so far, from any task or core.
pub fn stats() -> Stats {
    STATS.lock(Cell::get)
}

/// Serves the chip for the stack: see the [module documentation](self).
pub struct Runner<'d> {
    chip: Bcm4334,
    supplicant: Supplicant,
    report: fn(Activity),
    wake: EintInput,
    channel: ch::Runner<'d, MTU>,
}

/// Makes the chip a device of `embassy-net`, its link up. The chip must be joined and its host
/// wake line on, and [`Gic::init`](crate::drivers::gic::Gic::init) done, with the MMU on.
/// `report` hears of what happens to the connection.
pub fn new<'d, const RX: usize, const TX: usize>(
    state: &'d mut State<RX, TX>,
    chip: Bcm4334,
    supplicant: Supplicant,
    report: fn(Activity),
) -> (Device<'d>, Runner<'d>) {
    // GPX0 has a wake-up interrupt for every pin.
    let wake = EintInput::new(bcm4334::HOST_WAKE, Pull::None).expect("the host wake line is on GPX0");
    wake.arm(Trigger::RisingEdge);
    let (mut channel, device) = ch::new(state, HardwareAddress::Ethernet(supplicant.own()));
    channel.set_link_state(LinkState::Up);
    (device, Runner { chip, supplicant, report, wake, channel })
}

impl Runner<'_> {
    /// Serves the chip until the connection fails, then sets the link down and returns why.
    pub async fn run(self) -> Error {
        let Runner { mut chip, mut supplicant, report, wake, channel } = self;
        let (state, mut rx, mut tx) = channel.split();
        let mut link = Link { chip: &mut chip, supplicant: &mut supplicant, report, stats: Stats::ZERO };
        let error = link.serve(&wake, &mut rx, &mut tx).await;
        state.set_link_state(LinkState::Down);
        error
    }
}

/// What [`Runner::run`] works with.
struct Link<'a> {
    chip: &'a mut Bcm4334,
    supplicant: &'a mut Supplicant,
    report: fn(Activity),
    stats: Stats,
}

impl Link<'_> {
    async fn serve(&mut self, wake: &EintInput, rx: &mut ch::RxRunner<'_, MTU>, tx: &mut ch::TxRunner<'_, MTU>) -> Error {
        let mut last_read = Instant::now();
        // The last read stopped before the chip had nothing more: for the slice, or for room.
        let mut unfinished = false;
        loop {
            let until = Instant::now() + SLICE;
            if let Err(error) = self.send(tx, until) {
                return error;
            }

            let edge = wake.take();
            self.stats.edges += u32::from(edge);
            let known = self.chip.has_pending() || unfinished;
            let starved = tx.try_tx_buf().is_some() && !self.chip.can_send();
            let backstop = !edge && !known && !starved && last_read.elapsed() >= BACKSTOP;
            if edge || known || starved || backstop {
                if backstop {
                    self.stats.backstops += 1;
                } else if starved && !edge && !known {
                    self.stats.room_polls += 1;
                }
                last_read = Instant::now();
                match self.read(rx, tx, until) {
                    Ok((found, stop)) => {
                        self.stats.missed += u32::from(backstop && found);
                        unfinished = !matches!(stop, Stop::Empty);
                    }
                    Err(error) => return error,
                }
            }
            STATS.lock(|cell| cell.set(self.stats));

            // What can end the wait.
            let can_send = self.chip.can_send();
            let to_send = tx.try_tx_buf().is_some();
            let starved = to_send && !can_send;
            let waiting_in_chip = self.chip.has_pending() || unfinished;
            let deadline = if starved { Instant::now() + ROOM_POLL } else { last_read + BACKSTOP };
            if (waiting_in_chip && rx.try_rx_buf().is_some()) || (to_send && can_send) {
                // Work left, and room for it: let the other tasks run, then go on.
                embassy_futures::yield_now().await;
                continue;
            }
            select4(
                async {
                    if waiting_in_chip {
                        pending::<()>().await;
                    }
                    wake.wait_latched().await;
                },
                async {
                    if !can_send {
                        pending::<()>().await;
                    }
                    tx.tx_buf().await;
                },
                async {
                    if !waiting_in_chip {
                        pending::<()>().await;
                    }
                    rx.rx_buf().await;
                },
                Timer::at(deadline),
            )
            .await;
        }
    }

    /// Sends what the stack has while the firmware has room, until `until`.
    fn send(&mut self, tx: &mut ch::TxRunner<'_, MTU>, until: Instant) -> Result<(), Error> {
        while self.chip.can_send() && Instant::now() < until {
            let Some(frame) = tx.try_tx_buf() else {
                break;
            };
            let begin = Instant::now();
            let sent = self.chip.send_ethernet(frame);
            self.stats.write_us += begin.elapsed().as_micros();
            sent?;
            tx.tx_done();
            self.stats.frames_out += 1;
        }
        Ok(())
    }

    /// Reads until the chip has nothing more, the stack no room for it, or `until` has passed,
    /// sending between frames. Returns whether it found anything, and why it stopped.
    fn read(&mut self, rx: &mut ch::RxRunner<'_, MTU>, tx: &mut ch::TxRunner<'_, MTU>, until: Instant) -> Result<(bool, Stop), Error> {
        let mut found = false;
        loop {
            if found && Instant::now() >= until {
                return Ok((found, Stop::Slice));
            }
            let Some(buffer) = rx.try_rx_buf() else {
                self.stats.rx_full += 1;
                return Ok((found, Stop::Full));
            };
            let before = self.chip.frames_read();
            let begin = Instant::now();
            let polled = self.chip.poll(self.supplicant, buffer, self.report);
            self.stats.read_us += begin.elapsed().as_micros();
            match polled? {
                Some(length) => {
                    rx.rx_done(length);
                    self.stats.frames_in += 1;
                }
                None if self.chip.frames_read() != before => self.stats.chip_frames += 1,
                None => {
                    self.stats.empty_reads += 1;
                    return Ok((found, Stop::Empty));
                }
            }
            found = true;
            self.send(tx, until)?;
        }
    }
}

/// Why [`Link::read`] stopped.
enum Stop {
    /// The chip had nothing more.
    Empty,
    /// The stack's receive queue was full.
    Full,
    /// The slice was over.
    Slice,
}
