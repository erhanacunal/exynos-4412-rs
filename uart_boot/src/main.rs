//! A loader for the boot partition: waits for an application image over UART2 or, once it has
//! joined WiFi, over HTTP, checks it and starts it, so that a new build needs no fastboot. After
//! the application, a restart (its `r` key, or the reset button) comes back here.
//!
//! Images are `make bin` outputs, `<app>.bin`, sent by `tools/uartboot.py` from minicom
//! (Ctrl-A S), or by `tools/httpboot.py` (or `curl`) over WiFi: see [`net`]. On the UART wire,
//! from the host:
//!
//! | Bytes | Content |
//! |---|---|
//! | 8 | `EXYNBOOT` (run from RAM) or `EXYNSAVE` (save to CACHE, then run) |
//! | 4 | length, little-endian |
//! | 4 | CRC-32 (IEEE, as zlib's) of the image, little-endian |
//! | length | the image |
//!
//! and back, a line each: `uart_boot: receiving <length> bytes`, then `uart_boot: CRC OK` or
//! `uart_boot: ERROR ...`. A gap of [`GAP_MS`] in the image gives it up. After `EXYNSAVE` and
//! `CRC OK` comes `uart_boot: saved FERRITE/FERRITE.BIN` or `uart_boot: save ERROR ...`; the
//! image is started either way.
//!
//! # At power-on
//!
//! 1. For [`AUTOBOOT_MS`], the UART only, polled, with the MMU and caches off, as before WiFi.
//! 2. Built with WiFi (`WIFI_SSID`, `WIFI_PASSWORD`, `WIFI_FIRMWARE_DIR`: see `build.rs`), the
//!    chip joins the network and [`net`] serves HTTP and the UART together, for
//!    [`net::HTTP_WINDOW`] after it has an address.
//! 3. Then, if nothing has started arriving, the application saved on CACHE (p8, FAT32,
//!    `FERRITE/FERRITE.BIN`, see [`cache`]) is loaded, checked against the CRC saved with it and
//!    started. Without one, or with a bad one, this keeps waiting: a broken saved application
//!    never locks the board out.
//!
//! Holding volume up at power-on skips the saved application altogether, and so does test mode:
//! with `FERRITE/STAY` on CACHE, every image sent is run and nothing boots by itself. Test mode
//! is turned on with the key `s` or `POST /stay`, off with `a` or `POST /autoboot`; either
//! lasts across restarts. An HTTP connection, or `s`, also cancels the autoboot of this power-on
//! only. Without WiFi in the build, or when joining fails, step 2 is left out and the UART waits
//! on its own, polled.
//!
//! # Starting an image
//!
//! The image goes to [`STAGE`], above every application's link address, and runs from there
//! with the MMU and caches off, as `bootm` leaves them: its startup moves it down to its link
//! address, as from `bootm`'s 0x40009000. U-Boot's `r0`-`r2` are passed on. When WiFi has turned
//! the MMU on, [`start`] masks the interrupts, writes the image out to DRAM and turns the MMU and
//! the caches off first: the application finds the state the UART path leaves.
//!
//! Keys while waiting: `f` restart into fastboot, `r` restart, `s` test mode on, `a` off.

#![no_std]
#![no_main]

mod cache;
mod crc;
mod net;

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use exynos_4412_rs::drivers::gpio::{self, Pin, Pull};
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, println, soc};

exynos_4412_rs::entry!(main);

/// Where images go: 256 MiB into DRAM, far above the applications at `0x40008000` and far
/// below U-Boot at the top (hello_world/memory.x).
const STAGE: usize = 0x5000_0000;
/// The largest image taken.
const MAX_LEN: usize = 64 << 20;
/// Run the image from RAM.
const MAGIC_BOOT: &[u8; 8] = b"EXYNBOOT";
/// Save the image to CACHE as the application to boot, then run it.
const MAGIC_SAVE: &[u8; 8] = b"EXYNSAVE";
/// Every image starts with a zImage header: `startup.s`, `_start`.
const ZIMAGE_MAGIC_OFFSET: usize = 0x24;
const ZIMAGE_MAGIC: u32 = 0x016f_2818;
const GAP_MS: u64 = 2000;
/// How long the UART gets alone at power-on, before WiFi or the saved application.
const AUTOBOOT_MS: u64 = 3000;
/// Held at power-on (pressed = low): stay here, do not boot the saved application. Same pin as
/// `power::PowerKeys` (`exynos4412-mehmet.dts`, volume up).
const VOLUME_UP: Pin = gpio::GPX3.pin(4);
/// Time for the host's sender to exit and minicom to take the port back before the
/// application speaks.
const HANDOVER_MS: u64 = 300;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// Set: the saved application is not booted this power-on. Only loaded and stored, which work
/// with the MMU off.
static STAY: AtomicBool = AtomicBool::new(false);

