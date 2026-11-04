//! BCM4334 WiFi under Embassy: the chip joined as in `wifi_test`, then handed to `embassy-net`
//! through `exynos_4412_rs::embassy::wifi`, whose runner task reads the chip only after its host
//! wake line pulses, and otherwise sleeps.
//!
//! 1. Before the executor, polled as in `wifi_test`: the chip powered and started on SDIO, its
//!    firmware loaded, the host wake line on, a scan for the network named at build time, the
//!    WPA2 handshake.
//! 2. On CPU0: the WiFi runner, `embassy-net`'s runner with DHCP, a task ticking every 1 ms, the
//!    keys, a report every 10 s, and three HTTP servers on port 80.
//! 3. Once there is an address: a TCP connection to a public web server.
//!
//! HTTP, from another computer: `/` a status page, `/data` 1 MiB, `POST /upload` any length,
//! each timed. Pings are answered. Keys: `s` the report now, `r` restart, `f` restart into
//! fastboot.
//!
//! The firmware and NVRAM come from `WIFI_FIRMWARE_DIR` at build time, and the network from
//! `WIFI_SSID` and `WIFI_PASSWORD`, as for `wifi_test`: see `build.rs`. The passphrase is never
//! printed. The handshake nonce comes from timing the scan, as in `wifi_test`: fine to test with,
//! unproven beyond that.

#![no_std]
#![no_main]

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Ipv4Address, Stack, StackResources};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use exynos_4412_rs::crypto::{self, Hmac};
use exynos_4412_rs::drivers::bcm4334::{self, Activity, Bcm4334, Error, Event};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::sdhci::Sdhci;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::uart::{UartRx, buffer_console};
use exynos_4412_rs::embassy::wifi;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::wpa::{self, Supplicant};
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println, soc};

exynos_4412_rs::entry!(main);

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/firmware.bin"));
static NVRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nvram.txt"));

const SSID: Option<&str> = option_env!("WIFI_SSID");
const PASSWORD: Option<&str> = option_env!("WIFI_PASSWORD");
/// `2.4` or `5`: join only an access point of that band.
const BAND: Option<&str> = option_env!("WIFI_BAND");
const LAST_2G_CHANNEL: u8 = 14;
const SCAN_TIMEOUT_MS: u64 = 20_000;
const JOIN_TIMEOUT_MS: u64 = 30_000;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// Frames queued each way between the WiFi runner and the stack.
const RX_FRAMES: usize = 16;
const TX_FRAMES: usize = 16;
/// DHCP's socket and the servers' and the client's, with room to spare.
const SOCKETS: usize = 8;
const HTTP_PORT: u16 = 80;
const HTTP_SERVERS: usize = 3;
const SERVER_RX: usize = 32 * 1024;
const SERVER_TX: usize = 64 * 1024;
const REQUEST_MAX: usize = 1024;
const DATA_BYTES: usize = 1024 * 1024;
const TCP_TIMEOUT: Duration = Duration::from_secs(10);
const REPORT_PERIOD: Duration = Duration::from_secs(10);

const PUBLIC_WEB: Ipv4Address = Ipv4Address::new(1, 1, 1, 1);
const PUBLIC_WEB_REQUEST: &[u8] = b"GET / HTTP/1.0\r\nHost: 1.1.1.1\r\nConnection: close\r\n\r\n";

static mut WIFI_STATE: wifi::State<RX_FRAMES, TX_FRAMES> = wifi::State::new();
static mut RESOURCES: StackResources<SOCKETS> = StackResources::new();

static TICKS: AtomicU32 = AtomicU32::new(0);
static MAX_TICK_LATE_US: AtomicU32 = AtomicU32::new(0);
static REQUESTS: AtomicU32 = AtomicU32::new(0);

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

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

fn halt() -> ! {
    console::flush();
    cpu::halt();
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
    println!("Event {} ({}): status {}, reason {}, flags {:#x}", event.code, name, event.status, event.reason, event.flags);
}

