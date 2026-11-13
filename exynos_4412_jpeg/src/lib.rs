// SPDX-License-Identifier: GPL-2.0-only
//! The Exynos 4412's JPEG codec: baseline JPEG decoding into, and encoding from, raw pictures
//! in DRAM, one at a time.
//!
//! This crate is GPL-2.0-only, unlike `exynos_4412_rs` (MIT), on which it builds: the order of
//! operations, the register settings and the encoder's tables follow Linux 5.15.198's
//! `drivers/media/platform/s5p-jpeg` (Copyright (c) 2011-2014 Samsung Electronics Co., Ltd.),
//! the Exynos 4x12 parts of `jpeg-core.c` and `jpeg-hw-exynos4.c`, cited by file and line. That
//! driver is GPL-2.0-only, not "or later" as `s5p-mfc` is, so this crate is too. The code is
//! written for this crate. An application that links it is GPL-2.0 as well.
//!
//! # The codec
//!
//! At `0x1184_0000`, interrupt `GIC_SPI 88`, in the camera power domain, clocked by `aclk160`:
//! [`exynos_4412_rs::drivers::jpeg`] turns it on. It reads and writes DRAM at physical addresses
//! with no system MMU in between; this crate hands it the identity-mapped addresses of the
//! slices it is given, and keeps the caches out of the way. Decoding reads the stream's own
//! tables; encoding uses Linux's four quantisation levels ([`Quality`]) and the standard
//! Huffman tables, and writes a whole JPEG file, headers included.
//!
//! The CPU sleeps until the codec's interrupt ([`sleep::until`]).

#![no_std]

pub mod header;
pub mod regs;
pub mod tables;

use core::ptr;

use exynos_4412_rs::drivers::jpeg::{self, PowerDomainTimeout};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{mmu, sleep, soc};

pub use header::{Header, HeaderError};

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
/// `exynos4_jpeg_sw_reset` (`jpeg-hw-exynos4.c:16`): how long the reset is held.
const RESET_US: u64 = 100;
/// How long a picture gets. Linux's watchdog is absent for this variant; the vendor's is 1 s
/// (`jpeg_v2x/jpeg_dev.h`, `JPEG_WATCHDOG_INTERVAL`).
const TIMEOUT_US: u64 = 1_000_000;
/// `jpeg-hw-s5p.h:17-20`: the sizes the driver accepts.
pub const MIN_SIZE: usize = 32;
pub const MAX_SIZE: usize = 8192;

/// Chroma subsampling of a JPEG stream, in the order of `enum v4l2_jpeg_chroma_subsampling`:
/// a raw picture's may not be coarser than its stream's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subsampling {
    S444,
    S422,
    S420,
    Gray,
}

impl Subsampling {
    /// As `DECODE_IMG_FMT` gives it: `exynos4x12_decoded_subsampling`, `jpeg-core.c:561`.
    pub fn from_decoded(value: u32) -> Self {
        match value & regs::DECODED_IMG_FMT_MASK {
            0 => Self::Gray,
            1 => Self::S444,
            2 => Self::S422,
            _ => Self::S420,
        }
    }

    fn enc_fmt(self) -> u32 {
        match self {
            Self::S444 => regs::ENC_FMT_YUV_444,
            Self::S422 => regs::ENC_FMT_YUV_422,
            Self::S420 => regs::ENC_FMT_YUV_420,
            Self::Gray => regs::ENC_FMT_GRAY,
        }
    }
}

/// Encoding quality: the index into Linux's tables (`jpeg-core.c:347`), 0 the smallest files,
/// 3 the best pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality(u8);

impl Quality {
    pub const LOWEST: Self = Self(0);
    pub const HIGHEST: Self = Self(3);

    /// `None` above 3.
    pub const fn new(level: u8) -> Option<Self> {
        if level <= 3 { Some(Self(level)) } else { None }
    }

    pub const fn level(self) -> u8 {
        self.0
    }
}