/// Whether the saved application stays unbooted this power-on.
fn stay() -> bool {
    STAY.load(Ordering::Relaxed)
}

/// No autoboot this power-on.
fn hold() {
    STAY.store(true, Ordering::Relaxed);
}

/// Turns test mode on or off on CACHE; on also holds this power-on. Prints the outcome.
fn set_test_mode(on: bool) -> bool {
    if on {
        hold();
    }
    console::flush();
    match cache::set_stay(on) {
        Ok(()) if on => println!("uart_boot: test mode on ({}/{}): nothing boots by itself", cache::DIR, cache::STAY_FILE),
        Ok(()) => println!("uart_boot: test mode off: {}/{} boots from the next power-on", cache::DIR, cache::FILE),
        Err(error) => {
            println!("uart_boot: test mode ERROR {:?}", error);
            return false;
        }
    }
    true
}

/// What the host asked for with its magic.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Request {
    Boot,
    Save,
}

impl Request {
    /// The request whose magic ends `window`, the last 8 bytes received.
    fn from_magic(window: &[u8; 8]) -> Option<Self> {
        match window {
            MAGIC_BOOT => Some(Self::Boot),
            MAGIC_SAVE => Some(Self::Save),
            _ => None,
        }
    }
}

/// Acts on a key typed while waiting. Neither magic holds an `f`, `r`, `s` or `a`.
fn key(byte: u8) {
    match byte {
        b'f' => power::restart(BootMode::Fastboot),
        b'r' => power::restart(BootMode::Normal),
        b's' => {
            set_test_mode(true);
        }
        b'a' => {
            set_test_mode(false);
        }
        _ => {}
    }
}

fn wait_ms(mct: Mct, ms: u64) {
    let begin = mct.count();
    while mct.count() - begin < ms * COUNTS_PER_MS {}
}

fn byte_within_gap(mct: Mct) -> Option<u8> {
    let uart = console::uart();
    let begin = mct.count();
    loop {
        if let Some(byte) = uart.try_read_byte() {
            return Some(byte);
        }
        if mct.count() - begin > GAP_MS * COUNTS_PER_MS {
            return None;
        }
    }
}

fn word(mct: Mct) -> Option<u32> {
    let mut bytes = [0; 4];
    for byte in &mut bytes {
        *byte = byte_within_gap(mct)?;
    }
    Some(u32::from_le_bytes(bytes))
}

/// Waits for one of the magics, acting on keys meanwhile. With a `deadline_ms`, gives up after
/// that long without a whole magic and returns `None`.
fn wait_magic(mct: Mct, deadline_ms: Option<u64>) -> Option<Request> {
    let uart = console::uart();
    let begin = mct.count();
    // The last 8 bytes received, oldest first.
    let mut window = [0u8; 8];
    loop {
        let Some(byte) = uart.try_read_byte() else {
            if deadline_ms.is_some_and(|ms| mct.count() - begin > ms * COUNTS_PER_MS) {
                return None;
            }
            continue;
        };
        window.copy_within(1.., 0);
        window[7] = byte;
        if let Some(request) = Request::from_magic(&window) {
            return Some(request);
        }
        key(byte);
    }
}

/// Whether `len`, from a header, is a length this takes. Says why not.
fn length_ok(len: usize) -> bool {
    let ok = (ZIMAGE_MAGIC_OFFSET + 4..=MAX_LEN).contains(&len);
    if !ok {
        println!("uart_boot: ERROR length {} out of range", len);
    }
    ok
}

/// One image after its magic: its length and CRC if it arrived whole and right.
fn receive(mct: Mct) -> Result<(usize, u32), &'static str> {
    let len = word(mct).ok_or("no length")? as usize;
    let want = word(mct).ok_or("no CRC")?;
    if !length_ok(len) {
        return Err("length");
    }
    println!("uart_boot: receiving {} bytes", len);
    console::flush();
    let mut crc = crc::START;
    for offset in 0..len {
        let Some(byte) = byte_within_gap(mct) else {
            println!("uart_boot: ERROR timeout after {} of {} bytes", offset, len);
            return Err("timeout");
        };
        // SAFETY: STAGE..STAGE + MAX_LEN is DRAM that nothing here uses: this program is at
        // the bottom of DRAM, U-Boot at the top.
        unsafe { ((STAGE + offset) as *mut u8).write_volatile(byte) };
        crc = crc::update(crc, &[byte]);
    }
    check(len, crc::finish(crc), want)?;
    Ok((len, want))
}

