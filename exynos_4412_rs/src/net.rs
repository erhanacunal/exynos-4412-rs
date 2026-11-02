//! Just enough of IPv4 to get an address and answer and send pings: ARP (RFC 826), IPv4 (RFC 791),
//! ICMP echo (RFC 792), UDP (RFC 768) and a DHCP client's messages (RFC 2131, RFC 2132).
//!
//! It only transforms bytes: it builds whole Ethernet frames and reads them. It keeps no state, and
//! handles no fragments, no IP options on what it sends, and no TCP. Anything malformed reads as
//! [`Packet::Other`], so a frame from the air cannot make it panic.

/// An Ethernet address.
pub type Mac = [u8; 6];
/// An IPv4 address.
pub type Ip = [u8; 4];

/// Where every station listens.
pub const BROADCAST_MAC: Mac = [0xFF; 6];
const BROADCAST_IP: Ip = [0xFF; 4];
const UNSPECIFIED_IP: Ip = [0; 4];

/// `include/uapi/linux/if_ether.h:54`.
const ETHERTYPE_ARP: u16 = 0x0806;
/// `include/uapi/linux/if_ether.h:52`.
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERNET_HEADER: usize = 14;

/// RFC 826: Ethernet and IPv4, with their address lengths.
const ARP_PREFIX: [u8; 6] = [0, 1, 0x08, 0x00, 6, 4];
const ARP_LENGTH: usize = 28;
/// RFC 826.
const ARP_REQUEST: u16 = 1;
/// RFC 826.
const ARP_REPLY: u16 = 2;

const IPV4_HEADER: usize = 20;
/// Version 4, and 5 words of header.
const IPV4_VERSION_LENGTH: u8 = 0x45;
/// "Don't fragment", in the flags and offset field: RFC 791, 3.1.
const IPV4_DONT_FRAGMENT: u16 = 0x4000;
/// Hops a frame may take. RFC 1122, 3.2.1.7 asks for at least this kind of value.
const IPV4_TTL: u8 = 64;
/// "More fragments" and the fragment's offset: RFC 791, 3.1.
const IPV4_FRAGMENTED: u16 = 0x3FFF;
/// `include/uapi/linux/in.h:31`.
const PROTOCOL_ICMP: u8 = 1;
/// `include/uapi/linux/in.h:43`.
const PROTOCOL_UDP: u8 = 17;

/// `include/uapi/linux/icmp.h:26`.
const ICMP_ECHO_REPLY: u8 = 0;
/// `include/uapi/linux/icmp.h:30`.
const ICMP_ECHO_REQUEST: u8 = 8;
/// Type, code, checksum, identifier and sequence.
const ICMP_HEADER: usize = 8;

const UDP_HEADER: usize = 8;

/// RFC 2131, section 4.1.
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
/// RFC 2131, 2: op of a request and a reply.
const BOOT_REQUEST: u8 = 1;
const BOOT_REPLY: u8 = 2;
/// Offsets in a DHCP message: RFC 2131, figure 1.
const DHCP_ID: usize = 4;
const DHCP_FLAGS: usize = 10;
const DHCP_YOUR_ADDRESS: usize = 16;
const DHCP_CLIENT_MAC: usize = 28;
const DHCP_COOKIE: usize = 236;
const DHCP_OPTIONS: usize = 240;
/// Asks the server to broadcast its replies, as a client with no address yet may not get others:
/// RFC 2131, figure 2.
const DHCP_FLAG_BROADCAST: u16 = 0x8000;
/// RFC 2131, 3.
const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// The shortest message a BOOTP relay must take: RFC 1542, 2.1.
const DHCP_MIN_LENGTH: usize = 300;
/// RFC 2132 numbers these.
const OPTION_PAD: u8 = 0;
const OPTION_MASK: u8 = 1;
const OPTION_ROUTER: u8 = 3;
const OPTION_DNS: u8 = 6;
const OPTION_REQUESTED_IP: u8 = 50;
const OPTION_LEASE: u8 = 51;
const OPTION_TYPE: u8 = 53;
const OPTION_SERVER: u8 = 54;
const OPTION_LIST: u8 = 55;
const OPTION_END: u8 = 255;
const TYPE_DISCOVER: u8 = 1;
const TYPE_OFFER: u8 = 2;
const TYPE_REQUEST: u8 = 3;
const TYPE_ACK: u8 = 5;
const TYPE_NAK: u8 = 6;

