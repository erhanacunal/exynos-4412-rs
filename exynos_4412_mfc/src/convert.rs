// SPDX-License-Identifier: GPL-2.0-or-later
//! Decoded pictures onto a [`Framebuffer`], by the CPU: NV12MT detiled ([`crate::tiles`]),
//! converted from BT.601 limited range YCbCr to `0x00RRGGBB`, and scaled up by a whole number,
//! each source pixel a `scale` x `scale` square ([`draw`]), or turned a quarter turn clockwise
//! ([`draw_rotated`]), for a portrait picture on the landscape panel.

use exynos_4412_rs::drivers::display::Framebuffer;

use crate::h264::Sequence;
use crate::tiles::{Plane, TILE_BYTES, TILE_HEIGHT, TILE_WIDTH};

/// The largest scale [`draw`] takes: one tile row's pixels, scaled, are built on the stack.
pub const MAX_SCALE: usize = 8;

/// BT.601, limited range, in 8.8 fixed point: `R = 1.164 (Y - 16) + 1.596 (Cr - 128)`,
/// `G = 1.164 (Y - 16) - 0.391 (Cb - 128) - 0.813 (Cr - 128)`, `B = 1.164 (Y - 16) + 2.018 (Cb -
/// 128)`, each term from a table.
struct Tables {
    luma: [i32; 256],
    red_cr: [i32; 256],
    green_cb: [i32; 256],
    green_cr: [i32; 256],
    blue_cb: [i32; 256],
}

const TABLES: Tables = {
    let mut tables = Tables { luma: [0; 256], red_cr: [0; 256], green_cb: [0; 256], green_cr: [0; 256], blue_cb: [0; 256] };
    let mut i = 0;
    while i < 256 {
        let value = i as i32;
        tables.luma[i] = 298 * (value - 16) + 128;
        tables.red_cr[i] = 409 * (value - 128);
        tables.green_cb[i] = -100 * (value - 128);
        tables.green_cr[i] = -208 * (value - 128);
        tables.blue_cb[i] = 516 * (value - 128);
        i += 1;
    }
    tables
};

/// The chroma terms of one CbCr pair: red, green, blue.
#[inline(always)]
fn chroma_terms(cb: u8, cr: u8) -> (i32, i32, i32) {
    let t = &TABLES;
    (t.red_cr[usize::from(cr)], t.green_cb[usize::from(cb)] + t.green_cr[usize::from(cr)], t.blue_cb[usize::from(cb)])
}

#[inline(always)]
fn pixel(y: u8, (red, green, blue): (i32, i32, i32)) -> u32 {
    let luma = TABLES.luma[usize::from(y)];
    let channel = |term: i32| ((luma + term) >> 8).clamp(0, 255) as u32;
    channel(red) << 16 | channel(green) << 8 | channel(blue)
}

/// Draws the picture whose planes are `luma` and `chroma` (as [`crate::h264::Decoder::planes`]
/// gives them) with its top left corner at (`x`, `y`) of `framebuffer`, `scale` times its size,
/// clipped to the framebuffer. Does not flush.
///
/// # Panics
///
/// If `scale` is 0 or above [`MAX_SCALE`].
pub fn draw(luma: &[u8], chroma: &[u8], sequence: &Sequence, framebuffer: &mut Framebuffer, x: usize, y: usize, scale: usize) {
    assert!((1..=MAX_SCALE).contains(&scale), "scale {} out of 1..={}", scale, MAX_SCALE);
    let luma_plane = Plane::new(sequence.buf_width, sequence.buf_height);
    let chroma_plane = Plane::new(sequence.buf_width, sequence.chroma_height);
    // Even, so that each piece starts on a CbCr pair.
    let width = sequence.width & !1;
    let mut pixels = [0u32; TILE_WIDTH * MAX_SCALE];
    for row in 0..sequence.height {
        let mut column = 0;
        while column < width {
            let count = (width - column).min(TILE_WIDTH);
            let luma_start = luma_plane.offset(column, row);
            let chroma_start = chroma_plane.offset(column, row / 2);
            let luma_piece = &luma[luma_start..luma_start + count];
            let chroma_piece = &chroma[chroma_start..chroma_start + count];
            for pair in 0..count / 2 {
                let terms = chroma_terms(chroma_piece[2 * pair], chroma_piece[2 * pair + 1]);
                for half in 0..2 {
                    let i = 2 * pair + half;
                    pixels[i * scale..(i + 1) * scale].fill(pixel(luma_piece[i], terms));
                }
            }
            for line in 0..scale {
                framebuffer.write_span(x + column * scale, y + row * scale + line, &pixels[..count * scale]);
            }
            column += count;
        }
    }
}

/// Draws the picture turned a quarter turn clockwise, at its size: its top row becomes the
/// framebuffer's right column, and its left column the top row. (`x`, `y`) is the top left
/// corner of the turned picture, `height` x `width` pixels; clipped to the framebuffer. Does not
/// flush.
///
/// It goes through the picture one row of tiles at a time, two columns at once: their pixels
/// share chroma by twos, each pair of rows one CbCr pair, and each becomes a piece of a
/// framebuffer line.
pub fn draw_rotated(luma: &[u8], chroma: &[u8], sequence: &Sequence, framebuffer: &mut Framebuffer, x: usize, y: usize) {
    let luma_plane = Plane::new(sequence.buf_width, sequence.buf_height);
    let chroma_plane = Plane::new(sequence.buf_width, sequence.chroma_height);
    let width = sequence.width & !1;
    let height = sequence.height & !1;
    let mut left = [0u32; TILE_HEIGHT];
    let mut right = [0u32; TILE_HEIGHT];
    for tile_row in 0..height.div_ceil(TILE_HEIGHT) {
        let top = tile_row * TILE_HEIGHT;
        let rows = (height - top).min(TILE_HEIGHT);
        // These rows' chroma: half a tile row of the chroma plane.
        let chroma_top = (tile_row % 2) * (TILE_HEIGHT / 2) * TILE_WIDTH;
        // The picture's rows top..top + rows land on framebuffer columns from here, leftwards.
        let column = x + height - top - rows;
        for source_x in (0..width).step_by(2) {
            let tile_column = source_x / TILE_WIDTH;
            let within = source_x % TILE_WIDTH;
            let luma_base = luma_plane.tile_index(tile_column, tile_row) * TILE_BYTES + within;
            let chroma_base = chroma_plane.tile_index(tile_column, tile_row / 2) * TILE_BYTES + chroma_top + within;
            for pair in 0..rows / 2 {
                let at = chroma_base + pair * TILE_WIDTH;
                let terms = chroma_terms(chroma[at], chroma[at + 1]);
                for half in 0..2 {
                    let row = 2 * pair + half;
                    let at = luma_base + row * TILE_WIDTH;
                    // The lower a row, the further left it lands.
                    left[rows - 1 - row] = pixel(luma[at], terms);
                    right[rows - 1 - row] = pixel(luma[at + 1], terms);
                }
            }
            framebuffer.write_span(column, y + source_x, &left[..rows]);
            framebuffer.write_span(column, y + source_x + 1, &right[..rows]);
        }
    }
}
