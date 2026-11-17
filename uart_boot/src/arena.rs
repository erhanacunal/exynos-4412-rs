//! The allocator, for the ext4 code (`userdata`) alone: a bump arena over the region `link.x`
//! reserves (`__heap_size` in memory.x).
//!
//! It works with the MMU off (no exclusive accesses, unlike the library's `heap`): this program
//! allocates on CPU0 only, never in an interrupt handler. Freeing only counts; once every block
//! is back (a mounted file system dropped) the arena starts again from its first byte, so each
//! `userdata` call has the whole region.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

unsafe extern "C" {
    static mut __heap_start: u8;
    static mut __heap_end: u8;
}

struct Arena {
    /// The next free byte; 0 before the first allocation.
    next: UnsafeCell<usize>,
    /// Blocks handed out and not yet freed.
    live: UnsafeCell<usize>,
}

// SAFETY: see the module documentation: one core, no allocation in handlers.
unsafe impl Sync for Arena {}

#[global_allocator]
static ARENA: Arena = Arena { next: UnsafeCell::new(0), live: UnsafeCell::new(0) };

fn region() -> (usize, usize) {
    (&raw const __heap_start as usize, &raw const __heap_end as usize)
}

unsafe impl GlobalAlloc for Arena {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (start, end) = region();
        // SAFETY: see `Sync`.
        let (next, live) = unsafe { (&mut *self.next.get(), &mut *self.live.get()) };
        let from = if *next == 0 { start } else { *next };
        let at = from.next_multiple_of(layout.align());
        match at.checked_add(layout.size()) {
            Some(to) if to <= end => {
                *next = to;
                *live += 1;
                at as *mut u8
            }
            _ => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // SAFETY: see `Sync`.
        let (next, live) = unsafe { (&mut *self.next.get(), &mut *self.live.get()) };
        *live -= 1;
        if *live == 0 {
            *next = 0;
        }
    }
}
