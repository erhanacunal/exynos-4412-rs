//! BCM4334 WiFi chip test, fifth stage: loads the firmware, scans, joins a WPA2-Personal network, gets
//! an address by DHCP, and pings.
//!
//! Reports what U-Boot left, powers the chip up from reset, starts its SDIO bus and reads what the
//! chip says about itself. It lists the chip's cores, loads the firmware and NVRAM and checks them
//! by reading them back, runs the firmware, asks it for its version and its MAC address, and lists
//! the networks it can hear. Then it joins the one named at build time, gets an address, pings the
//! router and a public address, and stays on the network answering ARP and pings until told to stop.
//! It does not renew its address, so that lasts as long as the lease.
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
use exynos_4412_rs::net::{self, Addresses, DhcpReply, Ip, Mac, Packet};
use exynos_4412_rs::wpa::{self, Supplicant};
use exynos_4412_rs::{BootArgs, console, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/firmware.bin"));
static NVRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nvram.txt"));

/// `include/linux/mmc/sdio_ids.h:56`.
const VENDOR_BROADCOM: u16 = 0x02D0;
/// `include/linux/mmc/sdio_ids.h:61`.
const DEVICE_4334: u16 = 0x4334;
/// Reads of the ID register in the reliability check.
const ID_READS: u32 = 1000;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;
/// Networks kept from a scan.
const MAX_NETWORKS: usize = 48;
/// A full scan of both bands takes seconds. This is well over that, and not measured.
const SCAN_TIMEOUT_MS: u64 = 20_000;
/// The network to join.
const SSID: Option<&str> = option_env!("WIFI_SSID");
const PASSWORD: Option<&str> = option_env!("WIFI_PASSWORD");
/// Joining scans first, then authenticates and shakes hands. Not measured.
const JOIN_TIMEOUT_MS: u64 = 30_000;
/// How long a server or a host gets to answer, each time.
const ANSWER_TIMEOUT_MS: u64 = 3_000;
/// Times to ask before giving up.
const DHCP_ATTEMPTS: u32 = 4;
const ARP_ATTEMPTS: u32 = 4;
/// Pings to each host, and the time between them.
const PINGS: u16 = 4;
const PING_INTERVAL_MS: u64 = 1_000;
/// Identifies this program's pings among the replies.
const PING_ID: u16 = 0x4534;
/// A public address, to show that traffic gets past the router: a well known name server of Google.
const PUBLIC_HOST: Ip = [8, 8, 8, 8];

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
        if SSID.is_some_and(|ssid| ssid.as_bytes() == &bss.ssid[..usize::from(bss.ssid_length)]) {
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

fn print_ip(label: &str, ip: Ip) {
    println!("{}: {}.{}.{}.{}", label, ip[0], ip[1], ip[2], ip[3]);
}

/// A connected station, and its buffers.
struct Station<'a> {
    chip: &'a mut Bcm4334,
    supplicant: Supplicant,
    mct: Mct,
    mac: Mac,
    /// This station's address, once it has one.
    ip: Option<Ip>,
    received: [u8; net::FRAME_MAX],
    sent: [u8; net::FRAME_MAX],
    answered_arp: u32,
    answered_pings: u32,
}

impl Station<'_> {
    /// Reads what is waiting, and answers what is for this station. Returns the length of a frame
    /// now in `received`, if any arrived.
    fn poll(&mut self) -> Result<Option<usize>, Error> {
        let length = self.chip.poll(&mut self.supplicant, &mut self.received, report)?;
        if let Some(length) = length
            && let Some(own_ip) = self.ip
        {
            match net::parse(&self.received[..length]) {
                Packet::Arp {
                    request: true,
                    sender_mac,
                    sender_ip,
                    target_ip,
                } if target_ip == own_ip => {
                    let reply = net::arp_reply(&mut self.sent, self.mac, own_ip, sender_mac, sender_ip);
                    self.chip.send_ethernet(&self.sent[..reply])?;
                    self.answered_arp += 1;
                }
                Packet::EchoRequest {
                    source_mac,
                    source_ip,
                    destination_ip,
                    rest,
                } if destination_ip == own_ip => {
                    let addresses = Addresses {
                        destination_mac: source_mac,
                        source_mac: self.mac,
                        destination_ip: source_ip,
                        source_ip: own_ip,
                    };
                    if let Some(reply) = net::echo_reply(&mut self.sent, addresses, rest) {
                        self.chip.send_ethernet(&self.sent[..reply])?;
                        self.answered_pings += 1;
                    }
                }
                _ => {}
            }
        }
        Ok(length)
    }

    /// Serves the connection until `found` likes a frame or the time is up.
    fn wait_for<T>(&mut self, ms: u64, mut found: impl FnMut(Packet<'_>) -> Option<T>) -> Result<Option<T>, Error> {
        let begin = self.mct.count();
        while self.mct.count() - begin < ms * COUNTS_PER_MS {
            if let Some(length) = self.poll()?
                && let Some(value) = found(net::parse(&self.received[..length]))
            {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn wait_for_dhcp(
        &mut self,
        id: u32,
        wanted: impl Fn(&DhcpReply) -> bool,
    ) -> Result<Option<DhcpReply>, Error> {
        let mac = self.mac;
        self.wait_for(ANSWER_TIMEOUT_MS, |packet| match packet {
            Packet::Udp {
                destination_port,
                data,
            } => net::dhcp_reply(destination_port, data, mac, id).filter(&wanted),
            _ => None,
        })
    }

    /// Runs DHCP's two exchanges, and returns what the server acknowledged.
    fn get_address(&mut self, id: u32) -> Result<Option<DhcpReply>, Error> {
        for attempt in 1..=DHCP_ATTEMPTS {
            println!("DHCP: asking, attempt {}", attempt);
            let length = net::dhcp_discover(&mut self.sent, self.mac, id);
            self.chip.send_ethernet(&self.sent[..length])?;
            let Some(offer) = self.wait_for_dhcp(id, |reply| reply.offer)? else {
                continue;
            };
            print_ip("DHCP: offered", offer.address);
            let length = net::dhcp_request(&mut self.sent, self.mac, id, offer.address, offer.server);
            self.chip.send_ethernet(&self.sent[..length])?;
            let Some(reply) = self.wait_for_dhcp(id, |reply| reply.ack || reply.nak)? else {
                continue;
            };
            if reply.ack && reply.address == offer.address {
                return Ok(Some(reply));
            }
            println!("DHCP: refused");
        }
        Ok(None)
    }

    /// Finds the Ethernet address of a host on this network.
    fn find_mac(&mut self, own_ip: Ip, host: Ip) -> Result<Option<Mac>, Error> {
        for _ in 0..ARP_ATTEMPTS {
            let length = net::arp_request(&mut self.sent, self.mac, own_ip, host);
            self.chip.send_ethernet(&self.sent[..length])?;
            let found = self.wait_for(ANSWER_TIMEOUT_MS, |packet| match packet {
                Packet::Arp {
                    request: false,
                    sender_mac,
                    sender_ip,
                    ..
                } if sender_ip == host => Some(sender_mac),
                _ => None,
            })?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// Pings a host through `next_hop`, and returns how many answered.
    fn ping(&mut self, own_ip: Ip, next_hop: Mac, host: Ip) -> Result<u16, Error> {
        let addresses = Addresses {
            destination_mac: next_hop,
            source_mac: self.mac,
            destination_ip: host,
            source_ip: own_ip,
        };
        let mut answers = 0;
        for sequence in 1..=PINGS {
            let begin = self.mct.count();
            let length = net::echo_request(&mut self.sent, addresses, PING_ID, sequence);
            self.chip.send_ethernet(&self.sent[..length])?;
            let found = self.wait_for(ANSWER_TIMEOUT_MS, |packet| {
                (packet
                    == Packet::EchoReply {
                        source_ip: host,
                        id: PING_ID,
                        sequence,
                    })
                .then_some(())
            })?;
            let elapsed = (self.mct.count() - begin) / COUNTS_PER_MS;
            if found.is_some() {
                println!("Reply {}: {} ms", sequence, elapsed);
                answers += 1;
            } else {
                println!("Reply {}: none", sequence);
            }
            if sequence != PINGS {
                self.wait_for(PING_INTERVAL_MS, |_| None::<()>)?;
            }
        }
        Ok(answers)
    }
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
    let mut id = [0; 4];
    crypto::prf(pool, b"DHCP id", &mct.count().to_le_bytes(), &mut id);

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

    let mut station = Station {
        chip,
        supplicant,
        mct,
        mac: own,
        ip: None,
        received: [0; net::FRAME_MAX],
        sent: [0; net::FRAME_MAX],
        answered_arp: 0,
        answered_pings: 0,
    };

    println!("\nGetting an address");
    let begin = mct.count();
    let Some(lease) = station.get_address(u32::from_le_bytes(id))? else {
        println!("DHCP: no address (MISMATCH)");
        return Ok(());
    };
    println!("Address in {} ms", (mct.count() - begin) / COUNTS_PER_MS);
    print_ip("Address", lease.address);
    print_ip("Mask", lease.mask);
    print_ip("Router", lease.router);
    print_ip("Name server", lease.dns);
    println!("Lease: {} s", lease.lease_seconds);
    station.ip = Some(lease.address);

    println!("\nPinging the router");
    let Some(router) = station.find_mac(lease.address, lease.router)? else {
        println!("The router does not answer ARP (MISMATCH)");
        return serve(&mut station);
    };
    println!(
        "Router at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        router[0], router[1], router[2], router[3], router[4], router[5],
    );
    let answers = station.ping(lease.address, router, lease.router)?;
    println!("{} of {} answered: {}", answers, PINGS, verdict(answers != 0));

    print_ip("\nPinging, past the router", PUBLIC_HOST);
    let answers = station.ping(lease.address, router, PUBLIC_HOST)?;
    println!("{} of {} answered: {}", answers, PINGS, verdict(answers != 0));

    serve(&mut station)
}

/// Stays on the network, answering ARP and pings, until told to restart.
fn serve(station: &mut Station<'_>) -> Result<(), Error> {
    println!("\nStaying connected, answering pings. UART: r restart, f restart into fastboot");
    let uart = console::uart();
    let mut keys = PowerKeys::new();
    let mut reported = 0;
    loop {
        station.poll()?;
        keys.service(station.mct);
        if station.answered_pings != reported {
            reported = station.answered_pings;
            println!("Pings answered: {}, ARP answered: {}", reported, station.answered_arp);
        }
        match uart.try_read_byte() {
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
    }
}

fn run(pmic: Max77686, mct: Mct) -> Result<(), Error> {
    println!("\nPowering the chip up");
    bcm4334::power_on(pmic, mct)?;

    let host = Sdhci::new()?;
    let (mut chip, rate) = Bcm4334::start(host, mct)?;
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
    println!("4-bit bus at {} Hz", rate);

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
    if !PASSWORD.is_some_and(|password| (8..=63).contains(&password.len())) {
        println!("A WPA passphrase has 8 to 63 characters (MISMATCH)");
        return Ok(());
    }
    let Some(target) = target else {
        println!("The named network was not found, or asks for what this cannot do (MISMATCH)");
        return Ok(());
    };
    if let Err(error) = join(&mut chip, mct, own, &target, &pool) {
        report_diagnostics(&mut chip);
        return Err(error);
    }
    Ok(())
}

fn main(_args: BootArgs) -> ! {
    println!("\nBCM4334 WiFi chip test, fifth stage");

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
