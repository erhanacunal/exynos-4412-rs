//! A small HTTP/1.0 server, one connection per `smoltcp` TCP socket.
//!
//! `/` is a plain text page about the station, and `/data` is [`DATA_BYTES`] of text, to measure
//! how fast TCP sends. Each response closes its connection. The client is left to close first, so
//! that the 10 s wait after a close falls on its side and the socket can listen again at once.

use core::fmt::{self, Write};

use exynos_4412_rs::println;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{self, State};
use smoltcp::wire::Ipv4Address;

/// What happened to a connection, for its caller.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Nothing,
    /// A request was answered.
    Answered,
    /// All of `/data` was acknowledged.
    DataAcknowledged,
}
/// Bytes that `/data` sends.
pub const DATA_BYTES: u32 = 1 << 20;
/// Bytes of a request that are kept. Only its first line is used.
const REQUEST_MAX: usize = 512;
/// Bytes of a response's header, and of the whole response when it is not `/data`.
const HEAD_MAX: usize = 1024;
/// How long a connection may stay without a whole request. Browsers open some ahead of time.
const REQUEST_TIMEOUT_MS: u64 = 5_000;
/// How long the client gets to close, once it has everything.
const LINGER_MS: u64 = 5_000;
/// What `/data` repeats: 64 bytes, a line of text.
const LINE: &[u8; 64] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.\n";

/// What the page at `/` reports.
pub struct Status {
    pub uptime_s: u64,
    pub address: Option<Ipv4Address>,
    pub router: Option<Ipv4Address>,
    pub mac: [u8; 6],
    pub pings: u32,
    /// Requests answered before the one this page answers.
    pub requests: u32,
}

/// Text written into a fixed buffer. What does not fit is an error, and is left out.
struct Text<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> Text<N> {
    const fn new() -> Self {
        Self { bytes: [0; N], length: 0 }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

impl<const N: usize> Write for Text<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.length + text.len();
        let space = self.bytes.get_mut(self.length..end).ok_or(fmt::Error)?;
        space.copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

/// Shows bytes from the network as text, with `?` for what is not printable ASCII.
struct Ascii<'a>(&'a [u8]);

impl fmt::Display for Ascii<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.0 {
            f.write_char(if byte.is_ascii_graphic() || byte == b' ' { byte as char } else { '?' })?;
        }
        Ok(())
    }
}

enum Phase {
    /// Listening, or reading a request.
    Request,
    /// Queueing the response.
    Sending,
    /// All of the response is queued: waiting for it to be acknowledged, then for the client to
    /// close.
    Linger,
}

/// One connection's worth of server.
pub struct Server {
    handle: SocketHandle,
    port: u16,
    phase: Phase,
    /// When the current phase, or the connection, began, in ms.
    since_ms: u64,
    request: [u8; REQUEST_MAX],
    request_length: usize,
    head: Text<HEAD_MAX>,
    head_sent: usize,
    data_left: u32,
    data_total: u32,
    /// When the request was answered, in ms.
    answered_ms: u64,
    acknowledged: bool,
}

impl Server {
    /// Serves on `port` with a TCP socket of the set, which this then owns.
    pub const fn new(handle: SocketHandle, port: u16) -> Self {
        Self {
            handle,
            port,
            phase: Phase::Request,
            since_ms: 0,
            request: [0; REQUEST_MAX],
            request_length: 0,
            head: Text::new(),
            head_sent: 0,
            data_left: 0,
            data_total: 0,
            answered_ms: 0,
            acknowledged: false,
        }
    }

    pub const fn handle(&self) -> SocketHandle {
        self.handle
    }

    fn reset(&mut self, now_ms: u64) {
        self.phase = Phase::Request;
        self.since_ms = now_ms;
        self.request_length = 0;
        self.head = Text::new();
        self.head_sent = 0;
        self.data_left = 0;
        self.data_total = 0;
        self.acknowledged = false;
    }

    /// Does what the connection needs now.
    pub fn service(&mut self, socket: &mut tcp::Socket, now_ms: u64, status: &Status) -> Event {
        if !socket.is_open() {
            // A closed socket and a nonzero port: this cannot fail.
            let _ = socket.listen(self.port);
            self.reset(now_ms);
            return Event::Nothing;
        }
        match self.phase {
            Phase::Request => {
                if self.read_request(socket, now_ms, status) {
                    Event::Answered
                } else {
                    Event::Nothing
                }
            }
            Phase::Sending => {
                self.send(socket, now_ms);
                Event::Nothing
            }
            Phase::Linger => self.linger(socket, now_ms),
        }
    }