/// Bytes of the longest Ethernet frame on a network with the usual packet size, 1500 bytes.
pub const FRAME_MAX: usize = 1514;

/// The Internet checksum of the concatenation of `parts`: RFC 1071. Summing a block with its own
/// checksum gives 0.
pub fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut odd = None;
    for part in parts {
        let mut bytes = *part;
        if let Some(first) = odd.take()
            && let [second, rest @ ..] = bytes
        {
            sum += u32::from(u16::from_be_bytes([first, *second]));
            bytes = rest;
        }
        let (words, rest) = bytes.as_chunks::<2>();
        for word in words {
            sum += u32::from(u16::from_be_bytes(*word));
        }
        if let [last] = rest {
            odd = Some(*last);
        }
    }
    if let Some(last) = odd {
        sum += u32::from(u16::from_be_bytes([last, 0]));
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// A frame as far as it was understood.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    /// ARP.
    Arp {
        /// Whether it is a request, not a reply.
        request: bool,
        /// Who sent it.
        sender_mac: Mac,
        /// The address it says it has.
        sender_ip: Ip,
        /// The address it is about.
        target_ip: Ip,
    },
    /// An ICMP echo request.
    EchoRequest {
        /// Who sent it.
        source_mac: Mac,
        /// Who sent it.
        source_ip: Ip,
        /// Who it is for.
        destination_ip: Ip,
        /// The identifier, sequence number and data to return.
        rest: &'a [u8],
    },
    /// An ICMP echo reply.
    EchoReply {
        /// Who sent it.
        source_ip: Ip,
        /// The identifier it returns.
        id: u16,
        /// The sequence number it returns.
        sequence: u16,
    },
    /// A UDP datagram.
    Udp {
        /// Its destination port.
        destination_port: u16,
        /// Its contents.
        data: &'a [u8],
    },
    /// Anything else, or anything malformed.
    Other,
}

/// Reads a frame. A broken checksum reads as [`Packet::Other`].
pub fn parse(frame: &[u8]) -> Packet<'_> {
    parse_checked(frame).unwrap_or(Packet::Other)
}

fn parse_checked(frame: &[u8]) -> Option<Packet<'_>> {
    let payload = frame.get(ETHERNET_HEADER..)?;
    let source_mac = array(frame, 6)?;
    match u16::from_be_bytes(array(frame, 12)?) {
        ETHERTYPE_ARP => parse_arp(payload),
        ETHERTYPE_IPV4 => parse_ipv4(payload, source_mac),
        _ => None,
    }
}

fn array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn parse_arp(arp: &[u8]) -> Option<Packet<'_>> {
    if arp.len() < ARP_LENGTH || arp[..ARP_PREFIX.len()] != ARP_PREFIX {
        return None;
    }
    let operation = u16::from_be_bytes(array(arp, 6)?);
    if operation != ARP_REQUEST && operation != ARP_REPLY {
        return None;
    }
    Some(Packet::Arp {
        request: operation == ARP_REQUEST,
        sender_mac: array(arp, 8)?,
        sender_ip: array(arp, 14)?,
        target_ip: array(arp, 24)?,
    })
}

fn parse_ipv4(packet: &[u8], source_mac: Mac) -> Option<Packet<'_>> {
    let first = *packet.first()?;
    let header = usize::from(first & 0xF) * 4;
    if first >> 4 != 4 || header < IPV4_HEADER {
        return None;
    }
    // Ethernet pads short frames, so the total length decides where this one ends.
    let total = usize::from(u16::from_be_bytes(array(packet, 2)?));
    let packet = packet.get(..total)?;
    let head = packet.get(..header)?;
    if checksum(&[head]) != 0 || u16::from_be_bytes(array(head, 6)?) & IPV4_FRAGMENTED != 0 {
        return None;
    }
    let source_ip = array(head, 12)?;
    let destination_ip = array(head, 16)?;
    let body = &packet[header..];
    match head[9] {
        PROTOCOL_ICMP => parse_icmp(body, source_mac, source_ip, destination_ip),
        PROTOCOL_UDP => parse_udp(body, source_ip, destination_ip),
        _ => None,
    }
}