fn report(activity: Activity) {
    match activity {
        Activity::Event(event) => print_event(event),
        Activity::Handshake(message) => println!("Handshake: {:?}, answered", message),
        Activity::Ignored(error) => println!("Handshake: frame ignored, {:?}", error),
    }
}

fn mac_text(mac: [u8; 6]) -> [u8; 17] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = [b':'; 17];
    for (i, byte) in mac.iter().enumerate() {
        text[3 * i] = HEX[usize::from(byte >> 4)];
        text[3 * i + 1] = HEX[usize::from(byte & 0xF)];
    }
    text
}

/// The access point to join, the strongest of the named network's.
struct Target {
    bssid: [u8; 6],
    rssi: i16,
    channel: u8,
    rsn: [u8; wpa::RSN_MAX],
    rsn_length: usize,
}

fn band_allows(channel: u8) -> bool {
    match BAND {
        Some("2.4") => channel <= LAST_2G_CHANNEL,
        Some("5") => channel > LAST_2G_CHANNEL,
        _ => true,
    }
}

/// What the polled bring-up hands on.
struct Joined {
    chip: Bcm4334,
    supplicant: Supplicant,
    seed: u64,
}

/// Powers the chip up, starts its firmware and its host wake line, and joins the named network:
/// `wifi_test`'s steps, with less said. `None` if the build or the air lacks what it needs.
fn join(mct: Mct) -> Result<Option<Joined>, Error> {
    let (Some(ssid), Some(password)) = (SSID, PASSWORD) else {
        println!("Built without WIFI_SSID and WIFI_PASSWORD");
        return Ok(None);
    };
    if FIRMWARE.is_empty() {
        println!("Built without WIFI_FIRMWARE_DIR");
        return Ok(None);
    }
    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("I2C ERROR {:?}", error);
        return Ok(None);
    }
    bcm4334::power_on(Max77686::new(i2c), mct)?;
    let (mut chip, rate) = Bcm4334::start(Sdhci::new()?, mct, bcm4334::HIGH_SPEED_MAX_HZ)?;
    let id = chip.attach()?;
    println!("Chip {:#06x} revision {}, SDIO at {} Hz", id.chip(), id.revision(), rate);
    let begin = mct.count();
    chip.load_firmware(FIRMWARE, NVRAM)?;
    chip.start_firmware()?;
    chip.enable_host_wake()?;
    chip.disable_glomming()?;
    let mut address = [0; 32];
    chip.get_variable("cur_etheraddr", &mut address)?;
    let mut own = [0; 6];
    own.copy_from_slice(&address[..6]);
    let mac = mac_text(own);
    println!(
        "Firmware running in {} ms, host wake line on; MAC {}",
        (mct.count() - begin) / COUNTS_PER_MS,
        core::str::from_utf8(&mac).unwrap_or("?")
    );

    chip.prepare_radio()?;
    let mut target: Option<Target> = None;
    let mut pool = Hmac::new(&own).sign(&[&mct.count().to_le_bytes()[..]]);
    let mut results = 0u32;
    let begin = mct.count();
    let status = chip.scan(SCAN_TIMEOUT_MS, |bss, rsn| {
        results += 1;
        pool = Hmac::new(&pool).sign(&[&mct.count().to_le_bytes()[..], &bss.bssid, &bss.rssi.to_le_bytes()]);
        let named = ssid.as_bytes() == &bss.ssid[..usize::from(bss.ssid_length)];
        if named
            && band_allows(bss.channel)
            && wpa::rsn_supported(rsn)
            && rsn.len() <= wpa::RSN_MAX
            && target.as_ref().is_none_or(|known| bss.rssi > known.rssi)
        {
            let mut copy = [0; wpa::RSN_MAX];
            copy[..rsn.len()].copy_from_slice(rsn);
            target = Some(Target { bssid: bss.bssid, rssi: bss.rssi, channel: bss.channel, rsn: copy, rsn_length: rsn.len() });
        }
    })?;
    println!("Scan: {} results in {} ms, status {}", results, (mct.count() - begin) / COUNTS_PER_MS, status);
    let Some(target) = target else {
        println!("The named network was not found, not in WIFI_BAND, or asks for what this cannot do (MISMATCH)");
        return Ok(None);
    };

    let key = crypto::wpa_key(password.as_bytes(), ssid.as_bytes());
    let mut nonce = [0; 32];
    crypto::prf(&pool, b"Init Counter", &mct.count().to_le_bytes(), &mut nonce);
    let mut seed = [0; 8];
    crypto::prf(&pool, b"Stack seed", &mct.count().to_le_bytes(), &mut seed);
    let Some(mut supplicant) = Supplicant::new(key, own, target.bssid, &target.rsn[..target.rsn_length], nonce) else {
        println!("The access point asks for what this cannot do (MISMATCH)");
        return Ok(None);
    };
    let bssid = mac_text(target.bssid);
    println!(
        "Joining {}, channel {}, {} dBm",
        core::str::from_utf8(&bssid).unwrap_or("?"),
        target.channel,
        target.rssi
    );
    let begin = mct.count();
    chip.connect(ssid.as_bytes(), &mut supplicant, JOIN_TIMEOUT_MS, report)?;
    println!("Connected in {} ms: keys installed", (mct.count() - begin) / COUNTS_PER_MS);
    Ok(Some(Joined { chip, supplicant, seed: u64::from_le_bytes(seed) }))
}

