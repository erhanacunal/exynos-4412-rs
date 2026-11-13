// SPDX-License-Identifier: GPL-2.0-only
//! Registers of the Exynos 4x12 JPEG codec.
//!
//! From Linux 5.15.198, `drivers/media/platform/s5p-jpeg/jpeg-regs.h` (cited by line), Copyright
//! (c) 2011-2014 Samsung Electronics Co., Ltd.

/// `:173-201`.
pub const CNTL: usize = 0x00;
pub const INT_EN: usize = 0x04;
pub const INT_STATUS: usize = 0x0C;
/// The JPEG stream: read when decoding, written when encoding.
pub const OUT_MEM_BASE: usize = 0x10;
/// The picture's size when encoding: [`x_size`] and [`y_size`].
pub const IMG_SIZE: usize = 0x14;
/// The raw picture's planes: written when decoding, read when encoding.
pub const IMG_BA_PLANE_1: usize = 0x18;
pub const IMG_BA_PLANE_2: usize = 0x24;
pub const IMG_BA_PLANE_3: usize = 0x30;
pub const TBL_SEL: usize = 0x3C;
pub const IMG_FMT: usize = 0x40;
/// When decoding, the stream's size in units of 32 bytes; after encoding, in bytes.
pub const BITSTREAM_SIZE: usize = 0x44;
pub const HUFF_CNT: usize = 0x4C;
pub const FIFO_STATUS: usize = 0x50;
/// After decoding: width in bits 0-15, height in bits 16-31.
pub const DECODE_XY_SIZE: usize = 0x54;
/// After decoding: [`DECODED_IMG_FMT_MASK`], 0 grey, 1 4:4:4, 2 4:2:2, 3 4:2:0
/// (`jpeg-core.c:561`).
pub const DECODE_IMG_FMT: usize = 0x58;
pub const DECODED_IMG_FMT_MASK: u32 = 0x3;
pub const DECODED_SIZE_MASK: u32 = 0xFFFF;

/// `:209-227`, fields of [`CNTL`].
pub const CNTL_MODE_MASK: u32 = 0x3;
pub const CNTL_DEC_MODE: u32 = 1 << 0;
pub const CNTL_ENC_MODE: u32 = 1 << 1;
pub const CNTL_HUF_TBL_EN: u32 = 1 << 19;
pub const CNTL_SYS_INT_EN: u32 = 1 << 28;
/// Low holds the codec in reset.
pub const CNTL_SOFT_RESET_HI: u32 = 1 << 29;

/// `:230-237`, of [`INT_EN`] and [`INT_STATUS`].
pub const INT_EN_MASK: u32 = 0x1F;
pub const INT_PROT_ERR: u32 = 1 << 0;
pub const INT_IMG_COMPLETION: u32 = 1 << 1;
pub const INT_DEC_INVALID_FORMAT: u32 = 1 << 2;
pub const INT_MULTI_SCAN_ERROR: u32 = 1 << 3;
pub const INT_FRAME_ERR: u32 = 1 << 4;

/// `:250-255`.
pub const fn x_size(x: u32) -> u32 {
    x & 0xFFFF
}
pub const fn y_size(y: u32) -> u32 {
    (y & 0xFFFF) << 16
}

/// `:258-308`, fields of [`IMG_FMT`]. The low half is the raw picture's format, the same
/// numbers for either direction; bits 24-25 the stream's subsampling when encoding.
pub const IMG_FMT_RAW_MASK: u32 = 0xFFFF;
pub const IMG_GRAY: u32 = 0;
pub const IMG_RGB: u32 = 1;
pub const IMG_YUV_444: u32 = 2;
pub const IMG_YUV_422: u32 = 3;
pub const IMG_YUV_420: u32 = 4;
pub const GRAY_IP: u32 = 4 << 3;
pub const RGB_IP_16BIT: u32 = 4 << 6;
pub const RGB_IP_32BIT: u32 = 5 << 6;
pub const YUV_444_IP_2P: u32 = 4 << 9;
pub const YUV_422_IP_1P: u32 = 4 << 12;
pub const YUV_422_IP_2P: u32 = 5 << 12;
pub const YUV_420_IP_2P: u32 = 4 << 15;
pub const YUV_420_IP_3P: u32 = 5 << 15;
pub const ENC_FMT_SHIFT: u32 = 24;
pub const ENC_FMT_GRAY: u32 = 0 << ENC_FMT_SHIFT;
pub const ENC_FMT_YUV_444: u32 = 1 << ENC_FMT_SHIFT;
pub const ENC_FMT_YUV_422: u32 = 2 << ENC_FMT_SHIFT;
pub const ENC_FMT_YUV_420: u32 = 3 << ENC_FMT_SHIFT;
pub const SWAP_CHROMA_CRCB: u32 = 1 << 26;

/// `:322,339`: quantisation table `n` for component `c` (1 to 3), and Huffman tables `n`
/// (bit 1 AC, bit 0 DC) for it, in [`TBL_SEL`].
pub const fn q_tbl_comp(c: u32, n: u32) -> u32 {
    n << ((c - 1) << 1)
}
pub const fn huff_tbl_comp(c: u32, n: u32) -> u32 {
    n << (((c - 1) << 1) + 6)
}

/// `:374`: the four quantisation tables, 64 bytes each.
pub const fn qtbl_content(n: usize) -> usize {
    0x100 + n * 0x40
}

/// `:376-405`: the Huffman tables, code lengths (`L`) and values (`V`), DC and AC, luma and
/// chroma.
pub const HUFF_TBL_HDCLL: usize = 0x200;
pub const HUFF_TBL_HDCLV: usize = 0x210;
pub const HUFF_TBL_HDCCL: usize = 0x220;
pub const HUFF_TBL_HDCCV: usize = 0x230;
pub const HUFF_TBL_HACLL: usize = 0x240;
pub const HUFF_TBL_HACLV: usize = 0x250;
pub const HUFF_TBL_HACCL: usize = 0x300;
pub const HUFF_TBL_HACCV: usize = 0x310;
