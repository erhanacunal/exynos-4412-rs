//! Starting CPU1-3, and the lock they share memory with.
//!
//! The order of operations on CPU0, whose reasons are in each item's documentation:
//!
//! 1. [`mmu::enable`](crate::mmu::enable), which also enables the SCU;
//! 2. optionally [`L2c::enable`](crate::drivers::l2c::L2c::enable);
//! 3. [`Gic::init`](crate::drivers::gic::Gic::init), since waking a core takes an SGI;
//! 4. [`start_cpu`] for each core.
//!
//! A started core runs the given function in SVC mode with IRQ and FIQ masked, its own
//! stacks (`link.x`: `__stacks_per_cpu`), VFP on, and the MMU and L1 caches on. It shares
//! all memory coherently with CPU0, and has to call
//! [`Gic::init_this_cpu`](crate::drivers::gic::Gic::init_this_cpu) before it takes
//! interrupts. The vector table and exception handlers are common to all cores:
//! [`cpu::id`] tells a handler where it runs.
//!
//! # How a core is started
//!
//! The firmware holds a Non-secure core in its own pen. Only the interface is known, from
//! the kernels that run on this firmware (Linux 5.15.198 `arch/arm/mach-exynos/platsmp.c`
//! `exynos_boot_secondary`, `firmware.c` `exynos_set_cpu_boot_addr` and `exynos_cpu_boot`;
//! vendor kernel `arch/arm/mach-exynos/platsmp.c` `boot_secondary`): it needs the core
//! powered, the entry address in SYSRAM, an SMC and an SGI.
//!
//! # Store loops
//!
//! Do not leave a core in an endless store-only loop on a cache line other cores use. On the
//! board (Cortex-A9 r3p0), CPU1 and CPU2 looping on `str; add; b` stopped the other cores
//! that wrote the same line, in all three runs that tried it, with no fault reported. The
//! same loop on a line nobody else touches disturbed nothing, and still took interrupts.
//! So did a bounded store loop on a shared line, leaving every million stores. The cause
//! is not known and no published erratum has been matched to it.
//!
//! # Failure trace
//!
//! A starting core that lacks VFP access or cannot set `ACTLR.SMP` writes `<n>V` or `<n>S`
//! to UART2 and halts; [`start_cpu`] then returns [`StartError::BootTimeout`].

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::hint;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use crate::drivers::gic::Gic;
use crate::drivers::scu::Scu;
use crate::drivers::timer::Mct;
use crate::{console, cpu, firmware, soc};

/// `exynos-regs-pmu.h:134-138`: `EXYNOS_ARM_CORE_CONFIGURATION(n)`, with its status
/// register 4 bytes above.
const fn pmu_core_config(cpu: usize) -> usize {
    soc::PMU_BASE + 0x2000 + 0x80 * cpu
}
/// `exynos-regs-pmu.h:181` `S5P_CORE_LOCAL_PWR_EN`.
const CORE_LOCAL_PWR_EN: u32 = 3;

/// `firmware.c:81-89`: the boot address register of a core in the Non-secure SYSRAM page.
const fn sysram_boot_addr(cpu: usize) -> usize {
    soc::SYSRAM_NS_BASE + 0x1C + 4 * cpu
}

/// The SGI that wakes the firmware's pen, the one Linux uses: `arch/arm/kernel/smp.c:62`,
/// `IPI_WAKEUP` is first in its enum. Applications should use other IDs.
pub const WAKE_SGI: u16 = 0;

/// `SCTLR.M`, `arch/arm/include/asm/cp15.h`.
const SCTLR_M: u32 = 1;
/// `ACTLR.SMP`, `arch/arm/mm/proc-v7.S:306`.
const ACTLR_SMP: u32 = 1 << 6;

