//! The WiFi settings and the chip's firmware, read from CACHE at power-on ([`load`]).
//!
//! The board keeps them in `SYSTEM/` on CACHE, where the application (ferrite-ui-xl) reads the
//! same files, so neither needs them built in:
//!
//! | File | Content |
//! |---|---|
//! | `SYSTEM/WIFI.TXT` | `key=value` lines, `#` comment lines: `ssid=`, `password=`, optional `band=2.4` or `5`, `hostname=` |
//! | `SYSTEM/WIFIFW.BIN` | `brcmfmac4334-sdio.samsung,mehmet-board.bin` |
//! | `SYSTEM/WIFINV.TXT` | `brcmfmac4334-sdio.samsung,mehmet-board.txt` |
//!
//! They arrive with `uartboot.py --system FILE` / `httpboot.py --system FILE` (`EXYNSYSF`,
//! `POST /system/<NAME>`), or from the application's setup screen. Each one falls back, in this
//! order, to the copy ferrite-ui-xl used to keep in `FERRITE/` (`WIFI.CFG`, `WIFIFW.BIN`,
//! `WIFINV.TXT`), then to what the build gave (`WIFI_SSID`, `WIFI_PASSWORD`, `WIFI_BAND`,
//! `WIFI_FIRMWARE_DIR`: see `build.rs`).

use crate::cache::{self, DIR, SYSTEM_DIR};
use exynos_4412_rs::println;

pub const WIFI_FILE: &str = "WIFI.TXT";
pub const FIRMWARE_FILE: &str = "WIFIFW.BIN";
pub const NVRAM_FILE: &str = "WIFINV.TXT";
/// What ferrite-ui-xl called the settings in `FERRITE/` before `SYSTEM/`.
const LEGACY_WIFI_FILE: &str = "WIFI.CFG";
pub const DEFAULT_HOSTNAME: &str = "exynos";

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/firmware.bin"));
static NVRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nvram.txt"));

/// Where firmware and NVRAM read from CACHE go: DRAM above [`STAGE`](crate::STAGE)'s
/// [`MAX_LEN`](crate::MAX_LEN), which nothing else here uses.
const FILES: usize = 0x5800_0000;
const FIRMWARE_MAX: usize = 2 << 20;
const NVRAM_MAX: usize = 64 << 10;
const TEXT_MAX: usize = 2048;

const SSID_MAX: usize = 32;
const PASSWORD_MAX: usize = 64;
const HOSTNAME_MAX: usize = 32;

/// The network to join and what the chip needs to join it.
pub struct Settings {
    ssid: [u8; SSID_MAX],
    ssid_len: usize,
    password: [u8; PASSWORD_MAX],
    password_len: usize,
    /// 2 = 2.4 GHz only, 5 = 5 GHz only, 0 = either.
    pub band: u8,
    hostname: [u8; HOSTNAME_MAX],
    hostname_len: usize,
    pub firmware: &'static [u8],
    pub nvram: &'static [u8],
}

impl Settings {
    pub fn ssid(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.ssid_len]).unwrap_or("")
    }

    pub fn password(&self) -> &[u8] {
        &self.password[..self.password_len]
    }

    pub fn hostname(&self) -> &str {
        match core::str::from_utf8(&self.hostname[..self.hostname_len]) {
            Ok(name) if !name.is_empty() => name,
            _ => DEFAULT_HOSTNAME,
        }
    }

    /// Whether an access point on `channel` may be joined.
    pub fn band_allows(&self, channel: u8) -> bool {
        const LAST_2G_CHANNEL: u8 = 14;
        match self.band {
            2 => channel <= LAST_2G_CHANNEL,
            5 => channel > LAST_2G_CHANNEL,
            _ => true,
        }
    }
}

fn copy_into(field: &mut [u8], len: &mut usize, value: &[u8]) {
    let n = value.len().min(field.len());
    field[..n].copy_from_slice(&value[..n]);
    *len = n;
}

fn parse_band(value: &[u8]) -> u8 {
    match value {
        b"2.4" | b"2" => 2,
        b"5" => 5,
        _ => 0,
    }
}

/// `key=value` lines into `s`; unknown keys are left alone. Comments are whole lines starting
/// with `#` (a passphrase may hold one), or follow `band` / `hostname` values. Same rules as
/// ferrite-ui-xl's `ferrite_core::setup::WifiSettings::parse` — keep them in sync.
fn parse(text: &[u8], s: &mut Settings) {
    for line in text.split(|&byte| byte == b'\n') {
        let line = line.trim_ascii();
        if line.starts_with(b"#") {
            continue;
        }
        let Some(eq) = line.iter().position(|&byte| byte == b'=') else { continue };
        let (key, value) = (line[..eq].trim_ascii(), line[eq + 1..].trim_ascii());
        let short = value.split(|&byte| byte == b'#').next().unwrap_or(b"").trim_ascii();
        if key.eq_ignore_ascii_case(b"ssid") {
            copy_into(&mut s.ssid, &mut s.ssid_len, value);
        } else if key.eq_ignore_ascii_case(b"password") || key.eq_ignore_ascii_case(b"psk") {
            copy_into(&mut s.password, &mut s.password_len, value);
        } else if key.eq_ignore_ascii_case(b"band") {
            s.band = parse_band(short);
        } else if key.eq_ignore_ascii_case(b"hostname") {
            copy_into(&mut s.hostname, &mut s.hostname_len, short);
        }
    }
}

