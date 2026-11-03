//! BCM4334 WiFi chip test, sixth stage: loads the firmware, scans, joins a WPA2-Personal network, gets
//! an address by DHCP, pings, and speaks TCP both ways.
//!
//! Reports what U-Boot left, powers the chip up from reset, starts its SDIO bus and reads what the
//! chip says about itself. It lists the chip's cores, loads the firmware and NVRAM and checks them
//! by reading them back, runs the firmware, asks it for its version and its MAC address, and lists
//! the networks it can hear. Then it joins the one named at build time and hands it to `smoltcp`,
//! which gets an address and renews it, and pings the router and a public address. It opens a TCP
//! connection to a public web server, then serves HTTP on port 80 (see `http`) and answers ARP and
//! pings until told to stop.
//!
//! The firmware and NVRAM files come from `WIFI_FIRMWARE_DIR` at build time, and the network from
//! `WIFI_SSID` and `WIFI_PASSWORD`: see `build.rs`. The passphrase is never printed.
//!
//! Its handshake nonce comes from timing the scan, not from a hardware generator. This suffices to
//! test with, as each is mixed with the MAC and never repeats in practice, but is unproven as
//! a source of randomness. Do not ship it as is.
//!
//! The rails and enables it turns on stay on. Nothing here touches the eMMC, which is on another
//! controller and other pins.

#![no_std]
#![no_main]

mod http;

use exynos_4412_rs::crypto::{self, Hmac};
use exynos_4412_rs::drivers::bcm4334::{
    self, Activity, Bcm4334, Bss, CHIP_ID_4334, Error, Event, IOCTL_GET_VERSION, STATUS_SUCCESS, Security,
};
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::sdhci::Sdhci;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::wifi_device::{Stats, WifiDevice};
use exynos_4412_rs::wpa::{self, Supplicant};
use exynos_4412_rs::{BootArgs, console, mmu, print, println, soc};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities};
use smoltcp::socket::{dhcpv4, icmp, tcp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address,
};

exynos_4412_rs::entry!(main);

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/firmware.bin"));
static NVRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nvram.txt"));

/// `include/linux/mmc/sdio_ids.h:56`.
const VENDOR_BROADCOM: u16 = 0x02D0;
/// `include/linux/mmc/sdio_ids.h:61`.
const DEVICE_4334: u16 = 0x4334;
/// Reads of the ID register in the reliability check.
const ID_READS: u32 = 10_000;
/// The SDIO data rate, in MHz, from `WIFI_SDIO_MHZ` at build time. Linux ran this board at
/// [`bcm4334::BOARD_DATA_HZ`], 16 MHz. Without the variable this asks for the most of high
/// speed, 50 MHz. The firmware is read back after loading, so a bus that fails at the rate shows
/// there too.
const SDIO_MHZ: Option<&str> = option_env!("WIFI_SDIO_MHZ");
const SDIO_HZ: u32 = match SDIO_MHZ {
    Some(text) => parse_mhz(text) * 1_000_000,
    None => bcm4334::HIGH_SPEED_MAX_HZ,
};

/// A whole number of MHz, or 0 for anything else.
const fn parse_mhz(text: &str) -> u32 {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 3 {
        return 0;
    }
    let mut value = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            return 0;
        }
        value = value * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    value
}
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;
/// Networks kept from a scan.
const MAX_NETWORKS: usize = 48;
/// A full scan of both bands takes seconds. This is well over that, and not measured.
const SCAN_TIMEOUT_MS: u64 = 20_000;
/// The network to join.
const SSID: Option<&str> = option_env!("WIFI_SSID");
const PASSWORD: Option<&str> = option_env!("WIFI_PASSWORD");
/// The band to join in, `2.4` or `5`, if not whichever access point is strongest.
const BAND: Option<&str> = option_env!("WIFI_BAND");
/// Channels up to this one are in the 2.4 GHz band, the rest in 5 GHz.
const LAST_2G_CHANNEL: u8 = 14;
/// Joining scans first, then authenticates and shakes hands. Not measured.
const JOIN_TIMEOUT_MS: u64 = 30_000;
/// How long the DHCP server gets to give an address. The stack asks again on its own meanwhile.
const DHCP_TIMEOUT_MS: u64 = 30_000;
/// How long a host gets to answer a ping. The first may wait for its address to be found.
const PING_TIMEOUT_MS: u64 = 3_000;
/// Pings to each host, and the time between them.
const PINGS: u16 = 4;
const PING_INTERVAL_MS: u64 = 1_000;
/// Identifies this program's pings among the replies.
const PING_ID: u16 = 0x4534;
/// What a ping carries.
const PING_DATA: [u8; 56] = [0x5A; 56];
/// A public address, to show that traffic gets past the router: a well known name server of Google.
const PUBLIC_HOST: Ipv4Address = Ipv4Address::new(8, 8, 8, 8);
/// A public web server, to show TCP past the router: Cloudflare's, which answers on its address.
const PUBLIC_WEB: Ipv4Address = Ipv4Address::new(1, 1, 1, 1);
const PUBLIC_WEB_PORT: u16 = 80;
/// What is asked of it. Any answer will do: the test is that one comes.
const PUBLIC_WEB_REQUEST: &[u8] = b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\nConnection: close\r\n\r\n";
/// How long the connection and the answer each get. Not measured.
const TCP_TIMEOUT_MS: u64 = 5_000;
/// Bytes of the answer's first line that are kept.
const STATUS_LINE_MAX: usize = 128;
/// The first of the ports that connections from here come from: the start of the dynamic range.
const LOCAL_PORT_BASE: u16 = 49152;
/// Where the HTTP server listens, and the connections it takes there at once. Browsers open more
/// than one.
const HTTP_PORT: u16 = 80;
const SERVERS: usize = 4;
/// Each server's buffers. What it sends bounds the bytes in flight, so the speed of `/data`.
const SERVER_RX: usize = 1024;
const SERVER_TX: usize = 8192;
/// A second port, with one connection that can have far more in flight, to compare against.
const LARGE_PORT: u16 = 8080;
const LARGE_TX: usize = 65536;
/// Every server, the large one last.
const ALL_SERVERS: usize = SERVERS + 1;
const CLIENT_RX: usize = 2048;
const CLIENT_TX: usize = 256;
/// A TCP connection that hears nothing for this long is dropped.
const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Sockets: one for DHCP, one for pings, one TCP client, and the servers.
const SOCKETS: usize = 3 + ALL_SERVERS;
/// Pings each way that the ping socket can hold, and the bytes for them.
const PING_QUEUE: usize = 4;
const PING_BUFFER: usize = 256;

