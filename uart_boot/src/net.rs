//! Images over WiFi: the chip joined as in `wifi_async_test`, `embassy-net` with DHCP, an HTTP
//! server on port 80, and the UART protocol of [`crate`] at the same time.
//!
//! # HTTP
//!
//! | Request | Does |
//! |---|---|
//! | `GET /` | a page saying what is here |
//! | `POST /boot` (or `PUT`) | runs the body from RAM, like `EXYNBOOT` |
//! | `POST /save` (or `PUT`) | saves the body to CACHE, then runs it, like `EXYNSAVE` |
//! | `POST /stay` | test mode on: the saved application no longer boots by itself (key `s`) |
//! | `POST /autoboot` | test mode off, from the next power-on (key `a`) |
//!
//! The body is a `.bin`, or a `boot.img` whose kernel is then taken. It needs a
//! `Content-Length`; `Expect: 100-continue` is answered. An `X-CRC32` header (8 hex digits, the
//! CRC-32 of the body as sent) is checked when present: TCP's own checksums are weak, and
//! `tools/httpboot.py` always sends it. The answer comes once the image is checked (and saved),
//! then the image starts:
//!
//! ```text
//! curl --data-binary @target/armv7a-none-eabihf/release/hello_world.bin http://<address>/boot
//! ```
//!
//! The address comes by DHCP, and is printed; the name [`HOSTNAME`] goes with the request, which
//! many routers then resolve.
//!
//! # One image at a time
//!
//! [`STAGE`](crate::STAGE) has one image. Whoever starts receiving first, HTTP or the UART, claims
//! it ([`Claim`]); the other is turned away until that one fails. The saved application boots,
//! [`HTTP_WINDOW`] after the address came, only if no one holds the claim, no connection came,
//! and test mode is off (see [`crate`]).
//!
//! # State
//!
//! This turns on the MMU (not the L2 cache), the GIC, interrupts and Embassy's time on the MCT,
//! and buffers the console: [`crate::start`] undoes what an application would mind.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, DhcpConfig, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use exynos_4412_rs::crypto::{self, Hmac};
use exynos_4412_rs::drivers::bcm4334::{self, Activity, Bcm4334, Error};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::sdhci::Sdhci;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::uart::{UartRx, buffer_console};
use exynos_4412_rs::embassy::wifi;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::wpa::{self, Supplicant};
use exynos_4412_rs::{BootArgs, console, cpu, embassy, mmu, println, soc};

use crate::{GAP_MS, MAX_LEN, Request, crc};

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

/// The name given to DHCP.
pub const HOSTNAME: &str = "exynos";
/// How long HTTP gets, once there is an address, before the saved application boots.
pub const HTTP_WINDOW: Duration = Duration::from_secs(5);
/// How long DHCP gets before the saved application boots without an address.
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);

const RX_FRAMES: usize = 16;
const TX_FRAMES: usize = 16;
/// DHCP's socket and the servers', with room to spare.
const SOCKETS: usize = 4;
const HTTP_PORT: u16 = 80;
/// One to take an image, one to say no to a second meanwhile.
const HTTP_SERVERS: usize = 2;
const SERVER_RX: usize = 32 * 1024;
const SERVER_TX: usize = 4 * 1024;
const REQUEST_MAX: usize = 1024;
/// How long a connection may stay silent.
const TCP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the answer gets to reach the client before the image starts.
const ANSWER_LINGER: Duration = Duration::from_secs(1);
const ANDROID_MAGIC: &[u8; 8] = b"ANDROID!";
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

static mut WIFI_STATE: wifi::State<RX_FRAMES, TX_FRAMES> = wifi::State::new();
static mut RESOURCES: StackResources<SOCKETS> = StackResources::new();

/// U-Boot's registers, for [`crate::start`]: written once in [`run`], before the executor.
static mut ARGS: Option<BootArgs> = None;
/// An image checked at [`STAGE`](crate::STAGE): its length. The boot task starts it.
static READY: Signal<CriticalSectionRawMutex, usize> = Signal::new();
/// Held by whoever is writing [`STAGE`](crate::STAGE): see [`Claim`].
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// The right to write [`STAGE`](crate::STAGE), given back when dropped unless the image in it is
/// going to start ([`keep`](Self::keep)).
struct Claim;

impl Claim {
    fn take() -> Option<Self> {
        (!CLAIMED.swap(true, Ordering::AcqRel)).then_some(Self)
    }

    /// The image is going to start: no one else may write over it.
    fn keep(self) {
        core::mem::forget(self);
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        CLAIMED.store(false, Ordering::Release);
    }
}

