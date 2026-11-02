//! Connects a joined [`Bcm4334`] to the `smoltcp` network stack, as its Ethernet device.
//!
//! Reading the device also serves the connection, so a key handshake from the access point gets
//! its answer as long as the stack is polled. `smoltcp` has no way to take an error from a device,
//! so a failure stops the device, and [`failure`](WifiDevice::failure) tells what it was.

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetFrame, EthernetProtocol, Icmpv4Message, Icmpv4Packet, IpProtocol, Ipv4Packet};

use super::bcm4334::{Activity, Bcm4334, Error};
use crate::wpa::Supplicant;

/// Bytes of the longest Ethernet frame on a network with the usual packet size, 1500 bytes.
const FRAME_MAX: usize = 1514;

/// A joined chip as `smoltcp` sees it.
pub struct WifiDevice<'a> {
    chip: &'a mut Bcm4334,
    supplicant: &'a mut Supplicant,
    report: fn(Activity),
    received: [u8; FRAME_MAX],
    sent: [u8; FRAME_MAX],
    failure: Option<Error>,
    echo_requests: u32,
}

impl<'a> WifiDevice<'a> {
    /// Takes a chip that [`Bcm4334::connect`] has joined to a network, and the handshake state
    /// that did it. `report` hears of what happens to the connection.
    pub const fn new(chip: &'a mut Bcm4334, supplicant: &'a mut Supplicant, report: fn(Activity)) -> Self {
        Self {
            chip,
            supplicant,
            report,
            received: [0; FRAME_MAX],
            sent: [0; FRAME_MAX],
            failure: None,
            echo_requests: 0,
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
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, length: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // The stack asks for no more than `capabilities` allows.
        let frame = &mut self.buffer[..length];
        let result = f(frame);
        if let Err(error) = self.chip.send_ethernet(frame) {
            *self.failure = Some(error);
        }
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
        let length = match self.chip.poll(self.supplicant, &mut self.received, self.report) {
            Ok(length) => length?,
            Err(error) => {
                self.failure = Some(error);
                return None;
            }
        };
        let frame = &self.received[..length];
        if is_echo_request(frame) {
            self.echo_requests += 1;
        }
        let transmit = TxToken {
            chip: self.chip,
            buffer: &mut self.sent,
            failure: &mut self.failure,
        };
        Some((RxToken { frame }, transmit))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.failure.is_some() {
            return None;
        }
        Some(TxToken {
            chip: self.chip,
            buffer: &mut self.sent,
            failure: &mut self.failure,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = FRAME_MAX;
        capabilities.max_burst_size = Some(1);
        capabilities
    }
}
