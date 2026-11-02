//! MMU and cache test. Reports the security state and L2 cache state, compares memory
//! speed before and after `mmu::enable`, tries exclusive atomics, then keeps a 1 Hz
//! timer interrupt running while checking that it preserves floating-point registers.
//!
//! Keys: `t` reads above DRAM (translation fault), `x` executes from a peripheral
//! (prefetch abort), anything else echoes.

#![no_std]
#![no_main]

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::exceptions::ExceptionFrame;
use exynos_4412_rs::{BootArgs, console, cpu, mmu, print, println, soc};

exynos_4412_rs::entry!(main);

/// First address above DRAM, unmapped once the MMU is on.
const UNMAPPED: usize = soc::DRAM_BASE + soc::DRAM_SIZE;

static PROBING: AtomicBool = AtomicBool::new(false);
static PROBE_TRAPPED: AtomicBool = AtomicBool::new(false);

static TICKS: AtomicU32 = AtomicU32::new(0);

static BUFFER: [AtomicU32; 4096] = [const { AtomicU32::new(0) }; 4096];

// Read-modify-write atomics, tried only after `mmu::enable`.
static COUNTER_32: AtomicU32 = AtomicU32::new(0);
static COUNTER_64: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_undef(frame: &mut ExceptionFrame) {
    if !PROBING.load(Ordering::Relaxed) {
        panic!("unexpected undefined instruction, return {:#x}", frame.return_addr);
    }
    PROBE_TRAPPED.store(true, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
extern "C" fn handle_irq(_frame: &mut ExceptionFrame) {
    let gic = Gic::new();
    let Some(irq) = gic.acknowledge() else {
        return;
    };

    if irq.id() == soc::MCT_G0_IRQ {
        Mct::new().clear_interrupt();
        // SAFETY: only overwrites the two declared registers. Interrupted code must
        // not see this: the exception stub restores them.
        unsafe {
            asm!(
                "vmov.f64 d0, #3.0",
                "vmov.f64 d17, #3.0",
                out("d0") _,
                out("d17") _,
                options(nomem, nostack, preserves_flags),
            );
        }
        TICKS.fetch_add(1, Ordering::Relaxed);
    } else {
        println!("unexpected interrupt: {}", irq.id());
    }

    gic.end_of_interrupt(irq);
}

/// Reading SCR is undefined from the Non-secure state, so the trap tells the two apart.
fn runs_non_secure() -> bool {
    PROBING.store(true, Ordering::Relaxed);
    // SAFETY: at PL1 this either reads SCR or traps to `handle_undef`, which returns to
    // the next instruction. The result is unused and all else is preserved.
    unsafe {
        asm!("mrc p15, 0, {}, c1, c1, 0", out(reg) _, options(nomem, nostack, preserves_flags));
    }
    PROBING.store(false, Ordering::Relaxed);
    PROBE_TRAPPED.load(Ordering::Relaxed)
}

fn read_actlr() -> u32 {
    let value: u32;
    // SAFETY: reading ACTLR at PL1 has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c1, c0, 1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn read_nsacr() -> u32 {
    let value: u32;
    // SAFETY: NSACR is readable at PL1 in both security states, without side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c1, c1, 2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Microseconds for 64 write and read passes over 16 KiB, which fits the L1 cache.
fn memory_benchmark(mct: Mct) -> u64 {
    let start = mct.count();
    for pass in 0..64 {
        for word in &BUFFER {
            word.store(pass, Ordering::Relaxed);
        }
        for word in &BUFFER {
            if word.load(Ordering::Relaxed) != pass {
                panic!("memory verification failed");
            }
        }
    }
    (mct.count() - start) / u64::from(soc::MCT_CLOCK_HZ / 1_000_000)
}

/// Sets `d0` and `d17`, spins so a timer interrupt can land, and checks they survived.
fn fp_registers_survive() -> bool {
    let (d0_high, d17_high): (u32, u32);
    // SAFETY: only touches the declared registers. `vmov` from a d register writes a
    // pair of core registers; the low words of 1.0 and 2.0 are zero and go unused.
    unsafe {
        asm!(
            "vmov.f64 d0, #1.0",
            "vmov.f64 d17, #2.0",
            "2:",
            "subs {spins}, {spins}, #1",
            "bne 2b",
            "vmov {low}, {d0_high}, d0",
            "vmov {low}, {d17_high}, d17",
            spins = inout(reg) 2_000_000u32 => _,
            low = out(reg) _,
            d0_high = out(reg) d0_high,
            d17_high = out(reg) d17_high,
            out("d0") _,
            out("d17") _,
            options(nomem, nostack),
        );
    }
    d0_high == 0x3FF0_0000 && d17_high == 0x4000_0000
}

fn main(_args: BootArgs) -> ! {
    println!("\nMMU test");

    let non_secure = runs_non_secure();
    println!("Security state: {}", if non_secure { "Non-secure" } else { "Secure" });
    println!("ACTLR {:#010x}  NSACR {:#010x}", read_actlr(), read_nsacr());

    let l2c = L2c::new();
    println!(
        "L2: ID {:#010x}  CTRL {:#x}  AUX {:#010x}  ({})",
        l2c.cache_id(),
        l2c.ctrl(),
        l2c.aux_ctrl(),
        if l2c.is_enabled() { "on" } else { "off" },
    );

    let mct = Mct::new();
    mct.start();

    println!("SCTLR before: {:#010x}", cpu::read_sctlr());
    let uncached = memory_benchmark(mct);
    println!("Uncached: {} us", uncached);

    mmu::enable();

    println!("SCTLR after:  {:#010x}", cpu::read_sctlr());
    let cached = memory_benchmark(mct);
    println!("Cached:   {} us  ({}x)", cached, uncached / cached.max(1));

    COUNTER_32.fetch_add(5, Ordering::Relaxed);
    COUNTER_64.fetch_add(7, Ordering::Relaxed);
    println!(
        "LDREX/STREX: {} {}",
        COUNTER_32.load(Ordering::Relaxed),
        COUNTER_64.load(Ordering::Relaxed),
    );

    let gic = Gic::new();
    gic.init();
    gic.enable(soc::MCT_G0_IRQ);
    mct.start_periodic(soc::MCT_CLOCK_HZ);
    cpu::enable_irq();
    println!("t: translation fault, x: prefetch abort, others: echo");

    let uart = console::uart();
    let mut printed = 0;
    let mut interrupted_checks = 0u32;
    loop {
        let before = TICKS.load(Ordering::Relaxed);
        if !fp_registers_survive() {
            panic!("interrupt corrupted FP registers");
        }
        let ticks = TICKS.load(Ordering::Relaxed);
        if ticks != before {
            interrupted_checks += 1;
        }
        if ticks != printed {
            printed = ticks;
            println!("tick {}  interrupted FP checks: {}, no errors", ticks, interrupted_checks);
        }

        match uart.try_read_byte() {
            Some(b't') => {
                // SAFETY: the address is unmapped; the load is meant to fault.
                let value = unsafe { core::ptr::read_volatile(UNMAPPED as *const u32) };
                println!("no fault occurred: {:#x}", value);
            }
            Some(b'x') => {
                // SAFETY: the target is execute-never Device memory; the branch is meant
                // to fault, and the default handler does not return.
                unsafe {
                    asm!("blx {}", in(reg) soc::MCT_BASE, clobber_abi("C"));
                }
            }
            Some(b'\r') => println!(),
            Some(byte) => print!("{}", byte as char),
            None => {}
        }
    }
}