/// Whether the build has what WiFi needs.
pub fn configured() -> bool {
    SSID.is_some() && PASSWORD.is_some() && !FIRMWARE.is_empty()
}

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if !embassy::on_interrupt(irq.id()) {
        println!("uart_boot: unexpected interrupt {}", irq.id());
    }
    gic.end_of_interrupt(irq);
}

fn report(activity: Activity) {
    match activity {
        Activity::Event(event) if event.code == bcm4334::EVENT_DEAUTH_IND || event.code == bcm4334::EVENT_DISASSOC_IND => {
            println!("uart_boot: WiFi event {}, reason {}", event.code, event.reason);
        }
        Activity::Ignored(error) => println!("uart_boot: handshake frame ignored, {:?}", error),
        _ => {}
    }
}

fn band_allows(channel: u8) -> bool {
    match BAND {
        Some("2.4") => channel <= LAST_2G_CHANNEL,
        Some("5") => channel > LAST_2G_CHANNEL,
        _ => true,
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

/// What joining hands on.
struct Joined {
    chip: Bcm4334,
    supplicant: Supplicant,
    seed: u64,
}

/// Powers the chip up, starts its firmware and host wake line, and joins the named network, as
/// `wifi_async_test` does. `Ok(None)` when the network is not there.
fn join(mct: Mct) -> Result<Option<Joined>, Error> {
    let (Some(ssid), Some(password)) = (SSID, PASSWORD) else {
        return Ok(None);
    };
    let i2c = I2c::new(Bus::I2c0);
    if let Err(error) = i2c.init(400_000, 100) {
        println!("uart_boot: I2C ERROR {:?}", error);
        return Ok(None);
    }
    let begin = mct.count();
    bcm4334::power_on(Max77686::new(i2c), mct)?;
    let (mut chip, _rate) = Bcm4334::start(Sdhci::new()?, mct, bcm4334::HIGH_SPEED_MAX_HZ)?;
    chip.attach()?;
    chip.load_firmware(FIRMWARE, NVRAM)?;
    chip.start_firmware()?;
    chip.enable_host_wake()?;
    chip.disable_glomming()?;
    let mut address = [0; 32];
    chip.get_variable("cur_etheraddr", &mut address)?;
    let mut own = [0; 6];
    own.copy_from_slice(&address[..6]);

    chip.prepare_radio()?;
    let mut target: Option<Target> = None;
    let mut pool = Hmac::new(&own).sign(&[&mct.count().to_le_bytes()[..]]);
    chip.scan(SCAN_TIMEOUT_MS, |bss, rsn| {
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
    let Some(target) = target else {
        println!("uart_boot: network {} not found (or not in WIFI_BAND)", ssid);
        return Ok(None);
    };

    let key = crypto::wpa_key(password.as_bytes(), ssid.as_bytes());
    let mut nonce = [0; 32];
    crypto::prf(&pool, b"Init Counter", &mct.count().to_le_bytes(), &mut nonce);
    let mut seed = [0; 8];
    crypto::prf(&pool, b"Stack seed", &mct.count().to_le_bytes(), &mut seed);
    let Some(mut supplicant) = Supplicant::new(key, own, target.bssid, &target.rsn[..target.rsn_length], nonce) else {
        println!("uart_boot: the access point asks for what this cannot do");
        return Ok(None);
    };
    chip.connect(ssid.as_bytes(), &mut supplicant, JOIN_TIMEOUT_MS, report)?;
    println!(
        "uart_boot: joined {} in {} ms, channel {}, {} dBm",
        ssid,
        (mct.count() - begin) / COUNTS_PER_MS,
        target.channel,
        target.rssi
    );
    Ok(Some(Joined { chip, supplicant, seed: u64::from_le_bytes(seed) }))
}

/// Joins WiFi and serves images until one starts. Returns only if WiFi did not come up, with
/// the MMU, the GIC and interrupts on; the console stays buffered.
pub fn run(mct: Mct, args: BootArgs) {
    println!("uart_boot: joining WiFi");
    console::flush();
    mmu::enable();
    Gic::new().init();
    embassy::init();
    cpu::enable_irq();
    buffer_console();

    let joined = match join(mct) {
        Ok(Some(joined)) => joined,
        Ok(None) => return,
        Err(error) => {
            println!("uart_boot: WiFi ERROR {:?}", error);
            return;
        }
    };

    let mut dhcp = DhcpConfig::default();
    dhcp.hostname = HOSTNAME.try_into().ok();
    // SAFETY: the only references to these, taken once, here, before any task runs.
    let (state, resources) = unsafe {
        ARGS = Some(args);
        (&mut *addr_of_mut!(WIFI_STATE), &mut *addr_of_mut!(RESOURCES))
    };
    let (device, runner) = wifi::new(state, joined.chip, joined.supplicant, report);
    let (stack, net_runner) = embassy_net::new(device, Config::dhcpv4(dhcp), resources, joined.seed);

    embassy::run(|spawner| {
        spawner.spawn(wifi_task(runner).unwrap());
        spawner.spawn(net_task(net_runner).unwrap());
        spawner.spawn(uart_task().unwrap());
        for _ in 0..HTTP_SERVERS {
            spawner.spawn(http_task(stack).unwrap());
        }
        spawner.spawn(boot_task(stack).unwrap());
    })
}

#[embassy_executor::task]
async fn wifi_task(runner: wifi::Runner<'static>) {
    let error = runner.run().await;
    println!("uart_boot: WiFi stopped: {:?}", error);
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, wifi::Device<'static>>) {
    runner.run().await
}

/// Starts what comes ready, or the saved application once the window is over.
#[embassy_executor::task]
async fn boot_task(stack: Stack<'static>) {
    let up = with_timeout(DHCP_TIMEOUT, stack.wait_config_up()).await.is_ok();
    match stack.config_v4() {
        Some(config) if up => {
            let address = config.address.address();
            println!("uart_boot: http://{}/ ({}): POST /boot or /save", address, HOSTNAME);
            println!("uart_boot: waiting for an image (HTTP or UART)");
        }
        _ => println!("uart_boot: no address by DHCP; waiting for an image (UART)"),
    }
    let mut autoboot = (!crate::stay()).then(|| Instant::now() + if up { HTTP_WINDOW } else { Duration::from_ticks(0) });
    let len = loop {
        let Some(at) = autoboot else {
            break READY.wait().await;
        };
        match select(READY.wait(), Timer::at(at)).await {
            Either::First(len) => break len,
            Either::Second(()) => {
                autoboot = None;
                if crate::stay() {
                    println!("uart_boot: not booting {}: held", crate::cache::FILE);
                    continue;
                }
                // An image on its way goes first.
                let Some(claim) = Claim::take() else {
                    continue;
                };
                if let Some(len) = crate::load_saved() {
                    claim.keep();
                    break len;
                }
                println!("uart_boot: waiting for an image");
            }
        }
    };
    // SAFETY: written before the executor started, only read since.
    let args = unsafe { *addr_of_mut!(ARGS) }.expect("set in run");
    crate::start(Mct::new(), len, &args);
}

// --- UART -----------------------------------------------------------------------------------

/// The UART protocol of [`crate`], with `await`.
#[embassy_executor::task]
async fn uart_task() {
    let rx = UartRx::new(console::uart().id()).expect("the console UART has an interrupt");
    // The last 8 bytes received, oldest first.
    let mut window = [0u8; 8];
    loop {
        let byte = rx.read_byte().await;
        window.copy_within(1.., 0);
        window[7] = byte;
        let Some(request) = Request::from_magic(&window) else {
            crate::key(byte);
            continue;
        };
        window = [0; 8];
        let Some(claim) = Claim::take() else {
            println!("uart_boot: ERROR busy: an image is coming over HTTP");
            // The image's bytes must not be read as keys: dropped up to a gap.
            let mut sink = [0; 64];
            while with_timeout(Duration::from_millis(GAP_MS), rx.read(&mut sink)).await.is_ok() {}
            println!("uart_boot: waiting again");
            continue;
        };
        match receive_uart(&rx).await {
            Ok((len, crc)) => {
                if request == Request::Save {
                    crate::save(len, crc);
                }
                claim.keep();
                READY.signal(len);
                return;
            }
            Err(_) => println!("uart_boot: waiting again"),
        }
    }
}

async fn word(rx: &UartRx) -> Option<u32> {
    let mut bytes = [0; 4];
    for byte in &mut bytes {
        *byte = with_timeout(Duration::from_millis(GAP_MS), rx.read_byte()).await.ok()?;
    }
    Some(u32::from_le_bytes(bytes))
}

/// [`crate::receive`], from the UART's ring.
async fn receive_uart(rx: &UartRx) -> Result<(usize, u32), &'static str> {
    let len = word(rx).await.ok_or("no length")? as usize;
    let want = word(rx).await.ok_or("no CRC")?;
    if !crate::length_ok(len) {
        return Err("length");
    }
    println!("uart_boot: receiving {} bytes", len);
    let stage = crate::staged(len);
    let mut done = 0;
    while done < len {
        match with_timeout(Duration::from_millis(GAP_MS), rx.read(&mut stage[done..])).await {
            Ok(count) => done += count,
            Err(_) => {
                println!("uart_boot: ERROR timeout after {} of {} bytes", done, len);
                return Err("timeout");
            }
        }
    }
    crate::check(len, crc::crc32(stage), want)?;
    Ok((len, want))
}

// --- HTTP -----------------------------------------------------------------------------------

#[embassy_executor::task(pool_size = HTTP_SERVERS)]
async fn http_task(stack: Stack<'static>) {
    let mut rx = [0; SERVER_RX];
    let mut tx = [0; SERVER_TX];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(TCP_TIMEOUT));
        if socket.accept(HTTP_PORT).await.is_err() {
            continue;
        }
        // Someone is about to send: the saved application waits.
        crate::hold();
        let ready = match serve(&mut socket).await {
            Ok(ready) => ready,
            Err(error) => {
                println!("uart_boot: HTTP {:?}", error);
                None
            }
        };
        socket.close();
        let _ = with_timeout(ANSWER_LINGER, socket.flush()).await;
        socket.abort();
        if let Some(len) = ready {
            READY.signal(len);
            return;
        }
    }
}

type TcpResult<T> = Result<T, embassy_net::tcp::Error>;

async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> TcpResult<()> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        data = &data[written..];
    }
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The value of header `name` (lower case) in a request head, trimmed.
fn header<'a>(head: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    head.split(|&byte| byte == b'\n').skip(1).find_map(|line| {
        let colon = line.iter().position(|&byte| byte == b':')?;
        line[..colon].eq_ignore_ascii_case(name).then(|| line[colon + 1..].trim_ascii())
    })
}

