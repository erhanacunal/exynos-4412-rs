//! Connects a joined [`Bcm4334`] to the `smoltcp` network stack, as its Ethernet device.
//!
//! Reading the device also serves the connection, so a key handshake from the access point gets
//! its answer as long as the stack is polled. `smoltcp` has no way to take an error from a device,
//! so a failure stops the device, and [`failure`](WifiDevice::failure) tells what it was.

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, Icmpv4Message, Icmpv4Packet, IpProtocol, Ipv4Packet, TcpPacket,
};

use super::bcm4334::{Activity, Bcm4334, Error, Timing};
use super::timer::Mct;
use crate::wpa::Supplicant;

/// Bytes of the longest Ethernet frame on a network with the usual packet size, 1500 bytes.
const FRAME_MAX: usize = 1514;

/// What the device has done, to find where the time goes. Times are in MCT counts. Each only
/// grows, so a caller measures by subtracting two readings.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Frames handed to the chip.
    pub sent: u32,
    /// Their bytes.
    pub sent_bytes: u64,
    /// The time they took, waits for room included.
    pub send_time: u64,
    /// Reads of the chip that gave a frame to the stack.
    pub received: u32,
    /// Their time.
    pub receive_time: u64,
    /// Reads of the chip that gave nothing to the stack.
    pub empty_polls: u32,
    /// Their time.
    pub empty_poll_time: u64,
    /// Times the stack was told it cannot send, as the firmware had no room.
    pub refused: u32,
    /// TCP segments with data that came for this station.
    pub tcp_segments: u32,
    /// Of those, the ones that start past where the last one of their connection ended: something
    /// between was lost, or comes late. Only the latest connection is followed.
    pub tcp_gaps: u32,
    /// Of those, the ones that start before where the last one ended: data sent again.
    pub tcp_repeats: u32,
    /// The chip's own share of the times above.
    pub chip: Timing,
}

/// A joined chip as `smoltcp` sees it.
pub struct WifiDevice<'a> {
    chip: &'a mut Bcm4334,
    supplicant: &'a mut Supplicant,
    report: fn(Activity),
    mct: Mct,
    received: [u8; FRAME_MAX],
    sent: [u8; FRAME_MAX],
    failure: Option<Error>,
    echo_requests: u32,
    stats: Stats,
    flow: Option<Flow>,
}

/// Where the TCP data of a connection that came in last should go on.
#[derive(Clone, Copy)]
struct Flow {
    /// The sender's address and port, and this station's port.
    key: (u32, u16, u16),
    next: u32,
}

impl<'a> WifiDevice<'a> {
    /// Takes a chip that [`Bcm4334::connect`] has joined to a network, and the handshake state
    /// that did it. `report` hears of what happens to the connection.
    /// `mct` times what it does: see [`stats`](Self::stats).
    pub const fn new(
        chip: &'a mut Bcm4334,
        supplicant: &'a mut Supplicant,
        report: fn(Activity),
        mct: Mct,
    ) -> Self {
        Self {
            chip,
            supplicant,
            report,
            mct,
            received: [0; FRAME_MAX],
            sent: [0; FRAME_MAX],
            failure: None,
            echo_requests: 0,
            flow: None,
            stats: Stats {
                sent: 0,
                sent_bytes: 0,
                send_time: 0,
                received: 0,
                receive_time: 0,
                empty_polls: 0,
                empty_poll_time: 0,
                refused: 0,
                tcp_segments: 0,
                tcp_gaps: 0,
                tcp_repeats: 0,
                chip: Timing {
                    frame_writes: 0,
                    frame_reads: 0,
                    room_waits: 0,
                },
            },
        }
    }

    /// The chip, to ask it something. Its answers do not come through the stack, so a frame that
    /// comes meanwhile is lost.
    pub fn chip(&mut self) -> &mut Bcm4334 {
        self.chip
    }

    /// What it has done so far.
    pub fn stats(&self) -> Stats {
        Stats {
            chip: self.chip.timing(),
            ..self.stats
        }
    }

    /// What stopped the device, if anything has. It does nothing more after that.
    pub const fn failure(&self) -> Option<Error> {
        self.failure
    }

    /// How many pings addressed to this station have arrived. The stack answers them on its own
    /// and does not count them, so this counts what came in rather than what it sent back.
    pub const fn echo_requests(&self) -> u32 {
        self.echo_requests
    }
}

