//! A loader for the boot partition: waits for an application image over UART2, checks it and
//! starts it, so that a new build needs no fastboot. After the application, a restart (its `r`
//! key, or the reset button) comes back here.
//!
//! Images are `make bin` outputs, `<app>.bin`, sent by `tools/uartboot.py` from minicom
//! (Ctrl-A S). On the wire, from the host:
//!
//! | Bytes | Content |
//! |---|---|
//! | 8 | `EXYNBOOT` |
//! | 4 | length, little-endian |
//! | 4 | CRC-32 (IEEE, as zlib's) of the image, little-endian |
//! | length | the image |
//!
//! and back, a line each: `uart_boot: receiving <length> bytes`, then `uart_boot: CRC OK` or
//! `uart_boot: ERROR ...`. A gap of [`GAP_MS`] in the image gives it up.
//!
//! The image goes to [`STAGE`], above every application's link address, and runs from there
//! with the MMU and caches off, as `bootm` leaves them: its startup moves it down to its link
//! address, as from `bootm`'s 0x40009000. U-Boot's `r0`-`r2` are passed on.
//!
//! Keys while waiting: `f` restart into fastboot, `r` restart.

#![no_std]
#![no_main]

use core::arch::asm;

use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, println, soc};

exynos_4412_rs::entry!(main);

/// Where images go: 256 MiB into DRAM, far above the applications at `0x40008000` and far
/// below U-Boot at the top (hello_world/memory.x).
const STAGE: usize = 0x5000_0000;
/// The largest image taken.
const MAX_LEN: usize = 64 << 20;
const MAGIC: &[u8; 8] = b"EXYNBOOT";
/// Every image starts with a zImage header: `startup.s`, `_start`.
const ZIMAGE_MAGIC_OFFSET: usize = 0x24;
const ZIMAGE_MAGIC: u32 = 0x016f_2818;
const GAP_MS: u64 = 2000;
/// Time for the host's sender to exit and minicom to take the port back before the
/// application speaks.
const HANDOVER_MS: u64 = 300;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// CRC-32 as zlib computes it, a bit at a time: 115200 baud leaves time for it.
fn crc32_update(mut crc: u32, byte: u8) -> u32 {
    crc ^= u32::from(byte);
    for _ in 0..8 {
        crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
    }
    crc
}

/// The next byte, unless none comes for [`GAP_MS`].
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

/// Waits for [`MAGIC`], acting on keys meanwhile.
fn wait_magic() {
    let uart = console::uart();
    let mut matched = 0;
    loop {
        let byte = uart.read_byte();
        if byte == MAGIC[matched] {
            matched += 1;
            if matched == MAGIC.len() {
                return;
            }
            continue;
        }
        matched = usize::from(byte == MAGIC[0]);
        match byte {
            b'f' => power::restart(BootMode::Fastboot),
            b'r' => power::restart(BootMode::Normal),
            _ => {}
        }
    }
}

/// One image after its magic: its length if it arrived whole and right.
fn receive(mct: Mct) -> Result<usize, &'static str> {
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
    // SAFETY: inside the image just written, `len` being past this word.
    let magic = unsafe { ((STAGE + ZIMAGE_MAGIC_OFFSET) as *const u32).read_volatile() };
    if magic != ZIMAGE_MAGIC {
        println!("uart_boot: ERROR no zImage magic at {:#x}: {:#010x}; send a .bin", ZIMAGE_MAGIC_OFFSET, magic);
        return Err("magic");
    }
    println!("uart_boot: CRC OK");
    Ok(len)
}

/// Runs the image at [`STAGE`] with U-Boot's registers.
fn start(args: &BootArgs) -> ! {
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
    println!("\nuart_boot: send a .bin (minicom Ctrl-A S, uartboot) to {:#x}; keys f fastboot, r restart", STAGE);
    loop {
        wait_magic();
        match receive(mct) {
            Ok(len) => {
                console::flush();
                let begin = mct.count();
                while mct.count() - begin < HANDOVER_MS * COUNTS_PER_MS {}
                println!("uart_boot: starting {} bytes at {:#x}", len, STAGE);
                console::flush();
                start(&args);
            }
            Err(_) => {
                println!("uart_boot: waiting again");
                console::flush();
            }
        }
    }
}