/// `name` from `SYSTEM/`, else `FERRITE/<legacy>`, into `buf`: its length and where it came from.
fn read(name: &str, legacy: &str, buf: &mut [u8]) -> Option<(usize, &'static str)> {
    if let Ok(len) = cache::read_file(SYSTEM_DIR, name, buf) {
        return Some((len, SYSTEM_DIR));
    }
    cache::read_file(DIR, legacy, buf).ok().map(|len| (len, DIR))
}

/// A file read into DRAM at `at`, at most `max` bytes, with its source for the log.
fn read_to(at: usize, max: usize, name: &str) -> Option<(&'static [u8], &'static str)> {
    // SAFETY: FILES..FILES + FIRMWARE_MAX + NVRAM_MAX is DRAM nothing else uses (see FILES), read
    // once, at power-on, before anything takes these slices; afterwards it is only read.
    let buf = unsafe { core::slice::from_raw_parts_mut(at as *mut u8, max) };
    let (len, from) = read(name, name, buf)?;
    Some((&buf[..len], from))
}

/// Reads the settings and the chip's files from CACHE (or what the build gave). `None`: no
/// network named, or no firmware for the chip; the reason is printed.
pub fn load() -> Option<Settings> {
    let mut s = Settings {
        ssid: [0; SSID_MAX],
        ssid_len: 0,
        password: [0; PASSWORD_MAX],
        password_len: 0,
        band: 0,
        hostname: [0; HOSTNAME_MAX],
        hostname_len: 0,
        firmware: FIRMWARE,
        nvram: NVRAM,
    };
    let mut text = [0u8; TEXT_MAX];
    match read(WIFI_FILE, LEGACY_WIFI_FILE, &mut text) {
        Some((len, from)) => {
            parse(&text[..len], &mut s);
            let name = if from == SYSTEM_DIR { WIFI_FILE } else { LEGACY_WIFI_FILE };
            println!("uart_boot: WiFi settings from {}/{}", from, name);
        }
        None => {
            copy_into(&mut s.ssid, &mut s.ssid_len, option_env!("WIFI_SSID").unwrap_or("").as_bytes());
            copy_into(&mut s.password, &mut s.password_len, option_env!("WIFI_PASSWORD").unwrap_or("").as_bytes());
            s.band = parse_band(option_env!("WIFI_BAND").unwrap_or("").as_bytes());
        }
    }
    if s.ssid_len == 0 {
        println!("uart_boot: no WiFi ({}/{} names no network)", SYSTEM_DIR, WIFI_FILE);
        return None;
    }
    let firmware = read_to(FILES, FIRMWARE_MAX, FIRMWARE_FILE);
    let nvram = read_to(FILES + FIRMWARE_MAX, NVRAM_MAX, NVRAM_FILE);
    match (firmware, nvram) {
        (Some((fw, from)), Some((nv, _))) if !fw.is_empty() && !nv.is_empty() => {
            println!("uart_boot: WiFi chip firmware from {}/, {} bytes", from, fw.len());
            s.firmware = fw;
            s.nvram = nv;
        }
        _ if !FIRMWARE.is_empty() => {}
        _ => {
            println!("uart_boot: no WiFi (no {}/{} + {})", SYSTEM_DIR, FIRMWARE_FILE, NVRAM_FILE);
            return None;
        }
    }
    Some(s)
}

/// A `SYSTEM/` file name as sent (NUL-padded): upper-cased into `out`, if it is an 8.3 name of
/// letters, digits, `_` and `-`. The name.
pub fn system_name<'a>(raw: &[u8], out: &'a mut [u8; 12]) -> Option<&'a str> {
    let raw = raw.split(|&byte| byte == 0).next().unwrap_or(b"");
    let (base, ext) = match raw.iter().position(|&byte| byte == b'.') {
        Some(dot) => (&raw[..dot], &raw[dot + 1..]),
        None => (raw, &b""[..]),
    };
    let ok = |part: &[u8], max: usize| {
        part.len() <= max && part.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
    };
    if base.is_empty() || !ok(base, 8) || !ok(ext, 3) || (ext.is_empty() && raw.ends_with(b".")) {
        return None;
    }
    for (o, byte) in out.iter_mut().zip(raw) {
        *o = byte.to_ascii_uppercase();
    }
    core::str::from_utf8(&out[..raw.len()]).ok()
}
