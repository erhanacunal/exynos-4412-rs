//! Identity-mapped MMU with the L1 caches on.
//!
//! The translation table is built at compile time and lives in `.rodata`. It is never
//! written at run time, so it needs no cache maintenance. Table walks are non-cacheable
//! (`TTBR0` attributes 0) and read it straight from DRAM.
//!
//! Virtual addresses equal physical ones, in 1 MiB sections:
//!
//! | Range | Mapping | Reason |
//! |---|---|---|
//! | `0x0000_0000..0x0010_0000` | fault | catches null pointers |
//! | `0x0010_0000..DRAM` | Device, execute-never | SYSRAM and peripherals; never speculated |
//! | DRAM | Normal, write-back write-allocate, Shareable | [`soc::DRAM_BASE`] |
//! | `0xC000_0000`, 16 MiB | Normal, non-cacheable, execute-never | [`DRAM_UNCACHED`]: the start of DRAM again, to see what DRAM holds |
//! | rest above DRAM | fault | not ours: speculative accesses must not reach it |
//!
//! # After [`enable`]
//!
//! - DRAM is Shareable and `ACTLR.SMP` is set, so this core's L1 stays coherent with the
//!   other cores through the SCU. `LDREX`/`STREX` work there, so read-modify-write
//!   atomics and `AtomicU64` become usable, and synchronize across CPUs. Core-to-core
//!   coherency covers only cores that have themselves reached this state: see [`crate::smp`].
//! - `SCTLR.A` stays set. Generated code is `strict-align`, so nothing needs unaligned
//!   access, and a real one still faults.
//! - Anything else using DRAM directly, such as DMA, needs cache maintenance. A device that
//!   reads needs [`clean_dcache_range`] first. A device that writes needs
//!   [`discard_dcache_range`] before and [`invalidate_dcache_range`] after, on whole lines of
//!   [`DCACHE_LINE`] bytes.
//! - This does not enable the L2 cache: call
//!   [`L2c::enable`](crate::drivers::l2c::L2c::enable) afterwards.

use core::arch::{asm, global_asm};

/// L1 line of the Cortex-A9. The L2 cache has the same: `arch/arm/mm/cache-l2x0.c:37`.
const L1_LINE_SIZE: usize = 32;
/// The line of both data caches. Memory a device writes has to start and end on these.
pub const DCACHE_LINE: usize = L1_LINE_SIZE;

use crate::cpu;
use crate::drivers::l2c::L2c;
use crate::drivers::scu::Scu;
use crate::soc;

const SECTION_SIZE: usize = 1 << 20;
const TABLE_ENTRIES: usize = 4096;

/// Where the start of DRAM shows a second time, without the caches: a read there gets what DRAM
/// holds, whatever the caches hold for the same address. It is for checking cache maintenance,
/// as `l2_test` does. Do not write there what the cached view also uses: the two views of one
/// line disagree by design, and a dirty line would later be written back over the write.
pub const DRAM_UNCACHED: usize = 0xC000_0000;
/// How much of DRAM it covers, from [`soc::DRAM_BASE`]: the applications' images, `.bss` and
/// stacks are all there.
pub const DRAM_UNCACHED_SIZE: usize = 16 << 20;

const _: () = assert!(DRAM_UNCACHED >= soc::DRAM_BASE + soc::DRAM_SIZE && DRAM_UNCACHED_SIZE <= soc::DRAM_SIZE);

/// The uncached address of a DRAM address, if [`DRAM_UNCACHED`] covers it.
pub const fn uncached(address: usize) -> Option<usize> {
    if address >= soc::DRAM_BASE && address < soc::DRAM_BASE + DRAM_UNCACHED_SIZE {
        Some(address - soc::DRAM_BASE + DRAM_UNCACHED)
    } else {
        None
    }
}

// Section descriptor bits: Linux 5.15.198 `arch/arm/include/asm/pgtable-2level-hwdef.h`.
/// Line 19 `PMD_TYPE_SECT`.
const SECT: u32 = 2;
/// Line 29 `PMD_SECT_BUFFERABLE`. Alone, with TEX 0 and C 0, this is shareable Device.
const SECT_B: u32 = 1 << 2;
/// Line 30 `PMD_SECT_CACHEABLE`.
const SECT_C: u32 = 1 << 3;
/// Line 31 `PMD_SECT_XN`.
const SECT_XN: u32 = 1 << 4;
/// Line 36 `PMD_SECT_S`.
const SECT_S: u32 = 1 << 16;
/// Lines 32-33 `PMD_SECT_AP_WRITE | PMD_SECT_AP_READ`: full access, domain 0 implied.
const SECT_AP_FULL: u32 = (1 << 10) | (1 << 11);
/// Line 34 `PMD_SECT_TEX(1)`; line 46 `PMD_SECT_WBWA` adds C and B.
const SECT_TEX1: u32 = 1 << 12;