/// Checks the `len` bytes at [`STAGE`], whose CRC is `got`, against the `want` that came with
/// them, and for the zImage magic. Prints the verdict.
fn check(len: usize, got: u32, want: u32) -> Result<(), &'static str> {
    if got != want {
        println!("uart_boot: ERROR CRC {:#010x}, sent {:#010x}", got, want);
        return Err("CRC");
    }
    if len < ZIMAGE_MAGIC_OFFSET + 4 || !has_zimage_magic() {
        return Err("magic");
    }
    println!("uart_boot: CRC OK");
    Ok(())
}

fn has_zimage_magic() -> bool {
    // SAFETY: inside the image just placed at STAGE, which is longer than this word's end.
    let magic = unsafe { ((STAGE + ZIMAGE_MAGIC_OFFSET) as *const u32).read_volatile() };
    if magic != ZIMAGE_MAGIC {
        println!("uart_boot: ERROR no zImage magic at {:#x}: {:#010x}; send a .bin", ZIMAGE_MAGIC_OFFSET, magic);
    }
    magic == ZIMAGE_MAGIC
}

/// The `len` bytes at [`STAGE`].
///
/// Callers take turns: the polled path has one thread, and in [`net`] only the holder of its
/// claim on the stage calls this.
fn staged(len: usize) -> &'static mut [u8] {
    // SAFETY: STAGE..STAGE + MAX_LEN is DRAM that only the image being loaded uses (see
    // `receive`), and `len` <= MAX_LEN. One user at a time, as above.
    unsafe { core::slice::from_raw_parts_mut(STAGE as *mut u8, len.min(MAX_LEN)) }
}

/// Saves the `len` bytes at [`STAGE`], whose CRC is `crc`, as the application to boot. Prints
/// the outcome; the image is started either way.
fn save(len: usize, crc: u32) -> bool {
    console::flush();
    match cache::save(staged(len), crc) {
        Ok(()) => {
            println!("uart_boot: saved {}/{}", cache::DIR, cache::FILE);
            true
        }
        Err(error) => {
            println!("uart_boot: save ERROR {:?}", error);
            false
        }
    }
}

/// Loads the saved application to [`STAGE`]; its length, or `None` (reason printed).
fn load_saved() -> Option<usize> {
    match cache::load(staged(MAX_LEN)) {
        Ok(len) if has_zimage_magic() => {
            println!("uart_boot: {}/{} CRC OK, {} bytes", cache::DIR, cache::FILE, len);
            Some(len)
        }
        Ok(_) => None,
        Err(cache::Error::NotFound) => {
            println!("uart_boot: no {}/{} on CACHE", cache::DIR, cache::FILE);
            None
        }
        Err(error) => {
            println!("uart_boot: {}/{} not booted: {:?}", cache::DIR, cache::FILE, error);
            None
        }
    }
}

/// Runs the image at [`STAGE`], `len` bytes, with U-Boot's registers.
fn start(mct: Mct, len: usize, args: &BootArgs) -> ! {
    console::flush();
    wait_ms(mct, HANDOVER_MS);
    println!("uart_boot: starting {} bytes at {:#x}", len, STAGE);
    console::flush();
    if mmu::is_enabled() {
        leave_mmu(len, args);
    }
    // SAFETY: the MMU and caches have stayed off since startup, so the image is in DRAM as
    // written; the barriers order it before the jump, and the instruction cache and branch
    // predictor hold nothing of it. STAGE holds a checked image whose first instruction is
    // ARM code, and its startup expects exactly this state.
    unsafe {
        asm!(
            "dsb",
            "mcr p15, 0, {zero}, c7, c5, 0", // ICIALLU
            "mcr p15, 0, {zero}, c7, c5, 6", // BPIALL
            "dsb",
            "isb",
            "bx {entry}",
            zero = in(reg) 0,
            entry = in(reg) STAGE,
            in("r0") args.r0,
            in("r1") args.r1,
            in("r2") args.r2,
            options(noreturn),
        )
    }
}