/// A raw picture's layout in memory: the formats the Exynos 4x12 entries of `sjpeg_formats`
/// (`jpeg-core.c:37`) list for both directions. Planes follow each other with no gaps, each
/// line as wide as the picture ([`Format::aligned`]): the codec takes no stride.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `V4L2_PIX_FMT_RGB32`: 32 bits a pixel.
    Rgb32,
    /// `V4L2_PIX_FMT_RGB565`.
    Rgb565,
    /// Packed 4:2:2, Y Cb Y Cr.
    Yuyv,
    /// Packed 4:2:2, Y Cr Y Cb.
    Yvyu,
    /// Y plane, then interleaved CbCr at half width, full height.
    Nv16,
    /// As [`Format::Nv16`], CrCb.
    Nv61,
    /// Y plane, then interleaved CbCr at half width and half height.
    Nv12,
    /// As [`Format::Nv12`], CrCb.
    Nv21,
    /// Y, Cb, Cr planes, the last two at half width and half height.
    Yuv420,
    /// Y plane, then interleaved CbCr at full size.
    Nv24,
    /// As [`Format::Nv24`], CrCb.
    Nv42,
    /// Y only.
    Grey,
}

/// Where a raw picture's planes are, from its start, and how many bytes it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub cb: Option<usize>,
    pub cr: Option<usize>,
    pub bytes: usize,
}

impl Format {
    /// The `IMG_FMT` bits: `__exynos4_jpeg_set_img_fmt`, `jpeg-hw-exynos4.c:50`.
    fn img_fmt(self) -> u32 {
        use regs::*;
        match self {
            Self::Grey => IMG_GRAY | GRAY_IP,
            Self::Rgb32 => IMG_RGB | RGB_IP_32BIT,
            Self::Rgb565 => IMG_RGB | RGB_IP_16BIT,
            Self::Nv24 => IMG_YUV_444 | YUV_444_IP_2P,
            Self::Nv42 => IMG_YUV_444 | YUV_444_IP_2P | SWAP_CHROMA_CRCB,
            Self::Yuyv => IMG_YUV_422 | YUV_422_IP_1P,
            Self::Yvyu => IMG_YUV_422 | YUV_422_IP_1P | SWAP_CHROMA_CRCB,
            Self::Nv16 => IMG_YUV_422 | YUV_422_IP_2P,
            Self::Nv61 => IMG_YUV_422 | YUV_422_IP_2P | SWAP_CHROMA_CRCB,
            Self::Nv12 => IMG_YUV_420 | YUV_420_IP_2P,
            Self::Nv21 => IMG_YUV_420 | YUV_420_IP_2P | SWAP_CHROMA_CRCB,
            Self::Yuv420 => IMG_YUV_420 | YUV_420_IP_3P,
        }
    }

    /// The format's own subsampling, `.subsampling` in `sjpeg_formats`; RGB counts as 4:4:4.
    pub fn subsampling(self) -> Subsampling {
        match self {
            Self::Rgb32 | Self::Rgb565 | Self::Nv24 | Self::Nv42 => Subsampling::S444,
            Self::Yuyv | Self::Yvyu | Self::Nv16 | Self::Nv61 => Subsampling::S422,
            Self::Nv12 | Self::Nv21 | Self::Yuv420 => Subsampling::S420,
            Self::Grey => Subsampling::Gray,
        }
    }

    fn is_rgb(self) -> bool {
        matches!(self, Self::Rgb32 | Self::Rgb565)
    }

    /// log2 of the width and height steps, `.h_align` and `.v_align` of the Exynos 4x12
    /// decoding entries in `sjpeg_formats`.
    fn align_shifts(self) -> (u32, u32) {
        match self {
            Self::Rgb32 | Self::Rgb565 | Self::Nv24 | Self::Nv42 | Self::Grey => (0, 0),
            Self::Yuyv | Self::Yvyu | Self::Nv16 | Self::Nv61 => (1, 0),
            Self::Nv12 | Self::Nv21 | Self::Yuv420 => (1, 1),
        }
    }

    /// `width` and `height` rounded up to the format's steps, as `jpeg_bound_align_image`
    /// (`jpeg-core.c:1392`) makes them for a picture of that size.
    pub fn aligned(self, width: usize, height: usize) -> (usize, usize) {
        let (h, v) = self.align_shifts();
        (width.next_multiple_of(1 << h), height.next_multiple_of(1 << v))
    }