fn parse_decimal(text: &[u8]) -> Option<usize> {
    if text.is_empty() || text.len() > 10 {
        return None;
    }
    text.iter().try_fold(0usize, |value, &byte| {
        byte.is_ascii_digit().then(|| value.checked_mul(10)?.checked_add(usize::from(byte - b'0')))?
    })
}

fn parse_hex(text: &[u8]) -> Option<u32> {
    let text = text.strip_prefix(b"0x").unwrap_or(text);
    if text.is_empty() || text.len() > 8 {
        return None;
    }
    text.iter().try_fold(0u32, |value, &byte| Some(value << 4 | (byte as char).to_digit(16)?))
}

/// Text built with `write!`, up to 256 bytes; more is dropped.
struct Text {
    bytes: [u8; 256],
    len: usize,
}

impl Text {
    const fn new() -> Self {
        Self { bytes: [0; 256], len: 0 }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl core::fmt::Write for Text {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// The image in `stage`: all of it, or the kernel of a `boot.img`, moved to the start. `None`
/// for a `boot.img` header that points outside.
fn kernel_of(stage: &mut [u8]) -> Option<usize> {
    if !stage.starts_with(ANDROID_MAGIC) {
        return Some(stage.len());
    }
    let field = |at: usize| Some(u32::from_le_bytes(stage.get(at..at + 4)?.try_into().ok()?) as usize);
    // boot_img_hdr: kernel_size at 8, page_size at 36; the kernel starts one page in.
    let (size, page) = (field(8)?, field(36)?);
    let end = page.checked_add(size)?;
    if page == 0 || end > stage.len() {
        return None;
    }
    stage.copy_within(page..end, 0);
    println!("uart_boot: boot.img, kernel of {} bytes", size);
    Some(size)
}

/// Sends a whole answer: `status`, then `body` as text.
async fn answer(socket: &mut TcpSocket<'_>, status: &str, body: &[u8]) -> TcpResult<()> {
    let mut head = Text::new();
    let _ = core::fmt::write(
        &mut head,
        format_args!(
            "HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status,
            body.len()
        ),
    );
    write_all(socket, head.bytes()).await?;
    write_all(socket, body).await?;
    socket.flush().await
}

const USAGE: &[u8] = b"uart_boot on the Exynos 4412\n\
POST (or PUT) a .bin to /boot to run it, to /save to save it to CACHE and run it.\n\
POST /stay: test mode, nothing boots by itself; POST /autoboot ends it.\n\
Optional header X-CRC32: <8 hex digits>, the CRC-32 of the body.\n\
curl --data-binary @app.bin http://<this address>/boot\n";

/// Answers one request. `Some(length)` when an image is checked at the stage, claimed, and is
/// to start once the connection is closed.
async fn serve(socket: &mut TcpSocket<'_>) -> TcpResult<Option<usize>> {
    let mut request = [0; REQUEST_MAX];
    let mut filled = 0;
    let head_end = loop {
        if filled == request.len() {
            answer(socket, "431 Request Header Fields Too Large", b"Head too long\n").await?;
            return Ok(None);
        }
        let read = socket.read(&mut request[filled..]).await?;
        if read == 0 {
            return Ok(None);
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
    let request_kind = match path {
        b"/boot" => Some(Request::Boot),
        b"/save" => Some(Request::Save),
        _ => None,
    };
    if method == b"POST" && (path == b"/stay" || path == b"/autoboot") {
        let on = path == b"/stay";
        let body: &[u8] = match (crate::set_test_mode(on), on) {
            (true, true) => b"Test mode on: nothing boots by itself\n",
            (true, false) => b"Test mode off: the saved application boots from the next power-on\n",
            (false, _) => b"Test mode ERROR: see the console\n",
        };
        answer(socket, "200 OK", body).await?;
        return Ok(None);
    }
    match (method, request_kind) {
        (b"POST" | b"PUT", Some(kind)) => {
            let length = header(head, b"content-length").and_then(parse_decimal);
            let want = header(head, b"x-crc32").and_then(parse_hex);
            let expect = header(head, b"expect").is_some_and(|value| value.eq_ignore_ascii_case(b"100-continue"));
            let early = &request[head_end..filled];
            upload(socket, kind, length, want, expect, early).await
        }
        (b"GET", None) if path == b"/" => {
            answer(socket, "200 OK", USAGE).await?;
            Ok(None)
        }
        (b"GET", _) => {
            answer(socket, "404 Not Found", b"Not found: GET /, POST /boot or /save\n").await?;
            Ok(None)
        }
        _ => {
            answer(socket, "405 Method Not Allowed", b"GET /, POST /boot or /save\n").await?;
            Ok(None)
        }
    }
}

/// Takes the body of a `/boot` or `/save` into the stage. `early` is what came with the head.
async fn upload(
    socket: &mut TcpSocket<'_>,
    kind: Request,
    length: Option<usize>,
    want: Option<u32>,
    expect: bool,
    early: &[u8],
) -> TcpResult<Option<usize>> {
    let Some(length) = length else {
        answer(socket, "411 Length Required", b"Send a Content-Length\n").await?;
        return Ok(None);
    };
    if !(1..=MAX_LEN).contains(&length) {
        answer(socket, "413 Content Too Large", b"At most 64 MiB\n").await?;
        return Ok(None);
    }
    let Some(claim) = Claim::take() else {
        answer(socket, "409 Conflict", b"Busy: another image is coming\n").await?;
        return Ok(None);
    };
    if expect {
        write_all(socket, CONTINUE).await?;
    }
    if let Some(peer) = socket.remote_endpoint() {
        println!("uart_boot: receiving {} bytes over HTTP from {}", length, peer.addr);
    }
    let stage = crate::staged(length);
    let mut done = early.len().min(length);
    stage[..done].copy_from_slice(&early[..done]);
    let begin = Instant::now();
    while done < length {
        let read = socket.read(&mut stage[done..]).await?;
        if read == 0 {
            println!("uart_boot: ERROR HTTP body ended after {} of {} bytes", done, length);
            return Ok(None);
        }
        done += read;
    }
    let ms = begin.elapsed().as_millis().max(1);
    let got = crc::crc32(stage);
    println!("uart_boot: {} bytes in {} ms, {} KiB/s, CRC {:#010x}", length, ms, length as u64 * 1000 / 1024 / ms, got);
    if let Some(want) = want
        && want != got
    {
        println!("uart_boot: ERROR CRC {:#010x}, sent {:#010x}", got, want);
        answer(socket, "400 Bad Request", b"CRC mismatch: X-CRC32 does not match the body\n").await?;
        return Ok(None);
    }

    let (len, crc) = match kernel_of(stage) {
        Some(len) => (len, crc::crc32(&stage[..len])),
        None => {
            answer(socket, "400 Bad Request", b"boot.img header out of range\n").await?;
            return Ok(None);
        }
    };
    if crate::check(len, crc, crc).is_err() {
        answer(socket, "400 Bad Request", b"No zImage magic at 0x24: send a .bin from make bin\n").await?;
        return Ok(None);
    }
    let saved = kind == Request::Save && crate::save(len, crc);
    let mut body = Text::new();
    let _ = core::fmt::write(
        &mut body,
        format_args!(
            "CRC OK, {} bytes{}; starting\n",
            len,
            match (kind, saved) {
                (Request::Boot, _) => "",
                (Request::Save, true) => ", saved",
                (Request::Save, false) => ", save FAILED (see the console)",
            }
        ),
    );
    answer(socket, "200 OK", body.bytes()).await?;
    claim.keep();
    Ok(Some(len))
}