fn parse_icmp(body: &[u8], source_mac: Mac, source_ip: Ip, destination_ip: Ip) -> Option<Packet<'_>> {
    if body.len() < ICMP_HEADER || checksum(&[body]) != 0 || body[1] != 0 {
        return None;
    }
    match body[0] {
        ICMP_ECHO_REQUEST => Some(Packet::EchoRequest {
            source_mac,
            source_ip,
            destination_ip,
            rest: &body[4..],
        }),
        ICMP_ECHO_REPLY => Some(Packet::EchoReply {
            source_ip,
            id: u16::from_be_bytes(array(body, 4)?),
            sequence: u16::from_be_bytes(array(body, 6)?),
        }),
        _ => None,
    }
}

fn parse_udp(body: &[u8], source_ip: Ip, destination_ip: Ip) -> Option<Packet<'_>> {
    let length = usize::from(u16::from_be_bytes(array(body, 4)?));
    if length < UDP_HEADER {
        return None;
    }
    let datagram = body.get(..length)?;
    // 0 means the sender computed none: RFC 768.
    let sum = u16::from_be_bytes(array(datagram, 6)?);
    if sum != 0 && checksum(&[&udp_pseudo_header(source_ip, destination_ip, length), datagram]) != 0 {
        return None;
    }
    Some(Packet::Udp {
        destination_port: u16::from_be_bytes(array(datagram, 2)?),
        data: &datagram[UDP_HEADER..],
    })
}

fn udp_pseudo_header(source: Ip, destination: Ip, length: usize) -> [u8; 12] {
    let mut header = [0; 12];
    header[..4].copy_from_slice(&source);
    header[4..8].copy_from_slice(&destination);
    header[9] = PROTOCOL_UDP;
    header[10..12].copy_from_slice(&(length as u16).to_be_bytes());
    header
}

fn write_ethernet(output: &mut [u8], destination: Mac, source: Mac, ethertype: u16) {
    output[..6].copy_from_slice(&destination);
    output[6..12].copy_from_slice(&source);
    output[12..14].copy_from_slice(&ethertype.to_be_bytes());
}

/// An ARP message. Returns the frame's length. `output` needs 42 bytes.
fn arp(
    output: &mut [u8],
    destination: Mac,
    operation: u16,
    own_mac: Mac,
    own_ip: Ip,
    target_mac: Mac,
    target_ip: Ip,
) -> usize {
    write_ethernet(output, destination, own_mac, ETHERTYPE_ARP);
    let body = &mut output[ETHERNET_HEADER..ETHERNET_HEADER + ARP_LENGTH];
    body[..6].copy_from_slice(&ARP_PREFIX);
    body[6..8].copy_from_slice(&operation.to_be_bytes());
    body[8..14].copy_from_slice(&own_mac);
    body[14..18].copy_from_slice(&own_ip);
    body[18..24].copy_from_slice(&target_mac);
    body[24..28].copy_from_slice(&target_ip);
    ETHERNET_HEADER + ARP_LENGTH
}

/// Asks who has an address. Returns the frame's length.
pub fn arp_request(output: &mut [u8], own_mac: Mac, own_ip: Ip, target_ip: Ip) -> usize {
    arp(output, BROADCAST_MAC, ARP_REQUEST, own_mac, own_ip, [0; 6], target_ip)
}

/// Answers that this station has its address. Returns the frame's length.
pub fn arp_reply(output: &mut [u8], own_mac: Mac, own_ip: Ip, target_mac: Mac, target_ip: Ip) -> usize {
    arp(output, target_mac, ARP_REPLY, own_mac, own_ip, target_mac, target_ip)
}