const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;
/// `platsmp.c:342-344`: "wait max 10 ms until cpu1 is on".
const POWER_TIMEOUT_MS: u64 = 10;
/// `platsmp.c:368`: one second.
const BOOT_TIMEOUT_MS: u64 = 1000;
/// `platsmp.c:390` retries every 10 µs.
const BOOT_RETRY_COUNTS: u64 = soc::MCT_CLOCK_HZ as u64 / 100_000;

/// Why [`start_cpu`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// There is no such core, or it is CPU0.
    InvalidCpu,
    /// Only CPU0 may start cores.
    NotCpu0,
    /// The MMU of this core is off, see [`mmu::enable`](crate::mmu::enable).
    MmuOff,
    /// `ACTLR.SMP` is clear on this core, so memory is not coherent.
    NotCoherent,
    /// The SCU is off: the firmware ignored the write in [`mmu::enable`](crate::mmu::enable).
    ScuOff,
    /// The core was started before.
    AlreadyOnline,
    /// The core did not power up. Carries its PMU status register.
    PowerTimeout(u32),
    /// The core did not report in. See the failure trace in the module documentation.
    BootTimeout,
}

static ENTRIES: [AtomicPtr<()>; soc::CPU_COUNT] =
    [const { AtomicPtr::new(ptr::null_mut()) }; soc::CPU_COUNT];
static ONLINE: [AtomicBool; soc::CPU_COUNT] = [const { AtomicBool::new(false) }; soc::CPU_COUNT];

unsafe extern "C" {
    fn __exynos_secondary_entry();
}

// Called by `__exynos_secondary_entry`. A `sym` operand keeps the function alive through LTO.
global_asm!(
    ".pushsection .text.exynos_secondary_rust, \"ax\", %progbits",
    ".global __exynos_secondary_rust",
    ".type __exynos_secondary_rust, %function",
    "__exynos_secondary_rust:",
    "b {rust}",
    ".size __exynos_secondary_rust, . - __exynos_secondary_rust",
    ".popsection",
    rust = sym secondary_rust,
);

extern "C" fn secondary_rust(cpu: usize) -> ! {
    let entry = ENTRIES[cpu].load(Ordering::Acquire);
    // SAFETY: `start_cpu` stored this from a `fn(usize) -> !` before starting this core, and
    // the Acquire load makes it visible. The core only gets here through `start_cpu`.
    let entry = unsafe { core::mem::transmute::<*mut (), fn(usize) -> !>(entry) };
    ONLINE[cpu].store(true, Ordering::Release);
    entry(cpu)
}

/// Whether a core has reported in. CPU0 always has.
pub fn is_online(cpu: usize) -> bool {
    cpu == 0 || ONLINE.get(cpu).is_some_and(|flag| flag.load(Ordering::Acquire))
}

/// The raw PMU status register of a core. Its low two bits are both set when it is
/// powered: `platsmp.c:130-134`.
///
/// # Panics
///
/// If there is no such core.
pub fn power_status(cpu: usize) -> u32 {
    assert!(cpu < soc::CPU_COUNT);
    // SAFETY: this PMU register exists for each core, and reading it has no side effects.
    unsafe { ptr::read_volatile((pmu_core_config(cpu) + 4) as *const u32) }
}

/// The boot address the firmware would read for a core, for diagnostics.
///
/// # Panics
///
/// If there is no such core.
pub fn boot_address(cpu: usize) -> u32 {
    assert!(cpu < soc::CPU_COUNT);
    // SAFETY: a word of the Non-secure SYSRAM page, and reading it has no side effects.
    unsafe { ptr::read_volatile(sysram_boot_addr(cpu) as *const u32) }
}

fn power_up(cpu: usize, mct: Mct) -> Result<(), StartError> {
    if power_status(cpu) & CORE_LOCAL_PWR_EN == CORE_LOCAL_PWR_EN {
        return Ok(());
    }
    // SAFETY: requests local power for a core that is off, as `platsmp.c:116-122` does.
    // Nothing runs on that core yet.
    unsafe {
        ptr::write_volatile(pmu_core_config(cpu) as *mut u32, CORE_LOCAL_PWR_EN);
    }
    let start = mct.count();
    while power_status(cpu) & CORE_LOCAL_PWR_EN != CORE_LOCAL_PWR_EN {
        if mct.count() - start > POWER_TIMEOUT_MS * COUNTS_PER_MS {
            return Err(StartError::PowerTimeout(power_status(cpu)));
        }
    }
    Ok(())
}