/// Whether `BAND` allows joining on this channel.
fn band_allows(channel: u8) -> bool {
    match BAND {
        Some("2.4") => channel <= LAST_2G_CHANNEL,
        Some("5") => channel > LAST_2G_CHANNEL,
        _ => true,
    }
}

/// Prints the rate the firmware sends at, and the signal.
fn report_link(chip: &mut Bcm4334) {
    match (chip.tx_rate_kbps(), chip.rssi()) {
        (Ok(rate), Ok(rssi)) => println!(
            "Link: sending at {}.{} Mbit/s, signal {} dBm",
            rate / 1000,
            rate % 1000 / 100,
            rssi,
        ),
        (Err(error), _) | (_, Err(error)) => println!("Link: ERROR {:?}", error),
    }
}

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

fn idle(mct: Mct) -> ! {
    println!("Done. UART: r restart, f restart into fastboot. Keys work too: see power_test");
    let uart = console::uart();
    let mut keys = PowerKeys::new();
    loop {
        keys.service(mct);
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
    }
}

fn report_diagnostics(chip: &mut Bcm4334) {
    match chip.diagnostics() {
        Ok(state) => println!(
            "Interrupt status {:#010x}, mailbox {:#010x}, clock {:#04x}, last RAM word {:#010x}",
            state.int_status, state.mailbox, state.clock, state.last_ram_word,
        ),
        Err(error) => println!("Reading the chip's state: ERROR {:?}", error),
    }
}

/// Prints what is before the first zero, as long as it is ASCII.
fn print_text(bytes: &[u8]) {
    for &byte in bytes.iter().take_while(|&&byte| byte != 0) {
        if byte.is_ascii_graphic() || byte == b' ' {
            print!("{}", byte as char);
        } else if byte != b'\n' && byte != b'\r' {
            print!("?");
        }
    }
    println!();
}

/// Asks the firmware about itself, and returns its MAC address.
fn talk(chip: &mut Bcm4334) -> Result<[u8; 6], Error> {
    let mut version = [0; 4];
    chip.ioctl(IOCTL_GET_VERSION, false, &mut version)?;
    println!("Driver interface version {}", u32::from_le_bytes(version));

    chip.disable_glomming()?;
    println!("Frame merging off");

    let mut text = [0; 256];
    chip.get_variable("ver", &mut text)?;
    print!("Firmware: ");
    print_text(&text);

    let mut address = [0; 32];
    chip.get_variable("cur_etheraddr", &mut address)?;
    println!(
        "MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        address[0], address[1], address[2], address[3], address[4], address[5],
    );
    let mut own = [0; 6];
    own.copy_from_slice(&address[..6]);
    Ok(own)
}

fn print_ssid(bss: &Bss) {
    if bss.ssid_length == 0 {
        print!("(hidden)");
    }
    for &byte in &bss.ssid[..usize::from(bss.ssid_length)] {
        print!("{}", if byte.is_ascii_graphic() || byte == b' ' { byte as char } else { '?' });
    }
}

/// The access point to join, the strongest of the named network's.
struct Target {
    bssid: [u8; 6],
    rssi: i16,
    channel: u8,
    rsn: [u8; wpa::RSN_MAX],
    rsn_length: usize,
}

