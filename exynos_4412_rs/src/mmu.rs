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
//! | above DRAM | fault | not ours: speculative accesses must not reach it |
//!
//! # After [`enable`]
//!
//! - DRAM is Shareable and `ACTLR.SMP` is set, so this core's L1 stays coherent with the
//!   other cores through the SCU. `LDREX`/`STREX` work there, so read-modify-write
//!   atomics and `AtomicU64` become usable, and synchronize across CPUs. Core-to-core
//!   coherency covers only cores that have themselves reached this state: see [`crate::smp`].
//! - `SCTLR.A` stays set. Generated code is `strict-align`, so nothing needs unaligned
//!   access, and a real one still faults.
//! - Anything else using DRAM directly, such as DMA, needs cache maintenance.
//!   TODO: range clean and invalidate operations.
//! - This does not enable the L2 cache: call
//!   [`L2c::enable`](crate::drivers::l2c::L2c::enable) afterwards.

use core::arch::{asm, global_asm};

use crate::cpu;
use crate::drivers::l2c::L2c;
use crate::drivers::scu::Scu;
use crate::soc;

const SECTION_SIZE: usize = 1 << 20;
const TABLE_ENTRIES: usize = 4096;

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
