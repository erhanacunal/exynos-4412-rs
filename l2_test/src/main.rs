//! L2 cache test. Checks the MCT rate against the UART, measures memory speed at three
//! working set sizes with the L2 cache off and on, checks that data written before the
//! switch survives it, then keeps verifying memory under a 1 Hz timer interrupt.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::uart::Uart;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

const WORDS_L1: usize = 16 * 1024 / 4;
const WORDS_L2: usize = 256 * 1024 / 4;
/// Twice the L2 cache size.
const WORDS_BEYOND: usize = 2 * 1024 * 1024 / 4;

/// Word accesses per measurement, so every working set does the same amount of work.
const ACCESSES: usize = 8 * 1024 * 1024;

/// 8N1 at 115200 baud is 11520 characters per second, so 1152 take 100 ms.
const UART_CHARS_PER_SECOND: u64 = 11_520;
const UART_LINES: u64 = 16;
const UART_LINE_CHARS: u64 = 72;

static BUFFER: [AtomicU32; WORDS_BEYOND] = [const { AtomicU32::new(0) }; WORDS_BEYOND];
static TICKS: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };
    if irq.id() == soc::MCT_G0_IRQ {
        Mct::new().clear_interrupt();
        TICKS.fetch_add(1, Ordering::Relaxed);
    } else {
        println!("beklenmeyen kesme: {}", irq.id());
    }
    gic.end_of_interrupt(irq);
}

/// Times a known amount of UART output against the MCT. The terminal being readable means
/// the baud rate is right to within a few percent, which bounds the MCT rate as closely.
fn measure_mct_against_uart(mct: Mct, uart: Uart) -> u64 {
    uart.flush();
    let start = mct.count();
    for _ in 0..UART_LINES {
        for _ in 0..UART_LINE_CHARS - 2 {
            uart.write_byte(b'-');
        }
        uart.write_byte(b'\r');
        uart.write_byte(b'\n');
    }
    uart.flush();
    let counts = mct.count() - start;
    counts * UART_CHARS_PER_SECOND / (UART_LINES * UART_LINE_CHARS)
}

fn fill(words: usize, seed: u32) {
    for (index, word) in BUFFER[..words].iter().enumerate() {
        word.store(seed ^ (index as u32).wrapping_mul(0x9E37_79B1), Ordering::Relaxed);
    }
}

fn verify(words: usize, seed: u32) {
    for (index, word) in BUFFER[..words].iter().enumerate() {
        let expected = seed ^ (index as u32).wrapping_mul(0x9E37_79B1);
        let actual = word.load(Ordering::Relaxed);
        if actual != expected {
            panic!("bellek hatasi: indis {} beklenen {:#x} okunan {:#x}", index, expected, actual);
        }
    }
}

/// Nanoseconds per word for repeated write and verify passes over `words` words.
fn benchmark(mct: Mct, words: usize) -> u64 {
    let passes = (ACCESSES / (2 * words)).max(1);
    let start = mct.count();
    for pass in 0..passes {
        let seed = pass as u32;
        fill(words, seed);
        verify(words, seed);
    }
    let counts = mct.count() - start;
    let accesses = (2 * words * passes) as u64;
    counts * 1000 / u64::from(soc::MCT_CLOCK_HZ / 1_000_000) / accesses
}

fn report_speed(mct: Mct, label: &str) {
    println!(
        "{}: 16 KiB {} ns, 256 KiB {} ns, 2 MiB {} ns",
        label,
        benchmark(mct, WORDS_L1),
        benchmark(mct, WORDS_L2),
        benchmark(mct, WORDS_BEYOND),
    );
}

fn report_l2(l2c: L2c) {
    println!(
        "L2: CTRL {:#x} AUX {:#010x} TAG {:#x} DATA {:#x} PREFETCH {:#010x} POWER {:#x}",
        l2c.ctrl(),
        l2c.aux_ctrl(),
        l2c.tag_latency(),
        l2c.data_latency(),
        l2c.prefetch_ctrl(),
        l2c.power_ctrl(),
    );
}

fn main(_args: BootArgs) -> ! {
    println!("\nL2 testi");

    // SAFETY: the chip ID block is always readable, and reading it has no side effects.
    let chip_id = unsafe { core::ptr::read_volatile(soc::CHIPID_BASE as *const u32) };
    println!("Chip ID: {:#010x}", chip_id);

    let mct = Mct::new();
    mct.start();
    let uart = console::uart();
    let l2c = L2c::new();

    mmu::enable();
    report_l2(l2c);

    let rate = measure_mct_against_uart(mct, uart);
    println!("MCT hizi (UART ile olculen): {} Hz, beklenen {}", rate, soc::MCT_CLOCK_HZ);

    report_speed(mct, "L2 kapali");

    // Dirty in L1 and DRAM at the moment of the switch; must read back unchanged after it.
    fill(WORDS_L2, 0xA5A5_5A5A);

    println!("L2 aciliyor...");
    uart.flush();
    l2c.enable();
    println!("L2 acildi");
    report_l2(l2c);

    verify(WORDS_L2, 0xA5A5_5A5A);
    println!("L2 acilmadan yazilan veri dogru");

    report_speed(mct, "L2 acik  ");

    let gic = Gic::new();
    gic.init();
    gic.enable(soc::MCT_G0_IRQ);
    mct.start_periodic(soc::MCT_CLOCK_HZ);
    cpu::enable_irq();

    let mut printed = 0;
    let mut seed = 0u32;
    loop {
        seed = seed.wrapping_add(1);
        fill(WORDS_BEYOND, seed);
        verify(WORDS_BEYOND, seed);

        let ticks = TICKS.load(Ordering::Relaxed);
        if ticks != printed {
            printed = ticks;
            println!("tick {}  2 MiB dogrulama gecisi: {}, hata yok", ticks, seed);
        }
        match uart.try_read_byte() {
            Some(b'\r') => println!(),
            Some(byte) => print!("{}", byte as char),
            None => {}
        }
    }
}