/// Lists the networks, and returns the access point to join and what the timing gave for a nonce.
fn scan(chip: &mut Bcm4334, mct: Mct, own: [u8; 6]) -> Result<(Option<Target>, [u8; 20]), Error> {
    let firmware_handshake = chip.has_own_supplicant()?;
    println!("Firmware does the WPA handshake itself: {}", if firmware_handshake { "yes" } else { "no" });

    chip.prepare_radio()?;
    println!("Radio ready, scanning");

    let mut networks: [Option<Bss>; MAX_NETWORKS] = [None; MAX_NETWORKS];
    let mut results = 0u32;
    let mut dropped = 0u32;
    let mut target: Option<Target> = None;
    let mut unsupported = 0u32;
    let mut pool = Hmac::new(&own).sign(&[&mct.count().to_le_bytes()[..]]);
    let begin = mct.count();
    let status = chip.scan(SCAN_TIMEOUT_MS, |bss, rsn| {
        results += 1;
        pool = Hmac::new(&pool).sign(&[&mct.count().to_le_bytes()[..], &bss.bssid, &bss.rssi.to_le_bytes()]);
        if SSID.is_some_and(|ssid| ssid.as_bytes() == &bss.ssid[..usize::from(bss.ssid_length)])
            && band_allows(bss.channel)
        {
            if !wpa::rsn_supported(rsn) || rsn.len() > wpa::RSN_MAX {
                unsupported += 1;
            } else if target.as_ref().is_none_or(|known| bss.rssi > known.rssi) {
                let mut copy = [0; wpa::RSN_MAX];
                copy[..rsn.len()].copy_from_slice(rsn);
                target = Some(Target {
                    bssid: bss.bssid,
                    rssi: bss.rssi,
                    channel: bss.channel,
                    rsn: copy,
                    rsn_length: rsn.len(),
                });
            }
        }
        let same = networks.iter_mut().flatten().find(|known| known.bssid == bss.bssid);
        match same {
            Some(known) if known.rssi >= bss.rssi => {}
            Some(known) => *known = *bss,
            None => match networks.iter_mut().find(|slot| slot.is_none()) {
                Some(slot) => *slot = Some(*bss),
                None => dropped += 1,
            },
        }
    })?;
    println!(
        "Scan ended in {} ms with status {}: {}. {} results",
        (mct.count() - begin) / COUNTS_PER_MS,
        status,
        verdict(status == STATUS_SUCCESS),
        results,
    );

    let found = networks.iter().flatten().count();
    println!("{} networks, strongest first:", found);
    let mut printed = [false; MAX_NETWORKS];
    for _ in 0..found {
        let mut best: Option<usize> = None;
        for (i, slot) in networks.iter().enumerate() {
            if let Some(bss) = slot
                && !printed[i]
                && best.and_then(|b| networks[b]).is_none_or(|known| bss.rssi > known.rssi)
            {
                best = Some(i);
            }
        }
        let Some(i) = best else { break };
        printed[i] = true;
        let Some(bss) = networks[i] else { continue };
        let security = match bss.security {
            Security::Open => "open",
            Security::Wep => "WEP",
            Security::Wpa => "WPA",
            Security::Wpa2 => "WPA2",
        };
        print!(
            "{:4} dBm  ch {:3}  {:4}  {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  ",
            bss.rssi,
            bss.channel,
            security,
            bss.bssid[0],
            bss.bssid[1],
            bss.bssid[2],
            bss.bssid[3],
            bss.bssid[4],
            bss.bssid[5],
        );
        print_ssid(&bss);
        println!();
    }
    if dropped != 0 {
        println!("{} more networks did not fit in the table", dropped);
    }
    if unsupported != 0 {
        println!("{} results of the named network ask for what this cannot do", unsupported);
    }
    Ok((target, pool))
}

fn print_event(event: Event) {
    let name = match event.code {
        bcm4334::EVENT_SET_SSID => "joined",
        bcm4334::EVENT_AUTH => "authentication",
        bcm4334::EVENT_DEAUTH => "deauthentication sent",
        bcm4334::EVENT_DEAUTH_IND => "deauthenticated",
        bcm4334::EVENT_ASSOC => "association",
        bcm4334::EVENT_DISASSOC => "disassociation sent",
        bcm4334::EVENT_DISASSOC_IND => "disassociated",
        bcm4334::EVENT_LINK => "link",
        _ => "other",
    };
    println!(
        "Event {} ({}): status {}, reason {}, flags {:#x}",
        event.code, name, event.status, event.reason, event.flags,
    );
}

fn report(activity: Activity) {
    match activity {
        Activity::Event(event) => print_event(event),
        Activity::Handshake(message) => println!("Handshake: {:?}, answered", message),
        Activity::Ignored(error) => println!("Handshake: frame ignored, {:?}", error),
    }
}

/// The device's statistics at one moment.
#[derive(Clone, Copy, Default)]
struct Mark {
    stats: Stats,
    count: u64,
}