/// Whether a frame is a ping sent to this station alone.
fn is_echo_request(frame: &[u8]) -> bool {
    let Ok(ethernet) = EthernetFrame::new_checked(frame) else {
        return false;
    };
    if ethernet.ethertype() != EthernetProtocol::Ipv4 || !ethernet.dst_addr().is_unicast() {
        return false;
    }
    let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
        return false;
    };
    if ip.next_header() != IpProtocol::Icmp {
        return false;
    }
    Icmpv4Packet::new_checked(ip.payload()).is_ok_and(|icmp| icmp.msg_type() == Icmpv4Message::EchoRequest)
}

/// The connection, first byte and length of the data of a TCP segment sent to this station
/// alone, if the frame is one.
fn tcp_data(frame: &[u8]) -> Option<((u32, u16, u16), u32, u32)> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.ethertype() != EthernetProtocol::Ipv4 || !ethernet.dst_addr().is_unicast() {
        return None;
    }
    let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    if ip.next_header() != IpProtocol::Tcp {
        return None;
    }
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    let length = tcp.payload().len() as u32;
    if length == 0 {
        return None;
    }
    let key = (u32::from_be_bytes(ip.src_addr().octets()), tcp.src_port(), tcp.dst_port());
    Some((key, tcp.seq_number().0 as u32, length))
}

/// Counts a segment of TCP data against what its connection sent before.
fn follow(flow: &mut Option<Flow>, stats: &mut Stats, frame: &[u8]) {
    let Some((key, start, length)) = tcp_data(frame) else {
        return;
    };
    stats.tcp_segments += 1;
    let end = start.wrapping_add(length);
    let next = match *flow {
        Some(known) if known.key == key => {
            // Sequence numbers wrap: the sign of the difference says which comes first.
            let ahead = start.wrapping_sub(known.next) as i32;
            if ahead > 0 {
                stats.tcp_gaps += 1;
                end
            } else if ahead < 0 {
                stats.tcp_repeats += 1;
                if (end.wrapping_sub(known.next) as i32) > 0 { end } else { known.next }
            } else {
                end
            }
        }
        _ => end,
    };
    *flow = Some(Flow { key, next });
}

/// Lets `smoltcp` read a frame.
pub struct RxToken<'a> {
    frame: &'a [u8],
}

impl phy::RxToken for RxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.frame)
    }
}

/// Lets `smoltcp` write a frame.
pub struct TxToken<'a> {
    chip: &'a mut Bcm4334,
    buffer: &'a mut [u8; FRAME_MAX],
    failure: &'a mut Option<Error>,
    stats: &'a mut Stats,
    mct: Mct,
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, length: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // The stack asks for no more than `capabilities` allows.
        let frame = &mut self.buffer[..length];
        let result = f(frame);
        let begin = self.mct.count();
        if let Err(error) = self.chip.send_ethernet(frame) {
            *self.failure = Some(error);
        }
        self.stats.sent += 1;
        self.stats.sent_bytes += length as u64;
        self.stats.send_time += self.mct.count() - begin;
        result
    }
}

impl Device for WifiDevice<'_> {
    type RxToken<'a>
        = RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.failure.is_some() {
            return None;
        }
        let begin = self.mct.count();
        let polled = self.chip.poll(self.supplicant, &mut self.received, self.report);
        let time = self.mct.count() - begin;
        let length = match polled {
            Ok(Some(length)) => {
                self.stats.received += 1;
                self.stats.receive_time += time;
                length
            }
            Ok(None) => {
                self.stats.empty_polls += 1;
                self.stats.empty_poll_time += time;
                return None;
            }
            Err(error) => {
                self.failure = Some(error);
                return None;
            }
        };
        let frame = &self.received[..length];
        if is_echo_request(frame) {
            self.echo_requests += 1;
        }
        follow(&mut self.flow, &mut self.stats, frame);
        let transmit = TxToken {
            chip: self.chip,
            buffer: &mut self.sent,
            failure: &mut self.failure,
            stats: &mut self.stats,
            mct: self.mct,
        };
        Some((RxToken { frame }, transmit))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Without room in the firmware, sending would wait for it and drop the frames that come
        // meanwhile, such as the acknowledgements of what is being sent. Saying no instead keeps
        // the stack reading: each frame read updates the room.
        if self.failure.is_some() {
            return None;
        }
        if !self.chip.can_send() {
            self.stats.refused += 1;
            return None;
        }
        Some(TxToken {
            chip: self.chip,
            buffer: &mut self.sent,
            failure: &mut self.failure,
            stats: &mut self.stats,
            mct: self.mct,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = FRAME_MAX;
        // No `max_burst_size`: the firmware queues what comes in, and this reads it a frame at a
        // time, so there is no burst to fit. `smoltcp` would shrink every window it offers to that
        // many frames (`iface/packet.rs:155`): `Some(1)` held uploads to 1474 bytes in flight.
        capabilities
    }
}