/// Starts a core at `entry`, which receives the core number, and waits until it reports in.
///
/// CPU0 only, with the MMU on and [`Gic::init`](crate::drivers::gic::Gic::init) done.
/// Waits up to a second, spinning on the MCT, which this starts if needed.
/// From the first call on, the console takes a lock.
pub fn start_cpu(cpu: usize, entry: fn(usize) -> !) -> Result<(), StartError> {
    if cpu == 0 || cpu >= soc::CPU_COUNT {
        return Err(StartError::InvalidCpu);
    }
    if cpu::id() != 0 {
        return Err(StartError::NotCpu0);
    }
    if cpu::read_sctlr() & SCTLR_M == 0 {
        return Err(StartError::MmuOff);
    }
    if cpu::read_actlr() & ACTLR_SMP == 0 {
        return Err(StartError::NotCoherent);
    }
    if !Scu::new().is_enabled() {
        return Err(StartError::ScuOff);
    }
    if is_online(cpu) {
        return Err(StartError::AlreadyOnline);
    }

    let mct = Mct::new();
    mct.start();
    console::enable_locking();
    // The failure trace bypasses the console and does not check the FIFO: empty it first.
    console::uart().flush();

    power_up(cpu, mct)?;

    ENTRIES[cpu].store(entry as *mut (), Ordering::Release);

    let gic = Gic::new();
    let start = mct.count();
    while !is_online(cpu) {
        if mct.count() - start > BOOT_TIMEOUT_MS * COUNTS_PER_MS {
            return Err(StartError::BootTimeout);
        }
        // SAFETY: the Non-secure SYSRAM page holds the boot address the firmware reads
        // (`firmware.c:81-91`), through a Device mapping that needs no cache maintenance.
        // `SMC_CMD_CPU_BOOT` takes the core number. Repeating both is what
        // `platsmp.c:369-391` does until the core reports in.
        unsafe {
            ptr::write_volatile(
                sysram_boot_addr(cpu) as *mut u32,
                __exynos_secondary_entry as *const () as u32,
            );
            asm!("dsb", options(nostack, preserves_flags));
            firmware::smc(firmware::SMC_CMD_CPU_BOOT, cpu as u32, 0, 0);
        }
        gic.send_sgi(cpu, WAKE_SGI);

        let retry = mct.count();
        while mct.count() - retry < BOOT_RETRY_COUNTS {
            hint::spin_loop();
        }
    }
    Ok(())
}

/// A spinning lock around `T`. IRQ stays masked on the holding core, so an interrupt
/// handler on it cannot deadlock on the same lock.
pub struct SpinLock<T> {
    locked: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: `lock` gives one core at a time access to the value, and `T: Send` lets that
// access move between cores.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates a lock, unlocked.
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicU32::new(0), value: UnsafeCell::new(value) }
    }

    /// Waits for the lock.
    ///
    /// # Panics
    ///
    /// If the MMU is off on this core, where exclusive access cannot be relied on.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        assert!(cpu::read_sctlr() & SCTLR_M != 0, "SpinLock needs the MMU on");
        let irq_was_masked = cpu::mask_irq();
        while self
            .locked
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            hint::spin_loop();
        }
        SpinLockGuard { lock: self, irq_was_masked }
    }
}

/// Access to the value of a [`SpinLock`], which is released on drop.
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    irq_was_masked: bool,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard's existence proves this core holds the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this the only access through the guard.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(0, Ordering::Release);
        cpu::restore_irq(self.irq_was_masked);
    }
}