/// Writes Ethernet and IPv4 headers for `body_length` bytes that already follow them. Returns the
/// frame's length.
fn finish_ipv4(
    output: &mut [u8],
    addresses: Addresses,
    protocol: u8,
    body_length: usize,
) -> usize {
    write_ethernet(output, addresses.destination_mac, addresses.source_mac, ETHERTYPE_IPV4);
    let total = IPV4_HEADER + body_length;
    let head = &mut output[ETHERNET_HEADER..ETHERNET_HEADER + IPV4_HEADER];
    head.fill(0);
    head[0] = IPV4_VERSION_LENGTH;
    head[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    head[6..8].copy_from_slice(&IPV4_DONT_FRAGMENT.to_be_bytes());
    head[8] = IPV4_TTL;
    head[9] = protocol;
    head[12..16].copy_from_slice(&addresses.source_ip);
    head[16..20].copy_from_slice(&addresses.destination_ip);
    let sum = checksum(&[head]);
    head[10..12].copy_from_slice(&sum.to_be_bytes());
    ETHERNET_HEADER + total
}

/// Who a frame goes from and to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addresses {
    /// The next hop's Ethernet address.
    pub destination_mac: Mac,
    /// This station's.
    pub source_mac: Mac,
    /// The final destination.
    pub destination_ip: Ip,
    /// This station's.
    pub source_ip: Ip,
}

fn icmp(output: &mut [u8], addresses: Addresses, kind: u8, rest: &[u8]) -> Option<usize> {
    let start = ETHERNET_HEADER + IPV4_HEADER;
    let length = 4 + rest.len();
    let body = output.get_mut(start..start + length)?;
    body[0] = kind;
    body[1] = 0;
    body[2..4].fill(0);
    body[4..].copy_from_slice(rest);
    let sum = checksum(&[body]);
    body[2..4].copy_from_slice(&sum.to_be_bytes());
    Some(finish_ipv4(output, addresses, PROTOCOL_ICMP, length))
}

/// An echo request with a pattern as its data. Returns the frame's length.
pub fn echo_request(output: &mut [u8], addresses: Addresses, id: u16, sequence: u16) -> usize {
    let mut rest = [0; 4 + 56];
    rest[..2].copy_from_slice(&id.to_be_bytes());
    rest[2..4].copy_from_slice(&sequence.to_be_bytes());
    for (index, byte) in rest[4..].iter_mut().enumerate() {
        *byte = index as u8;
    }
    icmp(output, addresses, ICMP_ECHO_REQUEST, &rest).unwrap_or(0)
}

/// An echo reply returning `rest` from [`Packet::EchoRequest`]. Returns the frame's length, or
/// `None` if it does not fit in `output`.
pub fn echo_reply(output: &mut [u8], addresses: Addresses, rest: &[u8]) -> Option<usize> {
    icmp(output, addresses, ICMP_ECHO_REPLY, rest)
}

/// What a DHCP server said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DhcpReply {
    /// Whether it is an offer.
    pub offer: bool,
    /// Whether it is an acknowledgement.
    pub ack: bool,
    /// Whether it refuses: then start over.
    pub nak: bool,
    /// The address for this station.
    pub address: Ip,
    /// The server, which must be named to it again.
    pub server: Ip,
    /// The subnet mask.
    pub mask: Ip,
    /// The first router.
    pub router: Ip,
    /// The first name server.
    pub dns: Ip,
    /// How long the address lasts, in seconds.
    pub lease_seconds: u32,
}