    /// The planes of a `width` x `height` picture, already [`aligned`](Self::aligned):
    /// `exynos4_jpeg_set_img_addr`, `jpeg-core.c:2110`.
    pub fn layout(self, width: usize, height: usize) -> Layout {
        let pixels = width * height;
        let (cb, cr, bytes) = match self {
            Self::Rgb32 => (None, None, 4 * pixels),
            Self::Rgb565 | Self::Yuyv | Self::Yvyu => (None, None, 2 * pixels),
            Self::Nv16 | Self::Nv61 => (Some(pixels), None, 2 * pixels),
            Self::Nv12 | Self::Nv21 => (Some(pixels), None, pixels + pixels / 2),
            Self::Yuv420 => (Some(pixels), Some(pixels + pixels / 4), pixels + pixels / 2),
            Self::Nv24 | Self::Nv42 => (Some(pixels), None, 3 * pixels),
            Self::Grey => (None, None, pixels),
        };
        Layout { width, height, cb, cr, bytes }
    }
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The camera power domain did not turn on.
    Power(PowerDomainTimeout),
    /// The stream is not a baseline JPEG the codec takes.
    Header(HeaderError),
    /// Width or height outside [`MIN_SIZE`] to [`MAX_SIZE`], or, for encoding, not a multiple of
    /// the format's steps.
    BadSize { width: usize, height: usize },
    /// The raw format cannot hold the stream's subsampling (`s5p_jpeg_try_fmt_vid_cap`): its
    /// chroma is coarser, or the picture is 4:2:0 of odd width and the format 4:2:0 too, which
    /// the driver steers away from as it breaks luma. For encoding: grey out of a non-grey
    /// format (`s5p_jpeg_adjust_subs_ctrl`).
    Unsupported { format: Format, subsampling: Subsampling },
    /// A buffer does not start on a cache line, the caches being kept out of the codec's way
    /// line by line.
    Misaligned(usize),
    /// A buffer is smaller than the picture needs, rounded up to whole cache lines.
    TooSmall { need: usize, have: usize },
    /// The codec raised something other than completion: `INT_STATUS`, see `regs::INT_*`; what
    /// it held before the start; microseconds from the start.
    Codec { status: u32, stale: u32, us: u64 },
    /// No interrupt in time: `INT_STATUS` and `FIFO_STATUS`.
    Timeout(u32, u32),
    /// The codec says it wrote more than the output buffer holds.
    Overflow(usize),
}

/// A decoded picture.
#[derive(Clone, Copy, Debug)]
pub struct Decoded {
    /// The stream's header, as this crate read it.
    pub header: Header,
    /// Where the planes went in the output buffer.
    pub layout: Layout,
    /// The size the codec reports, `DECODE_XY_SIZE`.
    pub decoded_width: usize,
    pub decoded_height: usize,
    /// The subsampling the codec reports, `DECODE_IMG_FMT`.
    pub decoded_subsampling: Subsampling,
    /// From start to interrupt.
    pub us: u64,
}

/// A raw picture to encode, laid out as [`Format::layout`] says from the start of `data`.
#[derive(Clone, Copy, Debug)]
pub struct Picture<'a> {
    pub data: &'a [u8],
    pub format: Format,
    pub width: usize,
    pub height: usize,
}

/// An encoded picture.
#[derive(Clone, Copy, Debug)]
pub struct Encoded {
    /// Bytes of JPEG file at the start of the output buffer: `BITSTREAM_SIZE`, less the padding
    /// byte after the EOI of a file of odd length.
    pub len: usize,
    pub us: u64,
}

/// The codec, on.
pub struct Jpeg {
    mct: Mct,
    aclk160_hz: Option<u32>,
}

/// Bytes the output buffer of [`Jpeg::encode`] must hold for a `width` x `height` picture: the
/// codec is told no limit. Six bytes a pixel is above what baseline coding of 4:4:4 at the
/// finest of these tables can reach, plus a page for the headers, as Linux pads (`jpeg-core.c:1576`).
pub const fn encode_bound(width: usize, height: usize) -> usize {
    width * height * 6 + 4096
}