fn us(counts: u64) -> u64 {
    counts * 1_000_000 / u64::from(soc::MCT_CLOCK_HZ)
}

fn ms(counts: u64) -> u64 {
    counts / COUNTS_PER_MS
}

fn each_us(counts: u64, times: u32) -> u64 {
    us(counts) / u64::from(times.max(1))
}

/// Says where the time between two marks went. Other traffic in that time counts too.
fn report_transfer(before: &Mark, after: &Mark) {
    let a = &after.stats;
    let b = &before.stats;
    let total = after.count - before.count;
    let send = a.send_time - b.send_time;
    let receive = a.receive_time - b.receive_time;
    let empty = a.empty_poll_time - b.empty_poll_time;
    let sent = a.sent - b.sent;
    let received = a.received - b.received;
    let empty_polls = a.empty_polls - b.empty_polls;
    println!(
        "  {} ms: sending {} ms, receiving {} ms, empty polls {} ms, the rest {} ms",
        ms(total),
        ms(send),
        ms(receive),
        ms(empty),
        ms(total.saturating_sub(send + receive + empty)),
    );
    println!(
        "  Sent {} frames, {} bytes, {} us each: bus writes {} ms, {} us each; waiting for room {} ms",
        sent,
        a.sent_bytes - b.sent_bytes,
        each_us(send, sent),
        ms(a.chip.frame_writes - b.chip.frame_writes),
        each_us(a.chip.frame_writes - b.chip.frame_writes, sent),
        ms(a.chip.room_waits - b.chip.room_waits),
    );
    println!(
        "  Received {} frames, {} us each; {} empty polls, {} us each; bus reads of any frame {} ms",
        received,
        each_us(receive, received),
        empty_polls,
        each_us(empty, empty_polls),
        ms(a.chip.frame_reads - b.chip.frame_reads),
    );
    println!("  Stack told there was no room: {} times", a.refused - b.refused);
}

/// A connected station: its network stack and what it needs.
struct Station<'a> {
    device: WifiDevice<'a>,
    interface: Interface,
    sockets: SocketSet<'a>,
    dhcp: SocketHandle,
    ping: SocketHandle,
    client: SocketHandle,
    servers: [http::Server; ALL_SERVERS],
    /// What the device had done when each server last answered a request.
    marks: [Mark; ALL_SERVERS],
    capabilities: DeviceCapabilities,
    mct: Mct,
    mac: [u8; 6],
    /// The router DHCP last named.
    gateway: Option<Ipv4Address>,
    /// The address DHCP gave, while it holds.
    address: Option<Ipv4Address>,
    /// HTTP requests answered.
    requests: u32,
}

