//! The memory `Gpu` draws with: one fixed pool, in `.bss`, that the GPU's MMUs map. Nothing is
//! allocated from a heap.
//!
//! What the CPU writes and the GPU reads is cleaned from the CPU's caches before the GPU runs;
//! what the GPU writes, the CPU does not read, but for the screen, which is uncached.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use super::Error;
use crate::drivers::mali;

/// The most tiles a frame takes: one polygon list block each (`mali::PLB_MAX_BLOCKS`). 512 x
/// 512 pixels, or 352 x 352 square... any area of up to 512 tiles.
pub const MAX_TILES: usize = mali::PLB_MAX_BLOCKS;
/// The most draws in one frame.
pub const MAX_DRAWS: usize = 64;
/// Bytes for meshes, textures and shaders, kept until restart.
pub const LASTING_BYTES: usize = 2 << 20;
/// Bytes for a frame's own: each draw's uniforms, render state and the GP's output.
pub const FRAME_BYTES: usize = 512 << 10;

const BLOCK_WORDS: usize = mali::PLB_BLOCK / 4;
const HEAP_WORDS: usize = 16 * 1024;
pub(crate) const STREAM_WORDS: usize = MAX_TILES * 4 + 4;
/// A draw's vertex commands: 12 of two words (`mali::Commands`, the `vs_` ones).
pub(crate) const VS_DRAW_WORDS: usize = 24;
/// The PLBU's head and end, and a draw's commands: 5 and 1, and 13, of two words.
pub(crate) const PLBU_HEAD_WORDS: usize = 12;
pub(crate) const PLBU_DRAW_WORDS: usize = 26;

#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub(crate) struct Lines<T>(pub T);

#[repr(C, align(4096))]
pub(crate) struct Page<T>(pub T);

/// What every frame shares.
#[repr(C, align(4096))]
pub(crate) struct Fixed {
    /// The polygon list blocks, one a tile, row after row of the frame's tiles.
    pub plb: [u32; MAX_TILES * BLOCK_WORDS],
    pub heap: [u32; HEAP_WORDS],
    pub streams: [Lines<[u32; STREAM_WORDS]>; mali::PP_COUNT],
    pub block_addresses: Lines<[u32; MAX_TILES]>,
    /// The frame's own render state, for tiles without polygons: zeros.
    pub frame_state: Lines<[u32; 16]>,
    pub vs_list: Lines<[u32; MAX_DRAWS * VS_DRAW_WORDS]>,
    pub plbu_list: Lines<[u32; PLBU_HEAD_WORDS + MAX_DRAWS * PLBU_DRAW_WORDS]>,
}

pub(crate) struct Pool {
    pub fixed: Fixed,
    pub lasting: Page<[u8; LASTING_BYTES]>,
    pub frame: Page<[u8; FRAME_BYTES]>,
    pub tables: mali::IdentityMap<4>,
}

pub(crate) struct Store(UnsafeCell<Pool>);

// SAFETY: `take` hands out the one reference, once.
unsafe impl Sync for Store {}

static STORE: Store = Store(UnsafeCell::new(Pool {
    fixed: Fixed {
        plb: [0; MAX_TILES * BLOCK_WORDS],
        heap: [0; HEAP_WORDS],
        streams: [Lines([0; STREAM_WORDS]); mali::PP_COUNT],
        block_addresses: Lines([0; MAX_TILES]),
        frame_state: Lines([0; 16]),
        vs_list: Lines([0; MAX_DRAWS * VS_DRAW_WORDS]),
        plbu_list: Lines([0; PLBU_HEAD_WORDS + MAX_DRAWS * PLBU_DRAW_WORDS]),
    },
    lasting: Page([0; LASTING_BYTES]),
    frame: Page([0; FRAME_BYTES]),
    tables: mali::IdentityMap::new(),
}));

static TAKEN: AtomicBool = AtomicBool::new(false);

/// The pool, the first time; `Error::AlreadyStarted` after.
pub(crate) fn take() -> Result<&'static mut Pool, Error> {
    if TAKEN.swap(true, Ordering::AcqRel) {
        return Err(Error::AlreadyStarted);
    }
    // SAFETY: the flag lets this run once: the only reference.
    Ok(unsafe { &mut *STORE.0.get() })
}

/// A bump allocator over a byte array: addresses out, never freed but all at once.
pub(crate) struct Arena {
    base: usize,
    size: usize,
    used: usize,
}

impl Arena {
    pub fn new(bytes: &mut [u8]) -> Self {
        Self { base: bytes.as_mut_ptr() as usize, size: bytes.len(), used: 0 }
    }

    /// `bytes`, zeroed, at a multiple of `align`, a power of two.
    pub fn alloc(&mut self, bytes: usize, align: usize) -> Result<usize, Error> {
        let start = (self.base + self.used).next_multiple_of(align) - self.base;
        if start + bytes > self.size {
            return Err(Error::OutOfMemory);
        }
        self.used = start + bytes;
        let address = self.base + start;
        // SAFETY: inside the arena's array, which only this arena hands out.
        unsafe { core::ptr::write_bytes(address as *mut u8, 0, bytes) };
        Ok(address)
    }

    /// `values` copied in, at a multiple of `align`.
    pub fn copy<T: Copy>(&mut self, values: &[T], align: usize) -> Result<usize, Error> {
        let address = self.alloc(size_of_val(values), align.max(align_of::<T>()))?;
        // SAFETY: just allocated, of this size and alignment.
        unsafe { core::ptr::copy_nonoverlapping(values.as_ptr(), address as *mut T, values.len()) };
        Ok(address)
    }

    /// Forgets everything allocated.
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// The start and length of what is allocated, to clean from the caches.
    pub fn used(&self) -> (usize, usize) {
        (self.base, self.used)
    }
}

/// `count` words at `address`, which an arena handed out for them.
///
/// # Safety
///
/// `address` is from [`Arena::alloc`], with room for `count` words, and nothing else refers to
/// them meanwhile.
pub(crate) unsafe fn words<'a>(address: usize, count: usize) -> &'a mut [u32] {
    // SAFETY: as the caller promises.
    unsafe { core::slice::from_raw_parts_mut(address as *mut u32, count) }
}

/// The same for floats.
///
/// # Safety
///
/// As for [`words`].
pub(crate) unsafe fn floats<'a>(address: usize, count: usize) -> &'a mut [f32] {
    // SAFETY: as the caller promises.
    unsafe { core::slice::from_raw_parts_mut(address as *mut f32, count) }
}