const DEVICE: u32 = SECT | SECT_AP_FULL | SECT_B | SECT_XN;
const NORMAL_WBWA: u32 = SECT | SECT_AP_FULL | SECT_TEX1 | SECT_C | SECT_B | SECT_S;
/// TEX 1 with C 0 and B 0: Normal, non-cacheable, inner and outer (ARMv7-A ARM, short-descriptor
/// memory region attributes, TEX remap off). Linux 5.15.198 has no section macro for it.
const NORMAL_UNCACHED: u32 = SECT | SECT_AP_FULL | SECT_TEX1 | SECT_S | SECT_XN;

const _: () = assert!(
    soc::DRAM_BASE.is_multiple_of(SECTION_SIZE) && soc::DRAM_SIZE.is_multiple_of(SECTION_SIZE)
);

// SCTLR bits: `arch/arm/include/asm/cp15.h:10-40`.
const SCTLR_M: u32 = 1 << 0;
const SCTLR_C: u32 = 1 << 2;

// TTBR0 base alignment: the table has 2^12 entries of 4 bytes, for TTBCR.N = 0.
#[repr(C, align(16384))]
struct TranslationTable([u32; TABLE_ENTRIES]);

const fn build_table() -> TranslationTable {
    let mut entries = [0; TABLE_ENTRIES];
    let dram_start = soc::DRAM_BASE / SECTION_SIZE;
    let dram_end = dram_start + soc::DRAM_SIZE / SECTION_SIZE;
    let mut index = 1;
    while index < dram_end {
        let attributes = if index < dram_start { DEVICE } else { NORMAL_WBWA };
        entries[index] = (index * SECTION_SIZE) as u32 | attributes;
        index += 1;
    }
    let alias_start = DRAM_UNCACHED / SECTION_SIZE;
    let mut section = 0;
    while section < DRAM_UNCACHED_SIZE / SECTION_SIZE {
        entries[alias_start + section] = (soc::DRAM_BASE + section * SECTION_SIZE) as u32 | NORMAL_UNCACHED;
        section += 1;
    }
    TranslationTable(entries)
}

static TABLE: TranslationTable = build_table();

// Read by `__exynos_mmu_on` while the MMU is off. A `sym` operand keeps the table alive
// through LTO, and the table never changes, so this needs no coherency.
global_asm!(
    ".pushsection .rodata.exynos_mmu_table_addr, \"a\", %progbits",
    ".p2align 2",
    ".global __exynos_mmu_table_addr",
    "__exynos_mmu_table_addr:",
    ".word {table}",
    ".popsection",
    table = sym TABLE,
);

/// Writes a range of memory out to DRAM, from the L1 cache of this core and then from the L2
/// cache, so that a device reading DRAM sees what the CPU wrote. The lines stay valid.
///
/// Addresses are the identity mapping's, so also physical. Works with the MMU or either
/// cache off. Dirty lines in the L1 cache of another core are not covered.
pub fn clean_dcache_range(start: usize, len: usize) {
    if len == 0 {
        return;
    }
    erratum_764369_barrier();
    let mut line = start & !(L1_LINE_SIZE - 1);
    while line < start + len {
        // SAFETY: cleaning changes no data in the view of any observer. `DCCMVAC`, the same
        // operation as `arch/arm/mm/cache-v7.S:405`. An address without a mapping would fault,
        // and the caller passes memory it has been using.
        unsafe {
            asm!("mcr p15, 0, {}, c7, c10, 1", in(reg) line, options(nostack, preserves_flags));
        }
        line += L1_LINE_SIZE;
    }
    // SAFETY: a barrier has no effect other than ordering: the L1 lines must have left before
    // the L2 operation below.
    unsafe {
        asm!("dsb", options(nostack, preserves_flags));
    }
    L2c::new().clean_range(start, len);
}