impl Jpeg {
    /// Turns the codec's power domain and gate on.
    pub fn init(mct: Mct) -> Result<Self, Error> {
        let aclk160_hz = jpeg::power_on(mct).map_err(Error::Power)?;
        let codec = Self { mct, aclk160_hz };
        codec.reset();
        Ok(codec)
    }

    /// The rate of `aclk160`, the codec's clock, if the HAL can tell it.
    pub fn aclk160_hz(&self) -> Option<u32> {
        self.aclk160_hz
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: every offset is one of the codec's registers, device memory mapped for the
        // whole program; `init` turned the domain and the gate on.
        unsafe { ptr::read_volatile((soc::JPEG_BASE + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: as in `read`.
        unsafe { ptr::write_volatile((soc::JPEG_BASE + offset) as *mut u32, value) }
    }

    fn modify(&self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }

    /// `exynos4_jpeg_sw_reset`, `jpeg-hw-exynos4.c:16`.
    fn reset(&self) {
        self.modify(regs::CNTL, regs::CNTL_MODE_MASK, 0);
        self.modify(regs::CNTL, regs::CNTL_SOFT_RESET_HI, 0);
        let end = self.mct.count() + RESET_US * COUNTS_PER_US;
        while self.mct.count() < end {}
        self.modify(regs::CNTL, 0, regs::CNTL_SOFT_RESET_HI);
    }

    /// `exynos4_jpeg_set_interrupt`, `:163`.
    fn enable_interrupts(&self) {
        self.modify(regs::INT_EN, regs::INT_EN_MASK, regs::INT_EN_MASK);
    }

    /// `exynos4_jpeg_set_frame_buf_address`, `:219`, with the planes of `layout` at `base`.
    fn set_planes(&self, base: usize, layout: &Layout) {
        self.write(regs::IMG_BA_PLANE_1, base as u32);
        self.write(regs::IMG_BA_PLANE_2, layout.cb.map_or(0, |cb| (base + cb) as u32));
        self.write(regs::IMG_BA_PLANE_3, layout.cr.map_or(0, |cr| (base + cr) as u32));
    }

    /// `exynos4_jpeg_set_table`, `jpeg-core.c:679`: bytes to registers, four a word, little end
    /// first.
    fn set_table(&self, offset: usize, table: &[u8]) {
        for (i, chunk) in table.chunks(4).enumerate() {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write(offset + 4 * i, u32::from_le_bytes(word));
        }
    }

    /// Sets the mode and lets the interrupt out, which starts the codec, then sleeps until it
    /// is done: the end of `exynos4_jpeg_device_run` (`jpeg-core.c:2265-2266`) and
    /// `exynos4_jpeg_irq` (`:2698`). Returns `INT_STATUS` and the time taken.
    fn run(&self, mode: u32) -> Result<u64, Error> {
        // Reading INT_STATUS clears it, as the board shows: one read when it is raised, kept, as
        // exynos4_jpeg_irq reads it once. This first one clears anything left over.
        let stale = self.read(regs::INT_STATUS);
        let begin = self.mct.count();
        self.modify(regs::CNTL, 0, regs::CNTL_SYS_INT_EN);
        self.modify(regs::CNTL, regs::CNTL_MODE_MASK, mode);
        let deadline = begin + TIMEOUT_US * COUNTS_PER_US;
        let mut status = 0;
        let done = sleep::until(self.mct, &[soc::JPEG_IRQ], Some(deadline), || {
            status = self.read(regs::INT_STATUS);
            status != 0
        });
        let us = (self.mct.count() - begin) / COUNTS_PER_US;
        let fifo = self.read(regs::FIFO_STATUS);
        self.modify(regs::CNTL, regs::CNTL_SYS_INT_EN, 0);
        if !done {
            self.modify(regs::CNTL, regs::CNTL_MODE_MASK, 0);
            self.reset();
            return Err(Error::Timeout(status, fifo));
        }
        if status & regs::INT_EN_MASK != regs::INT_IMG_COMPLETION {
            self.modify(regs::CNTL, regs::CNTL_MODE_MASK, 0);
            self.reset();
            return Err(Error::Codec { status, stale, us });
        }
        Ok(us)
    }

    /// Decodes the baseline JPEG `stream` into `out` as `format`. The picture lands at the start
    /// of `out`, its planes as [`Decoded::layout`] says. `out` must start on a cache line and
    /// hold the layout's bytes rounded up to whole lines; nothing past that is touched.
    ///
    /// The steps of `exynos4_jpeg_device_run`'s decoding branch, `jpeg-core.c:2233-2263`.
    pub fn decode(&mut self, stream: &[u8], format: Format, out: &mut [u8]) -> Result<Decoded, Error> {
        let header = header::parse(stream).map_err(Error::Header)?;
        check_size(header.width, header.height)?;
        // s5p_jpeg_try_fmt_vid_cap, jpeg-core.c:1473-1530.
        let chroma_too_coarse = !format.is_rgb() && format.subsampling() < header.subsampling;
        let odd_420 = header.subsampling == Subsampling::S420 && header.width % 2 == 1 && format.subsampling() == Subsampling::S420;
        if chroma_too_coarse || odd_420 {
            return Err(Error::Unsupported { format, subsampling: header.subsampling });
        }
        let (width, height) = format.aligned(header.width, header.height);
        let layout = format.layout(width, height);
        let lines = whole_lines(out, layout.bytes)?;

        mmu::clean_dcache_range(stream.as_ptr() as usize, stream.len());
        mmu::discard_dcache_range(out.as_ptr() as usize, lines);

        self.reset();
        self.enable_interrupts();
        self.set_planes(out.as_mut_ptr() as usize, &layout);
        self.write(regs::OUT_MEM_BASE, stream.as_ptr() as u32);
        self.write(regs::IMG_FMT, format.img_fmt());
        self.write(regs::BITSTREAM_SIZE, stream.len().div_ceil(32) as u32);
        let result = self.run(regs::CNTL_DEC_MODE);
        mmu::invalidate_dcache_range(out.as_ptr() as usize, lines);
        let us = result?;

        let size = self.read(regs::DECODE_XY_SIZE);
        let decoded = Decoded {
            header,
            layout,
            decoded_width: (size & regs::DECODED_SIZE_MASK) as usize,
            decoded_height: ((size >> 16) & regs::DECODED_SIZE_MASK) as usize,
            decoded_subsampling: Subsampling::from_decoded(self.read(regs::DECODE_IMG_FMT)),
            us,
        };
        self.modify(regs::CNTL, regs::CNTL_MODE_MASK, 0);
        Ok(decoded)
    }

    /// Encodes `picture` into a JPEG file at the start of `out`, with `subsampling` and
    /// `quality`. The subsampling is raised to the format's own if that is coarser, as
    /// `s5p_jpeg_adjust_subs_ctrl` does (`jpeg-core.c:1867`); grey takes [`Format::Grey`]. The
    /// width and height must be multiples of the format's steps ([`Format::aligned`]). `out`
    /// must start on a cache line and hold [`encode_bound`].
    ///
    /// The steps of `exynos4_jpeg_device_run`'s encoding branch, `jpeg-core.c:2199-2232`.
    pub fn encode(&mut self, picture: &Picture, subsampling: Subsampling, quality: Quality, out: &mut [u8]) -> Result<Encoded, Error> {
        let &Picture { data: raw, format, width, height } = picture;
        check_size(width, height)?;
        if format.aligned(width, height) != (width, height) {
            return Err(Error::BadSize { width, height });
        }
        if subsampling == Subsampling::Gray && format != Format::Grey {
            return Err(Error::Unsupported { format, subsampling });
        }
        let subsampling = if format.is_rgb() { subsampling } else { subsampling.max(format.subsampling()) };
        let layout = format.layout(width, height);
        if raw.len() < layout.bytes {
            return Err(Error::TooSmall { need: layout.bytes, have: raw.len() });
        }
        let lines = whole_lines(out, encode_bound(width, height))?;

        mmu::clean_dcache_range(raw.as_ptr() as usize, layout.bytes);
        mmu::discard_dcache_range(out.as_ptr() as usize, lines);

        self.reset();
        self.enable_interrupts();
        self.modify(regs::CNTL, 0, regs::CNTL_HUF_TBL_EN);
        // exynos4_jpeg_set_huff_tbl, jpeg-core.c:711.
        self.set_table(regs::HUFF_TBL_HDCLL, &tables::HDCTBL0);
        self.set_table(regs::HUFF_TBL_HDCCL, &tables::HDCTBL0);
        self.set_table(regs::HUFF_TBL_HDCLV, &tables::HDCTBLG0);
        self.set_table(regs::HUFF_TBL_HDCCV, &tables::HDCTBLG0);
        self.set_table(regs::HUFF_TBL_HACLL, &tables::HACTBL0);
        self.set_table(regs::HUFF_TBL_HACCL, &tables::HACTBL0);
        self.set_table(regs::HUFF_TBL_HACLV, &tables::HACTBLG0);
        self.set_table(regs::HUFF_TBL_HACCV, &tables::HACTBLG0);
        // Table 0 luma, table 1 chroma: jpeg-core.c:2206-2211.
        let level = usize::from(quality.level());
        self.set_table(regs::qtbl_content(0), &tables::QTBL_LUMINANCE[level]);
        self.set_table(regs::qtbl_content(1), &tables::QTBL_CHROMINANCE[level]);
        // exynos4_jpeg_set_encode_tbl_select, jpeg-hw-exynos4.c:233.
        self.write(
            regs::TBL_SEL,
            regs::q_tbl_comp(1, 0)
                | regs::q_tbl_comp(2, 1)
                | regs::q_tbl_comp(3, 1)
                | regs::huff_tbl_comp(1, 1)
                | regs::huff_tbl_comp(2, 0)
                | regs::huff_tbl_comp(3, 3),
        );
        self.write(regs::IMG_SIZE, 0);
        self.write(regs::IMG_SIZE, regs::x_size(width as u32) | regs::y_size(height as u32));
        self.write(regs::IMG_FMT, subsampling.enc_fmt() | format.img_fmt());
        self.set_planes(raw.as_ptr() as usize, &layout);
        self.write(regs::OUT_MEM_BASE, out.as_mut_ptr() as u32);
        // exynos4_jpeg_set_encode_hoff_cnt, jpeg-hw-exynos4.c:278.
        self.write(regs::HUFF_CNT, if format == Format::Grey { 0xD2 } else { 0x1A2 });
        let result = self.run(regs::CNTL_ENC_MODE);
        mmu::invalidate_dcache_range(out.as_ptr() as usize, lines);
        let us = result?;
        let mut len = self.read(regs::BITSTREAM_SIZE) as usize;
        self.modify(regs::CNTL, regs::CNTL_MODE_MASK, 0);
        if len > out.len() {
            return Err(Error::Overflow(len));
        }
        // On the board a file of odd length comes with a zero byte after its EOI, counted in
        // BITSTREAM_SIZE: the codec writes 16 bits at a time. That byte is not the file's.
        if len >= 3 && out[len - 3..len] == [0xFF, 0xD9, 0x00] {
            len -= 1;
        }
        Ok(Encoded { len, us })
    }
}

fn check_size(width: usize, height: usize) -> Result<(), Error> {
    if (MIN_SIZE..=MAX_SIZE).contains(&width) && (MIN_SIZE..=MAX_SIZE).contains(&height) {
        Ok(())
    } else {
        Err(Error::BadSize { width, height })
    }
}

/// `bytes` rounded up to whole cache lines, if `buffer` starts on one and holds that many.
fn whole_lines(buffer: &[u8], bytes: usize) -> Result<usize, Error> {
    let start = buffer.as_ptr() as usize;
    if !start.is_multiple_of(mmu::DCACHE_LINE) {
        return Err(Error::Misaligned(start));
    }
    let lines = bytes.next_multiple_of(mmu::DCACHE_LINE);
    if buffer.len() < lines {
        return Err(Error::TooSmall { need: lines, have: buffer.len() });
    }
    Ok(lines)
}
