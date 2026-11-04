//! The global allocator: TLSF (`rlsf`) over a region, up to the DRAM free, the linker script reserves, behind a lock
//! that all four cores share.
//!
//! With it, `alloc`'s `Box`, `Vec`, `String` and the rest work on every core, and in interrupt
//! handlers. An application:
//!
//! 1. names it its allocator with [`global_heap!`](crate::global_heap), once, at the top level;
//! 2. gives the heap a size in its `memory.x`, `__heap_size = 64M;`, and makes `RAM` that much
//!    longer: `link.x` puts the heap after the stacks of all cores;
//! 3. calls [`init`] on CPU0 after [`mmu::enable`](crate::mmu::enable), before anything
//!    allocates or other cores start;
//! 4. writes `extern crate alloc;`.
//!
//! The library does not name it itself, so that an application of the same workspace can keep an
//! allocator of its own.
//!
//! # Time
//!
//! TLSF finds and frees a block in a bounded number of steps, whatever the heap's size and
//! history: no searching lists, no compaction. Each call holds the lock with IRQs masked on its
//! core for that time only. Growing or shrinking a block allocates a new one, copies with the lock
//! released, and frees the old one, so a large copy does not hold up the other cores.
//!
//! # Memory
//!
//! Each block carries an 8-byte header, and sizes round up to 16 bytes, the least TLSF hands
//! out. Asking for more alignment than 16 bytes can cost up to that much again. The heap is
//! Normal, cacheable memory like the rest of DRAM: a buffer for DMA needs the same cache care as
//! one in `.bss`.
//!
//! # Failure
//!
//! When no free block is large enough, the allocator returns null and counts it
//! ([`Stats::failures`]). Through `Box`, `Vec` and the like that becomes a panic; the raw
//! `alloc::alloc::alloc` returns the null.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

use rlsf::Tlsf;

use crate::smp::SpinLock;

/// First-level lists: blocks up to `16 << 26` bytes, 1 GiB, less 16. A larger heap is
/// handed over in pieces of that size, so no block can be larger.
const FLLEN: usize = 26;
/// Second-level lists in each: sizes within 1/32 of each other share a list.
const SLLEN: usize = 32;

type Pool = Tlsf<'static, u32, u32, FLLEN, SLLEN>;

/// What the heap has done since [`init`]. Sizes are what callers asked for, without headers or
/// rounding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes the heap was given.
    pub size: usize,
    /// Bytes allocated now.
    pub used: usize,
    /// The most bytes allocated at once.
    pub peak: usize,
    /// Allocations that succeeded.
    pub allocations: u32,
    /// Blocks freed.
    pub frees: u32,
    /// Allocations refused for want of a large enough free block.
    pub failures: u32,
}

struct Heap {
    pool: Pool,
    stats: Stats,
    ready: bool,
}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap {
    pool: Tlsf::new(),
    stats: Stats { size: 0, used: 0, peak: 0, allocations: 0, frees: 0, failures: 0 },
    ready: false,
});

/// The allocator, for [`global_heap!`](crate::global_heap): a handle on the one heap.
pub struct Allocator;

/// Why [`init`] did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// It has run already.
    AlreadyDone,
    /// `memory.x` gave the heap no size, or too little for a block.
    NoRegion,
}

unsafe extern "C" {
    static mut __heap_start: u8;
    static mut __heap_end: u8;
}

/// Hands the region `link.x` reserved to the allocator, and returns its size in bytes.
///
/// # Panics
///
/// If the MMU is off on this core: the lock needs it.
pub fn init() -> Result<usize, InitError> {
    let start = &raw mut __heap_start;
    let end = &raw mut __heap_end;
    let length = end as usize - start as usize;
    let mut heap = HEAP.lock();
    if heap.ready {
        return Err(InitError::AlreadyDone);
    }
    let region = NonNull::new(ptr::slice_from_raw_parts_mut(start, length)).ok_or(InitError::NoRegion)?;
    // SAFETY: `link.x` reserves the region for the heap alone, for the life of the program, and
    // `ready` makes this the only time it is handed over.
    let size = unsafe { heap.pool.insert_free_block_ptr(region) }.ok_or(InitError::NoRegion)?.get();
    heap.ready = true;
    heap.stats.size = size;
    Ok(size)
}

/// What the heap has done so far, from any core.
pub fn stats() -> Stats {
    HEAP.lock().stats
}

// SAFETY: every call goes through the lock, so the pool is changed by one core at a time, with
// no handler of that core in between; blocks come from the pool and go back to it with the
// alignment they were allocated with, which `GlobalAlloc` passes back in `layout`.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = HEAP.lock();
        match heap.pool.allocate(layout) {
            Some(block) => {
                let stats = &mut heap.stats;
                stats.allocations += 1;
                stats.used += layout.size();
                stats.peak = stats.peak.max(stats.used);
                block.as_ptr()
            }
            None => {
                heap.stats.failures += 1;
                ptr::null_mut()
            }
        }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        let mut heap = HEAP.lock();
        // SAFETY: `GlobalAlloc`'s contract: `block` came from `alloc` with `layout`, so from
        // this pool, with this alignment, and is not freed twice.
        unsafe { heap.pool.deallocate(NonNull::new_unchecked(block), layout.align()) };
        heap.stats.frees += 1;
        heap.stats.used -= layout.size();
    }
}

/// Makes [`heap`](crate::heap) the application's global allocator.
///
/// ```ignore
/// exynos_4412_rs::global_heap!();
/// ```
#[macro_export]
macro_rules! global_heap {
    () => {
        #[global_allocator]
        static __EXYNOS_HEAP: $crate::heap::Allocator = $crate::heap::Allocator;
    };
}