/// Before a device writes a range of DRAM: drops the range from the L1 cache of this core, then
/// from the L2 cache, discarding what the CPU wrote there and did not clean. No dirty line is
/// left to be written back over the device's data later. Some of what the CPU wrote may still
/// reach DRAM before this returns: on the board, `l2_test` saw a few lines of it do so. That is
/// before the device starts, so it writes over them. Lines may come back into the caches
/// meanwhile, but clean: call [`invalidate_dcache_range`] once the device is done.
///
/// Same addresses and limits as [`clean_dcache_range`]. Do not touch the range from the CPU
/// until the device is done.
///
/// # Panics
///
/// If `start` or `len` is not a multiple of [`DCACHE_LINE`]: a line shared with other data would
/// lose that data.
pub fn discard_dcache_range(start: usize, len: usize) {
    assert_whole_lines(start, len);
    invalidate_l1_range(start, len);
    L2c::new().invalidate_range(start, len);
}

/// After a device wrote a range of DRAM: drops the range from the L2 cache, then from the L1
/// cache of this core, so that reads come from DRAM. Lines the CPU pulled in while the device
/// worked, by speculation too, go with them. The L2 cache goes first, or the L1 cache could fill
/// again from it. Call [`discard_dcache_range`] before the device starts, or a dirty line could
/// still be written back over what it wrote.
///
/// # Panics
///
/// If `start` or `len` is not a multiple of [`DCACHE_LINE`].
pub fn invalidate_dcache_range(start: usize, len: usize) {
    assert_whole_lines(start, len);
    L2c::new().invalidate_range(start, len);
    invalidate_l1_range(start, len);
}

fn assert_whole_lines(start: usize, len: usize) {
    assert!(
        start.is_multiple_of(DCACHE_LINE) && len.is_multiple_of(DCACHE_LINE),
        "cache invalidate of {:#x}+{:#x}: not whole lines",
        start,
        len,
    );
}

/// Cortex-A9 erratum 764369: a maintenance operation by address on Inner Shareable memory may
/// not reach its point of coherency. Linux 5.15.198 puts a `dsb` before its loops
/// (`cache-v7.S:371`) and sets bit 0 of the SCU diagnostic register
/// ([`Scu::enable`](crate::drivers::scu::Scu)); this is the first half.
fn erratum_764369_barrier() {
    // SAFETY: a barrier has no effect other than ordering.
    unsafe {
        asm!("dsb", options(nostack, preserves_flags));
    }
}

/// `DCIMVAC` over the range, then a barrier, so the lines are gone before what follows.
fn invalidate_l1_range(start: usize, len: usize) {
    erratum_764369_barrier();
    for line in (start..start + len).step_by(L1_LINE_SIZE) {
        // SAFETY: the callers check that the range is whole lines, so only data in the range is
        // discarded, which they ask for. `DCIMVAC`, the operation of
        // `arch/arm/mm/cache-v7.S:383`. An address without a mapping would fault.
        unsafe {
            asm!("mcr p15, 0, {}, c7, c6, 1", in(reg) line, options(nostack, preserves_flags));
        }
    }
    // SAFETY: a barrier has no effect other than ordering.
    unsafe {
        asm!("dsb", options(nostack, preserves_flags));
    }
}

/// Turns on the MMU, the L1 data and instruction caches and branch prediction on this
/// core, CPU0 only: [`smp::start_cpu`](crate::smp::start_cpu) does this for the others.
///
/// Enables the SCU first if the firmware left it off. Doing it here, before any cache
/// holds data, is what spares the full cache clean `scu_enable` needs in
/// `arch/arm/kernel/smp_scu.c:65-69`.
///
/// Does nothing if the MMU or the data cache is already on: repeating the set/way
/// invalidation would discard dirty data.
///
/// # Panics
///
/// If `ACTLR.SMP` cannot be set from the Non-secure world.
pub fn enable() {
    if cpu::read_sctlr() & (SCTLR_M | SCTLR_C) != 0 {
        return;
    }

    Scu::new().enable();

    let l2c = L2c::new();
    if l2c.is_enabled() {
        l2c.invalidate_all();
    }

    let failed: u32;
    // SAFETY: the mapping is the identity for all code and data, so execution and every
    // live pointer carry on across the switch. The clobbers are the routine's own list,
    // see `__exynos_mmu_on`. Not `nomem`: this changes how later memory accesses behave.
    unsafe {
        asm!(
            "bl __exynos_mmu_on",
            lateout("r0") failed,
            out("r1") _,
            out("r2") _,
            out("r3") _,
            out("r4") _,
            out("r5") _,
            out("r12") _,
            out("lr") _,
        );
    }
    assert!(failed == 0, "ACTLR.SMP cannot be set: ACTLR {:#x}", cpu::read_actlr());
}