/// A client's message, DISCOVER or REQUEST. Returns the frame's length. `output` needs 342 bytes.
fn dhcp_message(output: &mut [u8], own_mac: Mac, id: u32, kind: u8, wanted: Option<(Ip, Ip)>) -> usize {
    let start = ETHERNET_HEADER + IPV4_HEADER + UDP_HEADER;
    let message = &mut output[start..start + DHCP_MIN_LENGTH];
    message.fill(0);
    message[0] = BOOT_REQUEST;
    message[1] = 1; // Ethernet: RFC 1700.
    message[2] = 6; // Bytes of its address.
    message[DHCP_ID..DHCP_ID + 4].copy_from_slice(&id.to_be_bytes());
    message[DHCP_FLAGS..DHCP_FLAGS + 2].copy_from_slice(&DHCP_FLAG_BROADCAST.to_be_bytes());
    message[DHCP_CLIENT_MAC..DHCP_CLIENT_MAC + 6].copy_from_slice(&own_mac);
    message[DHCP_COOKIE..DHCP_OPTIONS].copy_from_slice(&COOKIE);

    let mut at = DHCP_OPTIONS;
    let mut option = |code: u8, value: &[u8]| {
        message[at] = code;
        message[at + 1] = value.len() as u8;
        message[at + 2..at + 2 + value.len()].copy_from_slice(value);
        at += 2 + value.len();
    };
    option(OPTION_TYPE, &[kind]);
    if let Some((address, server)) = wanted {
        option(OPTION_REQUESTED_IP, &address);
        option(OPTION_SERVER, &server);
    }
    option(OPTION_LIST, &[OPTION_MASK, OPTION_ROUTER, OPTION_DNS, OPTION_LEASE]);
    message[at] = OPTION_END;

    let udp_length = UDP_HEADER + DHCP_MIN_LENGTH;
    let udp = &mut output[ETHERNET_HEADER + IPV4_HEADER..start + DHCP_MIN_LENGTH];
    udp[..2].copy_from_slice(&DHCP_CLIENT_PORT.to_be_bytes());
    udp[2..4].copy_from_slice(&DHCP_SERVER_PORT.to_be_bytes());
    udp[4..6].copy_from_slice(&(udp_length as u16).to_be_bytes());
    udp[6..8].fill(0);
    let sum = checksum(&[&udp_pseudo_header(UNSPECIFIED_IP, BROADCAST_IP, udp_length), udp]);
    // 0 would say "none": RFC 768.
    let sum = if sum == 0 { 0xFFFF } else { sum };
    udp[6..8].copy_from_slice(&sum.to_be_bytes());

    let addresses = Addresses {
        destination_mac: BROADCAST_MAC,
        source_mac: own_mac,
        destination_ip: BROADCAST_IP,
        source_ip: UNSPECIFIED_IP,
    };
    finish_ipv4(output, addresses, PROTOCOL_UDP, udp_length)
}

/// Asks for an address. Returns the frame's length. `output` needs 342 bytes.
pub fn dhcp_discover(output: &mut [u8], own_mac: Mac, id: u32) -> usize {
    dhcp_message(output, own_mac, id, TYPE_DISCOVER, None)
}

/// Takes an offered address. Returns the frame's length. `output` needs 342 bytes.
pub fn dhcp_request(output: &mut [u8], own_mac: Mac, id: u32, address: Ip, server: Ip) -> usize {
    dhcp_message(output, own_mac, id, TYPE_REQUEST, Some((address, server)))
}

