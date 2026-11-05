// SPDX-License-Identifier: GPL-2.0-or-later
//! The decoder's output layout, NV12MT, as Linux 5.15.198 documents it
//! (`Documentation/userspace-api/media/v4l/pixfmt-yuv-planar.rst`, "NV12MT"): each plane in
//! tiles of 64x32 bytes, each tile's rows one after the other; the tiles in 2x2 groups, Z-shaped
//! and mirrored Z-shaped by turns from left to right; a last row of tiles without a partner row
//! in plain order. The chroma plane, CbCr interleaved, is laid out like the luma plane, with half
//! its rows.

/// Bytes in a tile's row, rows in a tile, and bytes in a tile.
pub const TILE_WIDTH: usize = 64;
pub const TILE_HEIGHT: usize = 32;
pub const TILE_BYTES: usize = TILE_WIDTH * TILE_HEIGHT;

/// A plane of `columns` x `rows` tiles. `columns` is even: a frame buffer's width is a multiple
/// of 128.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    pub columns: usize,
    pub rows: usize,
}

impl Plane {
    /// The plane of a buffer `width` bytes wide and `height` rows high, both rounded up to whole
    /// tiles.
    pub const fn new(width: usize, height: usize) -> Self {
        Self { columns: width.div_ceil(TILE_WIDTH), rows: height.div_ceil(TILE_HEIGHT) }
    }

    /// Which tile in memory, counted from the plane's start, holds tile column `x` of tile row
    /// `y`.
    pub const fn tile_index(self, x: usize, y: usize) -> usize {
        let mut index = (y & !1) * self.columns + x;
        if y & 1 == 1 {
            // The second row of a pair: after the first row's two tiles of the same Z.
            index += (x & !3) + 2;
        } else if self.rows & 1 == 0 || y != self.rows - 1 {
            // The first row of a pair, where a mirrored Z puts its tiles after the second
            // row's: (x + 2) & !3 is 0 for a Z's columns, 4 for a mirrored one's, and so on.
            index += (x + 2) & !3;
        }
        index
    }

    /// The byte offset, from the plane's start, of byte `x` of row `y`.
    pub const fn offset(self, x: usize, y: usize) -> usize {
        self.tile_index(x / TILE_WIDTH, y / TILE_HEIGHT) * TILE_BYTES
            + (y % TILE_HEIGHT) * TILE_WIDTH
            + x % TILE_WIDTH
    }

    /// Calls `f` with row `y`'s first `width` bytes, in order, a piece of at most one tile row
    /// at a time.
    pub fn row(self, plane: &[u8], y: usize, width: usize, mut f: impl FnMut(&[u8])) {
        let mut x = 0;
        while x < width {
            let len = (width - x).min(TILE_WIDTH);
            let start = self.offset(x, y);
            f(&plane[start..start + len]);
            x += len;
        }
    }

    /// Copies the visible `width` x `height` bytes of `plane` into `out`, row after row,
    /// `stride` bytes apart.
    pub fn detile(self, plane: &[u8], width: usize, height: usize, out: &mut [u8], stride: usize) {
        for y in 0..height {
            let mut x = y * stride;
            self.row(plane, y, width, |piece| {
                out[x..x + piece.len()].copy_from_slice(piece);
                x += piece.len();
            });
        }
    }
}