#[embassy_executor::task]
async fn wifi_task(runner: wifi::Runner<'static>) {
    let error = runner.run().await;
    println!("WiFi stopped: {:?}", error);
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, wifi::Device<'static>>) {
    runner.run().await
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

/// What the WiFi runner did since `before`, in two lines.
fn print_stats(seconds: u64, now: &wifi::Stats, before: &wifi::Stats) {
    println!(
        "{} s: frames in {}, out {}, chip {}; empty reads {}; edges {}, backstops {}, missed {}; room polls {}, rx full {}",
        seconds,
        now.frames_in - before.frames_in,
        now.frames_out - before.frames_out,
        now.chip_frames - before.chip_frames,
        now.empty_reads - before.empty_reads,
        now.edges - before.edges,
        now.backstops - before.backstops,
        now.missed - before.missed,
        now.room_polls - before.room_polls,
        now.rx_full - before.rx_full,
    );
    println!(
        "  chip reads {} ms, writes {} ms; ticker {} ticks, {} us late at most",
        (now.read_us - before.read_us) / 1000,
        (now.write_us - before.write_us) / 1000,
        TICKS.swap(0, Ordering::Relaxed),
        MAX_TICK_LATE_US.swap(0, Ordering::Relaxed),
    );
}

#[embassy_executor::task]
async fn report_task() {
    let mut before = wifi::stats();
    let mut at = Instant::now();
    loop {
        Timer::after(REPORT_PERIOD).await;
        let now = wifi::stats();
        print_stats(at.elapsed().as_secs(), &now, &before);
        before = now;
        at = Instant::now();
    }
}

#[embassy_executor::task]
async fn keys_task() {
    let rx = UartRx::new(console::uart().id()).expect("the console UART has an interrupt");
    let mut before = wifi::stats();
    let mut at = Instant::now();
    loop {
        match rx.read_byte().await {
            b's' => {
                let now = wifi::stats();
                print_stats(at.elapsed().as_secs(), &now, &before);
                before = now;
                at = Instant::now();
            }
            b'r' => power::restart(BootMode::Normal),
            b'f' => power::restart(BootMode::Fastboot),
            _ => {}
        }
    }
}

/// Writes all of `data`, or fails.
async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        data = &data[written..];
    }
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The value of `Content-Length` in a request head, 0 without one.
fn content_length(head: &[u8]) -> usize {
    head.split(|&byte| byte == b'\n')
        .find_map(|line| {
            let colon = line.iter().position(|&byte| byte == b':')?;
            if !line[..colon].eq_ignore_ascii_case(b"content-length") {
                return None;
            }
            let digits = line[colon + 1..].iter().filter(|byte| byte.is_ascii_digit());
            Some(digits.fold(0usize, |value, digit| value.saturating_mul(10).saturating_add(usize::from(digit - b'0'))))
        })
        .unwrap_or(0)
}