/// Reads a UDP datagram as a DHCP server's answer to this station's message `id`.
pub fn dhcp_reply(destination_port: u16, data: &[u8], own_mac: Mac, id: u32) -> Option<DhcpReply> {
    if destination_port != DHCP_CLIENT_PORT
        || *data.first()? != BOOT_REPLY
        || array::<4>(data, DHCP_ID)? != id.to_be_bytes()
        || array::<6>(data, DHCP_CLIENT_MAC)? != own_mac
        || array::<4>(data, DHCP_COOKIE)? != COOKIE
    {
        return None;
    }
    let mut reply = DhcpReply {
        address: array(data, DHCP_YOUR_ADDRESS)?,
        ..DhcpReply::default()
    };
    let mut rest = data.get(DHCP_OPTIONS..)?;
    while let [code, tail @ ..] = rest {
        match *code {
            OPTION_END => break,
            OPTION_PAD => {
                rest = tail;
                continue;
            }
            _ => {}
        }
        let (length, tail) = tail.split_first()?;
        let value = tail.get(..usize::from(*length))?;
        match *code {
            OPTION_TYPE => match *value.first()? {
                TYPE_OFFER => reply.offer = true,
                TYPE_ACK => reply.ack = true,
                TYPE_NAK => reply.nak = true,
                _ => return None,
            },
            OPTION_SERVER => reply.server = array(value, 0)?,
            OPTION_MASK => reply.mask = array(value, 0)?,
            OPTION_ROUTER => reply.router = array(value, 0)?,
            OPTION_DNS => reply.dns = array(value, 0)?,
            OPTION_LEASE => reply.lease_seconds = u32::from_be_bytes(array(value, 0)?),
            _ => {}
        }
        rest = &tail[usize::from(*length)..];
    }
    (reply.offer || reply.ack || reply.nak).then_some(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: Mac = [2, 0, 0, 0, 0, 1];
    const PEER: Mac = [2, 0, 0, 0, 0, 2];
    const OWN_IP: Ip = [192, 168, 1, 10];
    const PEER_IP: Ip = [192, 168, 1, 1];
    const ADDRESSES: Addresses = Addresses {
        destination_mac: PEER,
        source_mac: OWN,
        destination_ip: PEER_IP,
        source_ip: OWN_IP,
    };

    #[test]
    fn checksum_of_a_known_header() {
        let header = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xC0, 0xA8, 0x00, 0x01,
            0xC0, 0xA8, 0x00, 0xC7,
        ];
        assert_eq!(checksum(&[&header]), 0xB861);
    }

    #[test]
    fn checksum_does_not_depend_on_how_it_is_split() {
        let data: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let whole = checksum(&[&data]);
        assert_eq!(checksum(&[&data[..3], &data[3..4], &data[4..]]), whole);
    }

    #[test]
    fn udp_checksum_matches_python() {
        let datagram = [0, 68, 0, 67, 0, 11, 0, 0, b'a', b'b', b'c'];
        let sum = checksum(&[&udp_pseudo_header(UNSPECIFIED_IP, BROADCAST_IP, 11), &datagram]);
        assert_eq!(sum, 0x3AEF);
    }

    #[test]
    fn arp_round_trip() {
        let mut frame = [0; 64];
        let length = arp_request(&mut frame, OWN, OWN_IP, PEER_IP);
        assert_eq!(length, 42);
        assert_eq!(
            parse(&frame[..length]),
            Packet::Arp {
                request: true,
                sender_mac: OWN,
                sender_ip: OWN_IP,
                target_ip: PEER_IP,
            },
        );
        let length = arp_reply(&mut frame, OWN, OWN_IP, PEER, PEER_IP);
        assert_eq!(frame[..6], PEER);
        assert!(matches!(parse(&frame[..length]), Packet::Arp { request: false, .. }));
    }

    #[test]
    fn echo_round_trip() {
        let mut frame = [0; FRAME_MAX];
        let length = echo_request(&mut frame, ADDRESSES, 0x1234, 7);
        let Packet::EchoRequest {
            source_mac,
            source_ip,
            destination_ip,
            rest,
        } = parse(&frame[..length])
        else {
            panic!("not an echo request");
        };
        assert_eq!((source_mac, source_ip, destination_ip), (OWN, OWN_IP, PEER_IP));
        let mut answer = [0; FRAME_MAX];
        let length = echo_reply(&mut answer, ADDRESSES, rest).unwrap();
        assert_eq!(
            parse(&answer[..length]),
            Packet::EchoReply {
                source_ip: OWN_IP,
                id: 0x1234,
                sequence: 7,
            },
        );
    }

    #[test]
    fn broken_checksums_are_refused() {
        let mut frame = [0; FRAME_MAX];
        let length = echo_request(&mut frame, ADDRESSES, 1, 1);
        frame[length - 1] ^= 1;
        assert_eq!(parse(&frame[..length]), Packet::Other);
        frame[length - 1] ^= 1;
        frame[ETHERNET_HEADER + 8] ^= 1;
        assert_eq!(parse(&frame[..length]), Packet::Other);
    }

    #[test]
    fn padding_after_the_packet_is_ignored() {
        let mut frame = [0; FRAME_MAX];
        let length = echo_request(&mut frame, ADDRESSES, 1, 1);
        assert!(matches!(parse(&frame[..length + 10]), Packet::EchoRequest { .. }));
    }

    #[test]
    fn dhcp_messages_are_well_formed() {
        let mut frame = [0; 400];
        let length = dhcp_request(&mut frame, OWN, 0xAABB_CCDD, OWN_IP, PEER_IP);
        assert_eq!(length, 342);
        let Packet::Udp {
            destination_port,
            data,
        } = parse(&frame[..length])
        else {
            panic!("not a datagram");
        };
        assert_eq!(destination_port, DHCP_SERVER_PORT);
        assert_eq!(data[0], BOOT_REQUEST);
        assert_eq!(data[DHCP_ID..DHCP_ID + 4], [0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(
            data[DHCP_OPTIONS..DHCP_OPTIONS + 15],
            [53, 1, 3, 50, 4, 192, 168, 1, 10, 54, 4, 192, 168, 1, 1],
        );
    }

    fn server_message(kind: u8, id: u32) -> [u8; 300] {
        let mut data = [0; 300];
        data[0] = BOOT_REPLY;
        data[DHCP_ID..DHCP_ID + 4].copy_from_slice(&id.to_be_bytes());
        data[DHCP_YOUR_ADDRESS..DHCP_YOUR_ADDRESS + 4].copy_from_slice(&OWN_IP);
        data[DHCP_CLIENT_MAC..DHCP_CLIENT_MAC + 6].copy_from_slice(&OWN);
        data[DHCP_COOKIE..DHCP_OPTIONS].copy_from_slice(&COOKIE);
        let options: [u8; 34] = [
            53, 1, kind, 0, 54, 4, 192, 168, 1, 1, 1, 4, 255, 255, 255, 0, 3, 4, 192, 168, 1, 1, 6, 4, 8, 8, 8, 8,
            51, 4, 0, 1, 0x51, 0x80,
        ];
        data[DHCP_OPTIONS..DHCP_OPTIONS + options.len()].copy_from_slice(&options);
        data[DHCP_OPTIONS + options.len()] = OPTION_END;
        data
    }

    #[test]
    fn dhcp_offer_is_read() {
        let data = server_message(TYPE_OFFER, 5);
        let reply = dhcp_reply(DHCP_CLIENT_PORT, &data, OWN, 5).unwrap();
        assert!(reply.offer && !reply.ack && !reply.nak);
        assert_eq!(reply.address, OWN_IP);
        assert_eq!(reply.server, PEER_IP);
        assert_eq!(reply.mask, [255, 255, 255, 0]);
        assert_eq!(reply.router, PEER_IP);
        assert_eq!(reply.dns, [8, 8, 8, 8]);
        assert_eq!(reply.lease_seconds, 86400);
    }

    #[test]
    fn dhcp_for_someone_else_is_ignored() {
        let data = server_message(TYPE_ACK, 5);
        assert!(dhcp_reply(DHCP_CLIENT_PORT, &data, OWN, 6).is_none());
        assert!(dhcp_reply(DHCP_CLIENT_PORT, &data, PEER, 5).is_none());
        assert!(dhcp_reply(DHCP_SERVER_PORT, &data, OWN, 5).is_none());
    }

    #[test]
    fn cut_frames_do_not_panic() {
        let mut frame = [0; FRAME_MAX];
        let lengths = [
            echo_request(&mut frame, ADDRESSES, 1, 1),
            arp_request(&mut frame, OWN, OWN_IP, PEER_IP),
            dhcp_discover(&mut frame, OWN, 1),
        ];
        for total in lengths {
            for length in 0..total {
                parse(&frame[..length]);
            }
        }
        let data = server_message(TYPE_OFFER, 5);
        for length in 0..data.len() {
            dhcp_reply(DHCP_CLIENT_PORT, &data[..length], OWN, 5);
        }
    }
}