impl Station<'_> {
    fn now(&self) -> Instant {
        Instant::from_micros((self.mct.count() / (COUNTS_PER_MS / 1000)) as i64)
    }

    /// Runs the stack once, and applies whatever DHCP decided. Returns the new configuration's
    /// address, if there is one. An error is the connection ending.
    fn poll(&mut self) -> Result<Option<Ipv4Address>, Error> {
        let now = self.now();
        self.interface.poll(now, &mut self.device, &mut self.sockets);
        if let Some(failure) = self.device.failure() {
            return Err(failure);
        }
        match self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll() {
            None => Ok(None),
            Some(dhcpv4::Event::Deconfigured) => {
                // The socket also says this once on starting, when there was nothing to lose.
                if self.address.is_some() {
                    println!("DHCP: address lost");
                }
                self.address = None;
                self.interface.update_ip_addrs(|addresses| addresses.clear());
                self.interface.routes_mut().remove_default_ipv4_route();
                Ok(None)
            }
            Some(dhcpv4::Event::Configured(config)) => {
                println!("DHCP: address {}", config.address);
                self.interface.update_ip_addrs(|addresses| {
                    addresses.clear();
                    // There is one address, and room for more: this cannot fail.
                    let _ = addresses.push(IpCidr::Ipv4(config.address));
                });
                match config.router {
                    Some(router) => {
                        println!("DHCP: router {}", router);
                        // The table is emptied of the default route, so there is room in it.
                        let _ = self.interface.routes_mut().add_default_ipv4_route(router);
                    }
                    None => {
                        println!("DHCP: no router");
                        self.interface.routes_mut().remove_default_ipv4_route();
                    }
                }
                for server in &config.dns_servers {
                    println!("DHCP: name server {}", server);
                }
                self.gateway = config.router;
                self.address = Some(config.address.address());
                Ok(Some(config.address.address()))
            }
        }
    }

    /// Runs the stack until DHCP gives an address, or the time is up.
    fn wait_for_address(&mut self) -> Result<Option<Ipv4Address>, Error> {
        let begin = self.mct.count();
        while self.mct.count() - begin < DHCP_TIMEOUT_MS * COUNTS_PER_MS {
            if let Some(address) = self.poll()? {
                return Ok(Some(address));
            }
        }
        Ok(None)
    }

    /// Runs the stack for a while.
    fn run_for(&mut self, ms: u64) -> Result<(), Error> {
        let begin = self.mct.count();
        while self.mct.count() - begin < ms * COUNTS_PER_MS {
            self.poll()?;
        }
        Ok(())
    }

    /// Pings a host, and returns how many answered.
    fn ping(&mut self, host: Ipv4Address) -> Result<u16, Error> {
        let mut answers = 0;
        for sequence in 1..=PINGS {
            let begin = self.mct.count();
            let answered = self.send_ping(host, sequence) && self.wait_for_reply(host, sequence)?;
            if answered {
                println!("Reply {}: {} ms", sequence, (self.mct.count() - begin) / COUNTS_PER_MS);
                answers += 1;
            } else {
                println!("Reply {}: none", sequence);
            }
            if sequence != PINGS {
                self.run_for(PING_INTERVAL_MS)?;
            }
        }
        Ok(answers)
    }

    /// Queues a ping. Returns false if the socket has no room for it.
    fn send_ping(&mut self, host: Ipv4Address, sequence: u16) -> bool {
        let request = Icmpv4Repr::EchoRequest {
            ident: PING_ID,
            seq_no: sequence,
            data: &PING_DATA,
        };
        let socket = self.sockets.get_mut::<icmp::Socket>(self.ping);
        let Ok(buffer) = socket.send(request.buffer_len(), IpAddress::Ipv4(host)) else {
            return false;
        };
        request.emit(&mut Icmpv4Packet::new_unchecked(buffer), &self.capabilities.checksum);
        true
    }

    /// Runs the stack until the host answers this ping, or the time is up.
    fn wait_for_reply(&mut self, host: Ipv4Address, sequence: u16) -> Result<bool, Error> {
        let begin = self.mct.count();
        while self.mct.count() - begin < PING_TIMEOUT_MS * COUNTS_PER_MS {
            self.poll()?;
            let socket = self.sockets.get_mut::<icmp::Socket>(self.ping);
            while let Ok((payload, source)) = socket.recv() {
                let Ok(packet) = Icmpv4Packet::new_checked(payload) else {
                    continue;
                };
                let Ok(reply) = Icmpv4Repr::parse(&packet, &self.capabilities.checksum) else {
                    continue;
                };
                if source == IpAddress::Ipv4(host)
                    && matches!(reply, Icmpv4Repr::EchoReply { seq_no, .. } if seq_no == sequence)
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn ms(&self) -> u64 {
        self.mct.count() / COUNTS_PER_MS
    }

    /// Runs the stack until the client socket is in a state that `done` accepts, or the time is up.
    fn wait_for_client(&mut self, done: impl Fn(&tcp::Socket) -> bool) -> Result<bool, Error> {
        let begin = self.mct.count();
        while self.mct.count() - begin < TCP_TIMEOUT_MS * COUNTS_PER_MS {
            self.poll()?;
            if done(self.sockets.get::<tcp::Socket>(self.client)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Opens a TCP connection to a web server, asks for a page, and reports the first line of the
    /// answer and its size. Returns whether an answer came and the connection closed cleanly.
    fn fetch(&mut self, host: Ipv4Address, port: u16) -> Result<bool, Error> {
        let local_port = LOCAL_PORT_BASE | (self.mct.count() as u16 & 0x3FFF);
        let begin = self.mct.count();
        let socket = self.sockets.get_mut::<tcp::Socket>(self.client);
        if let Err(error) = socket.connect(self.interface.context(), (IpAddress::Ipv4(host), port), local_port) {
            println!("Connecting: {:?} (MISMATCH)", error);
            return Ok(false);
        }
        if !self.wait_for_client(|socket| socket.may_send() || !socket.is_open())?
            || !self.sockets.get::<tcp::Socket>(self.client).may_send()
        {
            println!("No connection (MISMATCH)");
            self.sockets.get_mut::<tcp::Socket>(self.client).abort();
            return Ok(false);
        }
        println!("Connected in {} ms, from port {}", (self.mct.count() - begin) / COUNTS_PER_MS, local_port);

        // A new connection's buffer has room for the whole request.
        let _ = self.sockets.get_mut::<tcp::Socket>(self.client).send_slice(PUBLIC_WEB_REQUEST);
        let asked = self.mct.count();
        let mut line = [0; STATUS_LINE_MAX];
        let mut line_length = 0;
        let mut line_done = false;
        let mut total = 0usize;
        let mut first_ms = None;
        let deadline = TCP_TIMEOUT_MS * COUNTS_PER_MS;
        while self.mct.count() - asked < deadline {
            self.poll()?;
            let socket = self.sockets.get_mut::<tcp::Socket>(self.client);
            let mut chunk = [0; 256];
            while let Ok(count) = socket.recv_slice(&mut chunk) {
                if count == 0 {
                    break;
                }
                if first_ms.is_none() {
                    first_ms = Some((self.mct.count() - asked) / COUNTS_PER_MS);
                }
                total += count;
                for &byte in &chunk[..count] {
                    if line_done || byte == b'\r' || byte == b'\n' {
                        line_done |= line_length != 0;
                    } else if line_length < STATUS_LINE_MAX {
                        line[line_length] = byte;
                        line_length += 1;
                    }
                }
            }
            // The server closes when it is done: nothing more will come.
            if !socket.may_recv() {
                break;
            }
        }
        let closed = !self.sockets.get::<tcp::Socket>(self.client).may_recv();
        match first_ms {
            Some(ms) => {
                print!("First bytes in {} ms: ", ms);
                print_text(&line[..line_length]);
            }
            None => println!("No answer (MISMATCH)"),
        }
        println!(
            "{} bytes in {} ms, server closed: {}",
            total,
            (self.mct.count() - asked) / COUNTS_PER_MS,
            if closed { "yes" } else { "no" },
        );

        let socket = self.sockets.get_mut::<tcp::Socket>(self.client);
        socket.close();
        let finished =
            self.wait_for_client(|socket| matches!(socket.state(), tcp::State::TimeWait | tcp::State::Closed))?;
        if !finished {
            self.sockets.get_mut::<tcp::Socket>(self.client).abort();
        }
        println!("Closed from this side: {}", verdict(finished));
        Ok(first_ms.is_some() && closed && finished)
    }

    /// Lets each HTTP server do what its connection needs.
    fn serve_http(&mut self) {
        let now_ms = self.ms();
        let status = http::Status {
            uptime_s: now_ms / 1000,
            address: self.address,
            router: self.gateway,
            mac: self.mac,
            pings: self.device.echo_requests(),
            requests: self.requests,
        };
        for (server, mark) in self.servers.iter_mut().zip(&mut self.marks) {
            let socket = self.sockets.get_mut::<tcp::Socket>(server.handle());
            match server.service(socket, now_ms, &status) {
                http::Event::Nothing => {}
                http::Event::Answered => {
                    self.requests += 1;
                    *mark = Mark {
                        stats: self.device.stats(),
                        count: self.mct.count(),
                    };
                }
                http::Event::DataAcknowledged => {
                    let now = Mark {
                        stats: self.device.stats(),
                        count: self.mct.count(),
                    };
                    report_transfer(mark, &now);
                    report_link(self.device.chip());
                }
            }
        }
    }

    /// Stays on the network, with the stack answering ARP and pings and the servers HTTP, until
    /// told to restart.
    fn serve(&mut self) -> Result<(), Error> {
        print!("\nStaying connected, answering pings");
        match self.address {
            Some(address) => println!(
                " and HTTP: http://{}/ and http://{}/data, and with {} KiB in flight http://{}:{}/data",
                address,
                address,
                LARGE_TX / 1024,
                address,
                LARGE_PORT,
            ),
            None => println!(" and HTTP on ports {} and {}", HTTP_PORT, LARGE_PORT),
        }
        println!("UART: r restart, f restart into fastboot");
        let uart = console::uart();
        let mut keys = PowerKeys::new();
        let mut reported = self.device.echo_requests();
        loop {
            self.poll()?;
            self.serve_http();
            keys.service(self.mct);
            if self.device.echo_requests() != reported {
                reported = self.device.echo_requests();
                println!("Pings received: {}", reported);
            }
            match uart.try_read_byte() {
                Some(b'r') => power::restart(BootMode::Normal),
                Some(b'f') => power::restart(BootMode::Fastboot),
                _ => {}
            }
        }
    }
}

/// A TCP socket on these buffers, which gives up on a peer that goes quiet.
fn tcp_socket<'a>(rx: &'a mut [u8], tx: &'a mut [u8]) -> tcp::Socket<'a> {
    let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx));
    socket.set_timeout(Some(TCP_IDLE_TIMEOUT));
    socket
}

fn join(chip: &mut Bcm4334, mct: Mct, own: [u8; 6], target: &Target, pool: &[u8; 20]) -> Result<(), Error> {
    let (Some(ssid), Some(password)) = (SSID, PASSWORD) else {
        return Ok(());
    };
    println!("Crypto self-test: {}", verdict(crypto::self_test()));

    let begin = mct.count();
    let key = crypto::wpa_key(password.as_bytes(), ssid.as_bytes());
    println!("Key from the passphrase in {} ms", (mct.count() - begin) / COUNTS_PER_MS);

    let mut nonce = [0; 32];
    crypto::prf(pool, b"Init Counter", &mct.count().to_le_bytes(), &mut nonce);
    let mut seed = [0; 8];
    crypto::prf(pool, b"Stack seed", &mct.count().to_le_bytes(), &mut seed);

    let rsn = &target.rsn[..target.rsn_length];
    let Some(mut supplicant) = Supplicant::new(key, own, target.bssid, rsn, nonce) else {
        println!("The access point asks for what this cannot do (MISMATCH)");
        return Ok(());
    };

    println!(
        "Joining {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, channel {}, {} dBm",
        target.bssid[0],
        target.bssid[1],
        target.bssid[2],
        target.bssid[3],
        target.bssid[4],
        target.bssid[5],
        target.channel,
        target.rssi,
    );
    let begin = mct.count();
    chip.connect(ssid.as_bytes(), &mut supplicant, JOIN_TIMEOUT_MS, report)?;
    println!("Connected in {} ms: keys installed", (mct.count() - begin) / COUNTS_PER_MS);
    report_link(chip);

    let mut device = WifiDevice::new(chip, &mut supplicant, report, mct);
    let capabilities = device.capabilities();
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(own)));
    config.random_seed = u64::from_le_bytes(seed);
    let start = Instant::from_micros((mct.count() / (COUNTS_PER_MS / 1000)) as i64);
    let interface = Interface::new(config, &mut device, start);

    let mut storage = [SocketStorage::EMPTY; SOCKETS];
    let mut sockets = SocketSet::new(&mut storage[..]);
    let dhcp = sockets.add(dhcpv4::Socket::new());

    let mut received_meta = [icmp::PacketMetadata::EMPTY; PING_QUEUE];
    let mut received = [0; PING_BUFFER];
    let mut sent_meta = [icmp::PacketMetadata::EMPTY; PING_QUEUE];
    let mut sent = [0; PING_BUFFER];
    let mut ping_socket = icmp::Socket::new(
        icmp::PacketBuffer::new(&mut received_meta[..], &mut received[..]),
        icmp::PacketBuffer::new(&mut sent_meta[..], &mut sent[..]),
    );
    // A new socket with a nonzero identifier cannot refuse this.
    let _ = ping_socket.bind(icmp::Endpoint::Ident(PING_ID));
    let ping = sockets.add(ping_socket);

    let mut client_rx = [0; CLIENT_RX];
    let mut client_tx = [0; CLIENT_TX];
    let client = sockets.add(tcp_socket(&mut client_rx[..], &mut client_tx[..]));

    let mut server_rx = [[0; SERVER_RX]; SERVERS];
    let mut server_tx = [[0; SERVER_TX]; SERVERS];
    let mut large_rx = [0; SERVER_RX];
    let mut large_tx = [0; LARGE_TX];
    let mut handles = [SocketHandle::default(); ALL_SERVERS];
    for ((handle, rx), tx) in handles.iter_mut().zip(&mut server_rx).zip(&mut server_tx) {
        *handle = sockets.add(tcp_socket(&mut rx[..], &mut tx[..]));
    }
    handles[SERVERS] = sockets.add(tcp_socket(&mut large_rx[..], &mut large_tx[..]));
    let mut ports = [HTTP_PORT; ALL_SERVERS];
    ports[SERVERS] = LARGE_PORT;
    let mut servers_at = handles.iter().zip(ports);
    let servers = core::array::from_fn(|_| match servers_at.next() {
        Some((&handle, port)) => http::Server::new(handle, port),
        // Both arrays have ALL_SERVERS entries.
        None => unreachable!(),
    });

    let mut station = Station {
        device,
        interface,
        sockets,
        dhcp,
        ping,
        client,
        servers,
        marks: [Mark::default(); ALL_SERVERS],
        capabilities,
        mct,
        mac: own,
        gateway: None,
        address: None,
        requests: 0,
    };

    println!("\nGetting an address");
    let begin = mct.count();
    if station.wait_for_address()?.is_none() {
        println!("DHCP: no address (MISMATCH)");
        return station.serve();
    }
    println!("Address in {} ms", (mct.count() - begin) / COUNTS_PER_MS);

    let Some(router) = station.gateway else {
        println!("No router to ping (MISMATCH)");
        return station.serve();
    };
    println!("\nPinging the router, {}", router);
    let answers = station.ping(router)?;
    println!("{} of {} answered: {}", answers, PINGS, verdict(answers != 0));

    println!("\nPinging past the router, {}", PUBLIC_HOST);
    let answers = station.ping(PUBLIC_HOST)?;
    println!("{} of {} answered: {}", answers, PINGS, verdict(answers != 0));

    println!("\nTCP to {}:{}", PUBLIC_WEB, PUBLIC_WEB_PORT);
    let fetched = station.fetch(PUBLIC_WEB, PUBLIC_WEB_PORT)?;
    println!("TCP: {}", verdict(fetched));

    station.serve()
}

fn run(pmic: Max77686, mct: Mct) -> Result<(), Error> {
    println!("\nPowering the chip up");
    bcm4334::power_on(pmic, mct)?;

    let host = Sdhci::new()?;
    if SDIO_HZ == 0 {
        println!("WIFI_SDIO_MHZ is a whole number of MHz (MISMATCH)");
        return Ok(());
    }
    println!("Asking for {} Hz", SDIO_HZ);
    let (mut chip, rate) = Bcm4334::start(host, mct, SDIO_HZ)?;
    let card = chip.card();
    let info = card.info();
    println!("Card: OCR {:#08x}, {} functions, address {:#x}", info.ocr, info.functions, info.rca);
    match card.manufacturer().map_err(Error::Sdio)? {
        Some((vendor, device)) => println!(
            "Vendor {:#06x}, device {:#06x}: {}",
            vendor,
            device,
            verdict(vendor == VENDOR_BROADCOM && device == DEVICE_4334),
        ),
        None => println!("No manufacturer tuple in the CIS (MISMATCH)"),
    }
    println!(
        "4-bit bus at {} Hz, {} speed",
        rate,
        if SDIO_HZ > bcm4334::DEFAULT_SPEED_MAX_HZ { "high" } else { "default" },
    );

    println!("\nFinding the chip's contents");
    let id = chip.attach()?;
    println!(
        "Chip {:#06x}, revision {}: {}",
        id.chip(),
        id.revision(),
        verdict(id.chip() == CHIP_ID_4334),
    );
    println!("PMU revision {}, {} KiB of RAM", chip.pmu_revision(), chip.ram_size() / 1024);
    for core in chip.cores() {
        println!(
            "Core {:#05x} revision {}: registers {:#010x}, wrapper {:#010x}",
            core.id, core.revision, core.base, core.wrapper,
        );
    }

    let mut errors = 0;
    let mut different = 0;
    for _ in 0..ID_READS {
        match chip.chip_id() {
            Ok(again) if again == id => {}
            Ok(_) => different += 1,
            Err(_) => errors += 1,
        }
    }
    println!(
        "{} reads of the ID: {} errors, {} different values: {}",
        ID_READS,
        errors,
        different,
        verdict(errors == 0 && different == 0),
    );

    if FIRMWARE.is_empty() {
        println!("\nBuilt without WIFI_FIRMWARE_DIR: stopping before the firmware");
        return Ok(());
    }

    println!("\nLoading {} bytes of firmware", FIRMWARE.len());
    let begin = mct.count();
    let load = chip.load_firmware(FIRMWARE, NVRAM)?;
    println!(
        "Written and checked in {} ms. NVRAM: {} bytes at {:#x}",
        (mct.count() - begin) / COUNTS_PER_MS,
        load.nvram_bytes,
        load.nvram_address,
    );

    println!("\nRunning the firmware");
    if let Err(error) = chip.start_firmware() {
        report_diagnostics(&mut chip);
        return Err(error);
    }
    report_diagnostics(&mut chip);

    println!("\nTalking to the firmware");
    let own = match talk(&mut chip) {
        Ok(own) => own,
        Err(error) => {
            report_diagnostics(&mut chip);
            return Err(error);
        }
    };
    report_diagnostics(&mut chip);

    println!("\nScanning");
    let (target, pool) = match scan(&mut chip, mct, own) {
        Ok(found) => found,
        Err(error) => {
            report_diagnostics(&mut chip);
            return Err(error);
        }
    };

    println!("\nJoining");
    if SSID.is_none() || PASSWORD.is_none() {
        println!("Built without WIFI_SSID and WIFI_PASSWORD: stopping here");
        return Ok(());
    }
    if BAND.is_some_and(|band| band != "2.4" && band != "5") {
        println!("WIFI_BAND is 2.4 or 5 (MISMATCH)");
        return Ok(());
    }
    match BAND {
        Some(band) => println!("Joining in the {} GHz band only", band),
        None => println!("Joining in either band"),
    }
    if !PASSWORD.is_some_and(|password| (8..=63).contains(&password.len())) {
        println!("A WPA passphrase has 8 to 63 characters (MISMATCH)");
        return Ok(());
    }
    let Some(target) = target else {
        println!("The named network was not found, not in WIFI_BAND, or asks for what this cannot do (MISMATCH)");
        return Ok(());
    };
    if let Err(error) = join(&mut chip, mct, own, &target, &pool) {
        report_diagnostics(&mut chip);
        return Err(error);
    }
    Ok(())
}

fn main(_args: BootArgs) -> ! {
    println!("\nBCM4334 WiFi chip test, sixth stage");

    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("I2C ERROR {:?}", error);
        idle(mct);
    }
    let pmic = Max77686::new(i2c);

    match bcm4334::power_state(pmic) {
        Ok(before) => println!(
            "Found: LDO22 {:#04x}, 32 kHz {:#04x}, LDO enable {}, chip enable {}",
            before.ldo, before.clocks_32k, before.ldo_enable, before.wlan_enable,
        ),
        Err(error) => println!("Reading the PMIC: ERROR {:?}", error),
    }

    match run(pmic, mct) {
        Ok(()) => println!("\nAll steps ran"),
        Err(error) => println!("\nStopped: ERROR {:?}", error),
    }
    idle(mct);
}
