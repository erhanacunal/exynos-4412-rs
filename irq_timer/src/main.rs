//! Interrupt test: a 1 Hz MCT interrupt drives a counter that the main loop prints,
//! while the main loop keeps echoing UART input. Interrupted work resuming correctly
//! is what shows the IRQ return path works.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, print, println, soc};

exynos_4412_rs::entry!(main);

// Only `handle_irq` writes these, and only load and store are used: no LDREX with the MMU
// off. 32 bits on purpose: `AtomicU64` load and store compile to LDREXD/STREXD on this CPU.
// The counter's low word suffices, as a delta of one period fits and wraps correctly.
static TICKS: AtomicU32 = AtomicU32::new(0);
static LAST_COUNT: AtomicU32 = AtomicU32::new(0);
static LAST_DELTA: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };

    if irq.id() == soc::MCT_G0_IRQ {
        let mct = Mct::new();
        mct.clear_interrupt();
        let count = mct.count() as u32;
        LAST_DELTA.store(count.wrapping_sub(LAST_COUNT.load(Ordering::Relaxed)), Ordering::Relaxed);
        LAST_COUNT.store(count, Ordering::Relaxed);
        TICKS.store(TICKS.load(Ordering::Relaxed) + 1, Ordering::Release);
    } else {
        println!("unexpected interrupt: {}", irq.id());
    }

    gic.end_of_interrupt(irq);
}

fn main(_args: BootArgs) -> ! {
    println!("\nGIC + MCT test");

    let mct = Mct::new();
    mct.start();
    let before = mct.count();
    let after = (0..1000).fold(before, |_, _| mct.count());
    if after == before {
        panic!("MCT counter not advancing: {:#x}", before);
    }
    println!("MCT counter advancing: {:#x} -> {:#x}", before, after);

    let gic = Gic::new();
    gic.init();
    println!("GIC ready");

    LAST_COUNT.store(mct.count() as u32, Ordering::Relaxed);
    gic.enable(soc::MCT_G0_IRQ);
    mct.start_periodic(soc::MCT_CLOCK_HZ);
    cpu::enable_irq();

    let uart = console::uart();
    let mut printed = 0;
    loop {
        let ticks = TICKS.load(Ordering::Acquire);
        if ticks != printed {
            printed = ticks;
            println!("tick {}  counter delta {}", ticks, LAST_DELTA.load(Ordering::Relaxed));
        }
        match uart.try_read_byte() {
            Some(b'\r') => println!(),
            Some(byte) => print!("{}", byte as char),
            None => {}
        }
    }
}