/// KiB/s for `bytes` in `us`.
fn kib_per_s(bytes: usize, us: u64) -> u64 {
    bytes as u64 * 1_000_000 / 1024 / us.max(1)
}

/// Answers one request on an accepted connection.
async fn serve_request(socket: &mut TcpSocket<'_>, port: u16, stack: Stack<'_>) -> Result<(), embassy_net::tcp::Error> {
    let mut request = [0; REQUEST_MAX];
    let mut filled = 0;
    let head_end = loop {
        if filled == request.len() {
            return Ok(());
        }
        let read = socket.read(&mut request[filled..]).await?;
        if read == 0 {
            return Ok(());
        }
        filled += read;
        if let Some(at) = find(&request[..filled], b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = &request[..head_end];
    let line_end = find(head, b"\r\n").unwrap_or(head.len());
    let mut words = head[..line_end].split(|&byte| byte == b' ');
    let method = words.next().unwrap_or(b"");
    let path = words.next().unwrap_or(b"");
    let peer = socket.remote_endpoint();
    REQUESTS.fetch_add(1, Ordering::Relaxed);

    match (method, path) {
        (b"GET", b"/data") => {
            write_all(socket, b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n").await?;
            let mut chunk = [0u8; 4096];
            for (i, byte) in chunk.iter_mut().enumerate() {
                *byte = b'a' + (i % 26) as u8;
            }
            let begin = Instant::now();
            for _ in 0..DATA_BYTES / chunk.len() {
                write_all(socket, &chunk).await?;
            }
            socket.flush().await?;
            let us = begin.elapsed().as_micros();
            println!(
                "HTTP {}: {} bytes to {:?} acknowledged in {} ms, {} KiB/s",
                port,
                DATA_BYTES,
                peer,
                us / 1000,
                kib_per_s(DATA_BYTES, us)
            );
        }
        (b"POST", b"/upload") => {
            let length = content_length(head);
            let mut received = filled - head_end;
            let begin = Instant::now();
            while received < length {
                let read = socket.read(&mut request).await?;
                if read == 0 {
                    break;
                }
                received += read;
            }
            let us = begin.elapsed().as_micros();
            println!("HTTP {}: {} of {} bytes received in {} ms, {} KiB/s", port, received, length, us / 1000, kib_per_s(received, us));
            write_all(socket, b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nOK\n").await?;
        }
        (b"GET", b"/") => {
            let stats = wifi::stats();
            let mut page = Page::new();
            let address = stack.config_v4().map(|config| config.address);
            let _ = core::fmt::write(
                &mut page,
                format_args!(
                    "Exynos 4412, embassy-net over the BCM4334\nAddress {:?}\nUp {} s, {} requests\nFrames in {}, out {}; edges {}, backstops {}, missed {}\n",
                    address,
                    Instant::now().as_secs(),
                    REQUESTS.load(Ordering::Relaxed),
                    stats.frames_in,
                    stats.frames_out,
                    stats.edges,
                    stats.backstops,
                    stats.missed
                ),
            );
            let mut header = Page::new();
            let _ = core::fmt::write(
                &mut header,
                format_args!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", page.len),
            );
            write_all(socket, header.bytes()).await?;
            write_all(socket, page.bytes()).await?;
            println!("HTTP {}: / to {:?}", port, peer);
        }
        _ => {
            write_all(socket, b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        }
    }
    socket.flush().await
}

/// Text built with `write!`, up to 512 bytes; more is dropped.
struct Page {
    bytes: [u8; 512],
    len: usize,
}

impl Page {
    const fn new() -> Self {
        Self { bytes: [0; 512], len: 0 }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl core::fmt::Write for Page {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let room = self.bytes.len() - self.len;
        let take = text.len().min(room);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

#[embassy_executor::task(pool_size = HTTP_SERVERS)]
async fn http_task(stack: Stack<'static>, port: u16) {
    let mut rx = [0; SERVER_RX];
    let mut tx = [0; SERVER_TX];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(TCP_TIMEOUT));
        if socket.accept(port).await.is_err() {
            continue;
        }
        if let Err(error) = serve_request(&mut socket, port, stack).await {
            println!("HTTP {}: {:?}", port, error);
        }
        socket.close();
        let _ = with_timeout(TCP_TIMEOUT, socket.flush()).await;
        socket.abort();
    }
}

/// Connects to a public web server and reads its answer.
async fn fetch(stack: Stack<'static>) -> bool {
    let mut rx = [0; 4096];
    let mut tx = [0; 512];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(TCP_TIMEOUT));
    let begin = Instant::now();
    if let Err(error) = socket.connect((PUBLIC_WEB, 80)).await {
        println!("TCP to {}:80: connect ERROR {:?}", PUBLIC_WEB, error);
        return false;
    }
    let connected = begin.elapsed().as_millis();
    if write_all(&mut socket, PUBLIC_WEB_REQUEST).await.is_err() {
        return false;
    }
    let mut answer = [0; 4096];
    let mut total = 0;
    let mut first = [0u8; 64];
    let mut first_length = 0;
    loop {
        match socket.read(&mut answer).await {
            Ok(0) => break,
            Ok(read) => {
                if total == 0 {
                    first_length = find(&answer[..read], b"\r\n").unwrap_or(read).min(first.len());
                    first[..first_length].copy_from_slice(&answer[..first_length]);
                }
                total += read;
            }
            Err(error) => {
                println!("TCP read ERROR {:?}", error);
                return false;
            }
        }
    }
    socket.close();
    let _ = socket.flush().await;
    println!(
        "TCP to {}:80: connected in {} ms; {} bytes in {} ms, \"{}\"",
        PUBLIC_WEB,
        connected,
        total,
        begin.elapsed().as_millis(),
        core::str::from_utf8(&first[..first_length]).unwrap_or("?")
    );
    total > 0
}

#[embassy_executor::task]
async fn main_task(spawner: embassy_executor::Spawner, stack: Stack<'static>) {
    let begin = Instant::now();
    stack.wait_config_up().await;
    let Some(config) = stack.config_v4() else {
        println!("No IPv4 configuration (MISMATCH)");
        return;
    };
    println!(
        "DHCP: {} in {} ms, router {:?}, name servers {:?}",
        config.address,
        begin.elapsed().as_millis(),
        config.gateway,
        config.dns_servers
    );
    for _ in 0..HTTP_SERVERS {
        spawner.spawn(http_task(stack, HTTP_PORT).unwrap());
    }
    let fetched = fetch(stack).await;
    println!("TCP: {}", verdict(fetched));
    println!(
        "Serving: http://{}/ and /data, POST /upload; pings answered. Keys: s report, r restart, f fastboot",
        config.address.address()
    );
}

fn main(_args: BootArgs) -> ! {
    println!("\nBCM4334 under Embassy: embassy-net, woken by the host wake line");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();
    console::flush();
    buffer_console();

    let joined = match join(mct) {
        Ok(Some(joined)) => joined,
        Ok(None) => halt(),
        Err(error) => {
            println!("Bring-up ERROR {:?}", error);
            halt();
        }
    };

    // SAFETY: the only references to these, taken once, here.
    let (state, resources) = unsafe { (&mut *addr_of_mut!(WIFI_STATE), &mut *addr_of_mut!(RESOURCES)) };
    let (device, runner) = wifi::new(state, joined.chip, joined.supplicant, report);
    let (stack, net_runner) = embassy_net::new(device, Config::dhcpv4(Default::default()), resources, joined.seed);

    embassy::run(|spawner| {
        spawner.spawn(wifi_task(runner).unwrap());
        spawner.spawn(net_task(net_runner).unwrap());
        spawner.spawn(ticker().unwrap());
        spawner.spawn(report_task().unwrap());
        spawner.spawn(keys_task().unwrap());
        spawner.spawn(main_task(spawner, stack).unwrap());
    })
}