/// [`start`] after WiFi: leaves the state `net` set up, then the MMU and caches, and jumps.
///
/// Interrupts are masked for good, and the sources `net` turned on are turned off where they
/// start: the MCT's comparator 0 (Embassy's time) and the console UART's interrupts. Each
/// application's `Gic::init` clears what is left at the GIC. The WiFi chip is left running: an
/// application that uses it powers it up from reset.
///
/// The L2 cache is not enabled by this program, but may be by U-Boot: the image's range is
/// cleaned through both levels, so the image is in DRAM. Then, in one block that touches no
/// memory, the MMU and caches go off and the whole L1 data cache is cleaned and invalidated by set
/// and way (`cache-v7.S`, `v7_flush_dcache_all`): no dirty line stays to be written back over the
/// application later. The stack's own lines may then sit in an enabled L2 cache, which the
/// application's `mmu::enable` discards; with the MMU off nothing reaches the L2 cache.
fn leave_mmu(len: usize, args: &BootArgs) -> ! {
    cpu::disable_irq();
    Mct::new().stop_periodic();
    console::uart().mask_interrupts();
    mmu::clean_dcache_range(STAGE, len);
    // SAFETY: identity mapping, so execution carries on at the same addresses with the MMU off.
    // From the SCTLR write on, the block uses registers only, so no stale value can be read
    // from DRAM; it never returns, so every register is free. STAGE holds a checked image, now
    // in DRAM, and the jump is as in `start`.
    unsafe {
        asm!(
            "cpsid if",
            "mrc p15, 0, r4, c1, c0, 0",  // SCTLR
            "bic r4, r4, #(1 << 0)",      // M
            "bic r4, r4, #(1 << 2)",      // C
            "bic r4, r4, #(1 << 11)",     // Z
            "bic r4, r4, #(1 << 12)",     // I
            "mcr p15, 0, r4, c1, c0, 0",
            "isb",
            "mov r4, #0",
            "mcr p15, 2, r4, c0, c0, 0",  // CSSELR: L1 data
            "isb",
            "mrc p15, 1, r4, c0, c0, 0",  // CCSIDR
            "and r5, r4, #7",
            "add r5, r5, #4",             // r5 = set shift
            "ubfx r8, r4, #3, #10",       // r8 = way, counting down
            "ubfx r10, r4, #13, #15",     // r10 = highest set
            "clz r12, r8",                // r12 = way shift
            "1:",
            "mov lr, r10",
            "2:",
            "lsl r4, r8, r12",
            "orr r4, r4, lr, lsl r5",
            "mcr p15, 0, r4, c7, c14, 2", // DCCISW
            "subs lr, lr, #1",
            "bpl 2b",
            "subs r8, r8, #1",
            "bpl 1b",
            "dsb",
            "mov r4, #0",
            "mcr p15, 0, r4, c7, c5, 0",  // ICIALLU
            "mcr p15, 0, r4, c7, c5, 6",  // BPIALL
            "dsb",
            "isb",
            "bx r3",
            in("r0") args.r0,
            in("r1") args.r1,
            in("r2") args.r2,
            in("r3") STAGE,
            options(noreturn),
        )
    }
}

fn main(args: BootArgs) -> ! {
    let mct = Mct::new();
    mct.start();
    VOLUME_UP.set_input(Pull::Up);
    wait_ms(mct, 5); // the pull-up settles
    let held = !VOLUME_UP.read();
    let wifi = net::configured();
    println!("\nuart_boot: send a .bin (minicom Ctrl-A S, uartboot) to {:#x}; keys f r s a", STAGE);
    if held {
        hold();
        println!("uart_boot: volume up held: not booting {}/{}", cache::DIR, cache::FILE);
    } else if matches!(cache::stay(), Ok(true)) {
        hold();
        println!("uart_boot: test mode ({}/{}): not booting {}; key a ends it", cache::DIR, cache::STAY_FILE, cache::FILE);
    }
    if wifi {
        let then = if stay() { "" } else { ", then FERRITE/FERRITE.BIN" };
        println!("uart_boot: WiFi in {} ms, then HTTP{}", AUTOBOOT_MS, then);
    } else if !stay() {
        println!("uart_boot: booting {}/{} in {} ms unless an image arrives", cache::DIR, cache::FILE, AUTOBOOT_MS);
    }
    // The UART alone, for this long; `None`: for good.
    let mut window = (wifi || !stay()).then_some(AUTOBOOT_MS);
    loop {
        let Some(request) = wait_magic(mct, window) else {
            window = None;
            if wifi {
                // Returns only if WiFi did not come up.
                net::run(mct, args);
                println!("uart_boot: no WiFi, UART only");
            }
            if !stay() && let Some(len) = load_saved() {
                start(mct, len, &args);
            }
            println!("uart_boot: waiting for an image");
            console::flush();
            continue;
        };
        // An image is coming: no WiFi or autoboot after it either, even if it fails.
        window = None;
        match receive(mct) {
            Ok((len, crc)) => {
                if request == Request::Save {
                    save(len, crc);
                }
                start(mct, len, &args);
            }
            Err(_) => {
                println!("uart_boot: waiting again");
                console::flush();
            }
        }
    }
}