    fn read_request(&mut self, socket: &mut tcp::Socket, now_ms: u64, status: &Status) -> bool {
        if matches!(socket.state(), State::Listen | State::SynReceived) {
            self.since_ms = now_ms;
            return false;
        }
        if socket.can_recv()
            && let Ok(count) = socket.recv_slice(&mut self.request[self.request_length..])
        {
            self.request_length += count;
        }
        let request = &self.request[..self.request_length];
        let complete = request.windows(4).any(|end| end == b"\r\n\r\n") || request.len() == REQUEST_MAX;
        if !complete {
            if socket.state() == State::CloseWait {
                // The client is done, without a whole request.
                socket.close();
                self.phase = Phase::Linger;
            } else if now_ms - self.since_ms > REQUEST_TIMEOUT_MS {
                socket.abort();
            }
            return false;
        }

        let line_end = request.windows(2).position(|end| end == b"\r\n").unwrap_or(request.len());
        let mut words = request[..line_end].split(|&byte| byte == b' ');
        let method = words.next().unwrap_or(&[]);
        let path = words.next().unwrap_or(&[]);
        match socket.remote_endpoint() {
            Some(peer) => println!("HTTP: {} {} on port {} from {}", Ascii(method), Ascii(path), self.port, peer),
            None => println!("HTTP: {} {} on port {}", Ascii(method), Ascii(path), self.port),
        }

        // A full buffer cannot take more: what does not fit is left out, and the page says less.
        match (method, path) {
            (b"GET", b"/") => {
                let mut body = Text::<512>::new();
                let _ = write_status(&mut body, status);
                let _ = self.write_head("200 OK", "text/plain; charset=utf-8", body.length as u32);
                let _ = self.head.write_str(core::str::from_utf8(body.as_bytes()).unwrap_or(""));
            }
            (b"GET", b"/data") => {
                let _ = self.write_head("200 OK", "application/octet-stream", DATA_BYTES);
                self.data_left = DATA_BYTES;
                self.data_total = DATA_BYTES;
            }
            (b"GET", _) => self.write_error("404 Not Found", "Not found. Try / or /data\n"),
            (_, b"") => self.write_error("400 Bad Request", "Bad request\n"),
            _ => self.write_error("405 Method Not Allowed", "Only GET\n"),
        }
        self.phase = Phase::Sending;
        self.answered_ms = now_ms;
        self.send(socket, now_ms);
        true
    }

    fn write_head(&mut self, status: &str, content_type: &str, length: u32) -> fmt::Result {
        write!(
            self.head,
            "HTTP/1.0 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status, content_type, length,
        )
    }

    fn write_error(&mut self, status: &str, body: &str) {
        let _ = self.write_head(status, "text/plain; charset=utf-8", body.len() as u32);
        let _ = self.head.write_str(body);
    }

    fn send(&mut self, socket: &mut tcp::Socket, now_ms: u64) {
        while socket.can_send() {
            let head = &self.head.as_bytes()[self.head_sent..];
            let count = if !head.is_empty() {
                socket.send_slice(head).unwrap_or(0)
            } else if self.data_left != 0 {
                let offset = (self.data_total - self.data_left) as usize;
                let left = self.data_left as usize;
                socket
                    .send(|buffer| {
                        let count = buffer.len().min(left);
                        for (i, byte) in buffer[..count].iter_mut().enumerate() {
                            *byte = LINE[(offset + i) % LINE.len()];
                        }
                        (count, count)
                    })
                    .unwrap_or(0)
            } else {
                break;
            };
            if count == 0 {
                break;
            }
            if !head.is_empty() {
                self.head_sent += count;
            } else {
                self.data_left -= count as u32;
            }
        }
        if self.head_sent == self.head.length && self.data_left == 0 {
            self.phase = Phase::Linger;
            self.since_ms = now_ms;
        }
    }

    fn linger(&mut self, socket: &mut tcp::Socket, now_ms: u64) -> Event {
        // Unread bytes would close the window, and hold the client's close behind them.
        let _ = socket.recv(|bytes| (bytes.len(), ()));
        let mut event = Event::Nothing;
        if !self.acknowledged && socket.send_queue() == 0 {
            self.acknowledged = true;
            self.since_ms = now_ms;
            if self.data_total != 0 {
                let ms = (now_ms - self.answered_ms).max(1);
                println!(
                    "HTTP: {} bytes acknowledged in {} ms, {} KiB/s",
                    self.data_total,
                    ms,
                    u64::from(self.data_total) * 1000 / 1024 / ms,
                );
                event = Event::DataAcknowledged;
            }
        }
        if socket.state() == State::CloseWait {
            socket.close();
        } else if now_ms - self.since_ms > LINGER_MS && socket.state() == State::Established {
            // The client has not closed: the wait after closing falls on this side then.
            socket.close();
        }
        event
    }
}

fn write_status(out: &mut impl Write, status: &Status) -> fmt::Result {
    let m = status.mac;
    writeln!(out, "BCM4334 WiFi test on the Exynos 4412, bare-metal Rust")?;
    writeln!(out, "Running for {} s", status.uptime_s)?;
    match status.address {
        Some(address) => writeln!(out, "Address {}", address)?,
        None => writeln!(out, "No address")?,
    }
    match status.router {
        Some(router) => writeln!(out, "Router {}", router)?,
        None => writeln!(out, "No router")?,
    }
    writeln!(out, "MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])?;
    writeln!(out, "Pings received: {}", status.pings)?;
    writeln!(out, "HTTP requests answered before this one: {}", status.requests)
}
