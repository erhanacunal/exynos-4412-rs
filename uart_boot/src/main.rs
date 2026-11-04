//! A loader for the boot partition: waits for an application image over UART2, checks it and
//! starts it, so that a new build needs no fastboot. After the application, a restart (its `r`
//! key, or the reset button) comes back here.
//!
//! Images are `make bin` outputs, `<app>.bin`, sent by `tools/uartboot.py` from minicom
//! (Ctrl-A S). On the wire, from the host:
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
//! # Booting the saved application
//!
//! When no image starts arriving within [`AUTOBOOT_MS`] of power-on, the one saved on CACHE
//! (p8, FAT32, `FERRITE/FERRITE.BIN`, see [`cache`]) is loaded, checked against the CRC saved
//! with it and started. Without one, or with a bad one, this keeps waiting for the UART: a
//! broken saved application never locks the board out. Holding volume up at power-on skips the
//! saved application altogether.
//!
//! The image goes to [`STAGE`], above every application's link address, and runs from there
//! with the MMU and caches off, as `bootm` leaves them: its startup moves it down to its link
//! address, as from `bootm`'s 0x40009000. U-Boot's `r0`-`r2` are passed on.
//!
//! Keys while waiting: `f` restart into fastboot, `r` restart.

#![no_std]
#![no_main]

mod cache;

use core::arch::asm;

use exynos_4412_rs::drivers::gpio::{self, Pin, Pull};
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, println, soc};

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
/// How long the UART gets at power-on before the saved application boots.
const AUTOBOOT_MS: u64 = 3000;
/// Held at power-on (pressed = low): stay here, do not boot the saved application. Same pin as
/// `power::PowerKeys` (`exynos4412-mehmet.dts`, volume up).
const VOLUME_UP: Pin = gpio::GPX3.pin(4);
/// Time for the host's sender to exit and minicom to take the port back before the
/// application speaks.
const HANDOVER_MS: u64 = 300;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// What the host asked for with its magic.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Request {
    Boot,
    Save,
}

/// CRC-32 as zlib computes it, a bit at a time: 115200 baud leaves time for it.
fn crc32_update(mut crc: u32, byte: u8) -> u32 {
    crc ^= u32::from(byte);
    for _ in 0..8 {
        crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
    }
    crc
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
        if &window == MAGIC_BOOT {
            return Some(Request::Boot);
        }
        if &window == MAGIC_SAVE {
            return Some(Request::Save);
        }
        // Neither magic holds an `f` or an `r`.
        match byte {
            b'f' => power::restart(BootMode::Fastboot),
            b'r' => power::restart(BootMode::Normal),
            _ => {}
        }
    }
}

/// One image after its magic: its length and CRC if it arrived whole and right.
fn receive(mct: Mct) -> Result<(usize, u32), &'static str> {
    let len = word(mct).ok_or("no length")? as usize;
    let want = word(mct).ok_or("no CRC")?;
    if !(ZIMAGE_MAGIC_OFFSET + 4..=MAX_LEN).contains(&len) {
        println!("uart_boot: ERROR length {} out of range", len);
        return Err("length");
    }
    println!("uart_boot: receiving {} bytes", len);
    console::flush();
    let mut crc = !0;
    for offset in 0..len {
        let Some(byte) = byte_within_gap(mct) else {
            println!("uart_boot: ERROR timeout after {} of {} bytes", offset, len);
            return Err("timeout");
        };
        // SAFETY: STAGE..STAGE + MAX_LEN is DRAM that nothing here uses: this program is at
        // the bottom of DRAM, U-Boot at the top.
        unsafe { ((STAGE + offset) as *mut u8).write_volatile(byte) };
        crc = crc32_update(crc, byte);
    }
    let crc = !crc;
    if crc != want {
        println!("uart_boot: ERROR CRC {:#010x}, sent {:#010x}", crc, want);
        return Err("CRC");
    }
    if !has_zimage_magic() {
        return Err("magic");
    }
    println!("uart_boot: CRC OK");
    Ok((len, crc))
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
fn staged(len: usize) -> &'static mut [u8] {
    // SAFETY: STAGE..STAGE + MAX_LEN is DRAM that only the image being loaded uses (see
    // `receive`), and `len` <= MAX_LEN.
    unsafe { core::slice::from_raw_parts_mut(STAGE as *mut u8, len.min(MAX_LEN)) }
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

/// Runs the image at [`STAGE`] with U-Boot's registers.
fn start(mct: Mct, len: usize, args: &BootArgs) -> ! {
    console::flush();
    wait_ms(mct, HANDOVER_MS);
    println!("uart_boot: starting {} bytes at {:#x}", len, STAGE);
    console::flush();
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

fn main(args: BootArgs) -> ! {
    let mct = Mct::new();
    mct.start();
    VOLUME_UP.set_input(Pull::Up);
    wait_ms(mct, 5); // the pull-up settles
    let stay = !VOLUME_UP.read();
    println!("\nuart_boot: send a .bin (minicom Ctrl-A S, uartboot) to {:#x}; keys f fastboot, r restart", STAGE);
    let mut autoboot = if stay {
        println!("uart_boot: volume up held: not booting {}/{}", cache::DIR, cache::FILE);
        None
    } else {
        println!("uart_boot: booting {}/{} in {} ms unless an image arrives", cache::DIR, cache::FILE, AUTOBOOT_MS);
        Some(AUTOBOOT_MS)
    };
    loop {
        let Some(request) = wait_magic(mct, autoboot) else {
            // No image within the window: the saved application, once.
            autoboot = None;
            if let Some(len) = load_saved() {
                start(mct, len, &args);
            }
            println!("uart_boot: waiting for an image");
            console::flush();
            continue;
        };
        // An image is coming: no autoboot after it either, even if it fails.
        autoboot = None;
        match receive(mct) {
            Ok((len, crc)) => {
                if request == Request::Save {
                    console::flush();
                    match cache::save(staged(len), crc) {
                        Ok(()) => println!("uart_boot: saved {}/{}", cache::DIR, cache::FILE),
                        Err(error) => println!("uart_boot: save ERROR {:?}", error),
                    }
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
