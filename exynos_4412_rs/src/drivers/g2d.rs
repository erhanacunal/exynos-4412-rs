//! The 2D graphics accelerator at [`soc::G2D_BASE`], FIMG2D 4.1: fills rectangles, copies them
//! between images, scaled on the way if the sizes differ, and blends them over what is there by
//! their alpha, all in memory and without the CPU. One operation runs at a time; the CPU starts
//! it and later waits for its end ([`G2d::is_done`]), polled.
//!
//! Registers and fields: Linux 5.15.198 `drivers/gpu/drm/exynos/exynos_drm_g2d.c:30-112` has
//! the ones its command list checker needs; the rest come from the vendor kernel's map,
//! `willow` `drivers/media/video/samsung/fimg2d4x/fimg2d4x_regs.h`, cited by line. Which
//! registers each operation sets follows libdrm 2.4.131 `exynos/exynos_fimg2d.c` (MIT, see
//! `THIRD_PARTY_NOTICES.md`), cited by function. As there, a rectangle's right and bottom
//! edges are exclusive.
//!
//! The G2D reads and writes DRAM by physical address, past the CPU's caches: the images must
//! be uncached [`Framebuffer`]s, or cleaned before and invalidated after, as for any device.
//! Its System MMU stays off.

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::cmu::Cmu;
use crate::drivers::display::Framebuffer;
use crate::drivers::timer::Mct;
use crate::soc;

/// `sclk_fimg2d` here: 200 MHz, from the 800 MHz MPLL. The vendor kernel asks for 201 MHz
/// (`willow` `arch/arm/mach-exynos/mach-mehmet.c:2963`), which divides down to the same.
pub const DEFAULT_HZ: u32 = 200_000_000;

/// The largest width or height of an image or a rectangle, `fimg2d4x_regs.h:345-346`.
pub const MAX_SIZE: usize = 8000;

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

// fimg2d4x_regs.h:24-27, 34-37.
const SOFT_RESET: usize = 0x000;
const INTEN: usize = 0x004;
const INTC_PEND: usize = 0x00C;
const FIFO_STAT: usize = 0x010;
const BITBLT_START: usize = 0x100;
const BITBLT_COMMAND: usize = 0x104;
const BLEND_FUNCTION: usize = 0x108;
const ROUND_MODE: usize = 0x10C;
// fimg2d4x_regs.h:41-53: the source.
const SRC_SELECT: usize = 0x300;
const SRC_BASE_ADDR: usize = 0x304;
const SRC_STRIDE: usize = 0x308;
const SRC_COLOR_MODE: usize = 0x30C;
const SRC_LEFT_TOP: usize = 0x310;
const SRC_RIGHT_BOTTOM: usize = 0x314;
const SRC_REPEAT_MODE: usize = 0x31C;
const SRC_SCALE_CTRL: usize = 0x328;
const SRC_XSCALE: usize = 0x32C;
const SRC_YSCALE: usize = 0x330;
// fimg2d4x_regs.h:54-59: the destination.
const DST_SELECT: usize = 0x400;
const DST_BASE_ADDR: usize = 0x404;
const DST_STRIDE: usize = 0x408;
const DST_COLOR_MODE: usize = 0x40C;
const DST_LEFT_TOP: usize = 0x410;
const DST_RIGHT_BOTTOM: usize = 0x414;
// fimg2d4x_regs.h:80-85.
const ROP4: usize = 0x614;
const ALPHA: usize = 0x618;
const SF_COLOR: usize = 0x70C;

/// fimg2d4x_regs.h:135: clears every setting, as the vendor kernel does before each operation
/// (`fimg2d4x_reset`, its soft reset proper left out there).
const SOFT_RESET_SFR_CLEAR: u32 = 1 << 1;
/// fimg2d4x_regs.h:145,151: the end of an operation, enabled and pending.
const INT_BLIT: u32 = 1 << 0;
/// fimg2d4x_regs.h:196.
const START_BITBLT: u32 = 1 << 0;
// fimg2d4x_regs.h:200-208.
const COMMAND_SOLID_FILL: u32 = 1 << 28;
const COMMAND_SRC_PREMULT: u32 = 1 << 24;
const COMMAND_ALPHA_BLEND: u32 = 1 << 20;
// fimg2d4x_regs.h:232-240, with the coefficients of `fimg2d4x.h:119-131`: each colour of the
// result is src_coeff * Sc + dst_coeff * Dc.
const BLEND_INV_DST_COEFF: u32 = 1 << 18;
const BLEND_DST_COEFF_SHIFT: u32 = 8;
const COEFF_SRC_ALPHA: u32 = 2;
const COEFF_GLOBAL_ALPHA: u32 = 6;
/// The four 2-bit fields that say which alpha the coefficients take (`fimg2d4x_regs.h:235-238`),
/// each set to `ALPHA_PERPIXEL_MUL_GLOBAL`, the pixel's times the global (`fimg2d4x.h:83-86`).
const BLEND_ALPHA_MUL_GLOBAL: u32 = 2 << 4 | 2 << 6 | 2 << 12 | 2 << 14;
/// fimg2d4x_regs.h:245: premultiplication rounds as ((A + 1) * B) >> 8, the vendor kernel's
/// `premult_round_mode`.
const ROUND_PREMULT_1: u32 = 1 << 4;
/// fimg2d4x_regs.h:270,272: an image read from memory, or the background colour, unread.
const SELECT_MEMORY: u32 = 0;
const SELECT_BGCOLOR: u32 = 2;
/// fimg2d4x_regs.h:355: outside the source rectangle, its edge pixels.
const REPEAT_CLAMP: u32 = 3;
/// fimg2d4x_regs.h:373-374, and the mode the vendor kernel takes for bilinear scaling that
/// clamps, which it calls hidden: "ignore repeat mode at boundary" (`fimg2d4x_hw.c:405-408`).
const SCALE_NEAREST: u32 = 1;
const SCALE_BILINEAR_CLAMPED: u32 = 3;
/// The raster operation that copies the source (`G2D_ROP3_SRC`, `exynos_fimg2d.h:117`), as the
/// unmasked one, bits 0-7 of `ROP4`.
const ROP3_SRC: u32 = 0xCC;
/// A rectangle's corners: x in bits 0-12, y in 16-28 (`FIMG2D_OFFSET`, fimg2d4x_regs.h:17).
const COORD_MASK: u32 = 0x1FFF;
/// Strides are 16 bits (`FIMG2D_STRIDE`, fimg2d4x_regs.h:16).
const MAX_STRIDE: usize = 0xFFFF;

/// Whether an operation has started and not been waited for.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A rectangle is empty, larger than [`MAX_SIZE`] or not inside its image, or an image's
    /// stride is too short or too long.
    BadRect,
    /// The previous operation has not ended.
    Busy,
    /// The operation did not end in time. Holds `INTC_PEND`.
    Timeout(u32),
}

/// How pixels are stored (`fimg2d4x_regs.h:308-310`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `0x00RRGGBB` in 32 bits, the framebuffers' format: alpha is taken as opaque, and the G2D
    /// writes the top byte as 0xFF.
    Xrgb8888,
    /// `0xAARRGGBB` in 32 bits.
    Argb8888,
    /// `RRRRRGGGGGGBBBBB` in 16 bits.
    Rgb565,
}

impl Format {
    /// Bytes per pixel.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Xrgb8888 | Self::Argb8888 => 4,
            Self::Rgb565 => 2,
        }
    }

    /// The colour format field of `SRC_COLOR_MODE` and `DST_COLOR_MODE`, channels in the order
    /// A-R-G-B, the field's 0.
    const fn mode(self) -> u32 {
        match self {
            Self::Xrgb8888 => 0,
            Self::Argb8888 => 1,
            Self::Rgb565 => 2,
        }
    }
}

/// An image in memory: where it starts, as a physical address, how many bytes one line takes,
/// its size in pixels and its format.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    /// The top left pixel's physical address.
    pub address: usize,
    /// Bytes from one line to the next.
    pub stride: usize,
    /// Pixels in a line.
    pub width: usize,
    /// Lines.
    pub height: usize,
    /// How its pixels are stored.
    pub format: Format,
}

impl Surface {
    /// The whole of `framebuffer`, its pixels read as `format`, 32 bits.
    pub fn of(framebuffer: &Framebuffer, format: Format) -> Self {
        Self {
            address: framebuffer.address(),
            stride: framebuffer.stride(),
            width: framebuffer.width(),
            height: framebuffer.height(),
            format,
        }
    }

    /// `rect`'s corners for the `LEFT_TOP` and `RIGHT_BOTTOM` registers, if it is inside.
    fn corners(&self, rect: Rect) -> Result<(u32, u32), Error> {
        let fits = |start: usize, len: usize, limit: usize| {
            (1..=MAX_SIZE).contains(&len) && start.checked_add(len).is_some_and(|end| end <= limit)
        };
        let lines = self.width.checked_mul(self.format.bytes()).is_some_and(|bytes| bytes <= self.stride);
        if !fits(rect.x, rect.width, self.width.min(MAX_SIZE))
            || !fits(rect.y, rect.height, self.height.min(MAX_SIZE))
            || !lines
            || self.stride > MAX_STRIDE
        {
            return Err(Error::BadRect);
        }
        let point = |x: usize, y: usize| (y as u32 & COORD_MASK) << 16 | (x as u32 & COORD_MASK);
        Ok((point(rect.x, rect.y), point(rect.x + rect.width, rect.y + rect.height)))
    }
}

/// A rectangle of an image, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    /// Left column.
    pub x: usize,
    /// Top line.
    pub y: usize,
    /// Columns.
    pub width: usize,
    /// Lines.
    pub height: usize,
}

impl Rect {
    /// The rectangle of `width` by `height` pixels from (`x`, `y`).
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self { x, y, width, height }
    }

    /// The whole of `surface`.
    pub const fn all(surface: &Surface) -> Self {
        Self::new(0, 0, surface.width, surface.height)
    }
}

/// How a scaled copy picks its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// The nearest source pixel.
    Nearest,
    /// Between the four nearest, by distance; at the edges, the edge pixels.
    Bilinear,
}

/// What the colours of a source with alpha hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alpha {
    /// The colours as they are, to be multiplied by the alpha: `0x80FF0000` is red at half.
    Straight,
    /// The colours times the alpha already: half red is `0x80800000`.
    Premultiplied,
}

/// The G2D. Operations [`fill`](Self::fill), [`copy`](Self::copy) and [`blend`](Self::blend)
/// start it and return; [`wait`](Self::wait) waits for the end, before which nothing else may
/// start.
#[derive(Clone, Copy, Debug, Default)]
pub struct G2d;

impl G2d {
    /// The G2D.
    pub const fn new() -> Self {
        Self
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: every offset is a register of the G2D's 4 KiB, device memory that stays
        // mapped for the whole program; callers use it only after `power_on`.
        unsafe { ptr::read_volatile((soc::G2D_BASE + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: as in `read`.
        unsafe { ptr::write_volatile((soc::G2D_BASE + offset) as *mut u32, value) }
    }

    /// Opens the gates and runs `sclk_fimg2d` at the highest rate not above `hz`
    /// ([`DEFAULT_HZ`]). Returns the rate. The G2D has no power domain of its own on this SoC
    /// (`exynos4412.dtsi:298-305` names none).
    pub fn power_on(&self, hz: u32) -> u32 {
        let cmu = Cmu::new();
        cmu.open_g2d_gates();
        let rate = cmu.set_g2d_clock(hz);
        self.write(INTEN, 0);
        self.write(INTC_PEND, INT_BLIT);
        RUNNING.store(false, Ordering::Release);
        rate
    }

    /// Fills `rect` of `target` with `color`, in `target`'s format: `0x00RRGGBB` for
    /// [`Format::Xrgb8888`]. As `g2d_solid_fill`.
    pub fn fill(&self, target: &Surface, rect: Rect, color: u32) -> Result<(), Error> {
        let (left_top, right_bottom) = target.corners(rect)?;
        self.begin()?;
        self.set_target(target, SELECT_MEMORY, left_top, right_bottom);
        self.write(SF_COLOR, color);
        self.write(BITBLT_COMMAND, COMMAND_SOLID_FILL);
        self.run();
        Ok(())
    }

    /// Copies `from` of `source` to `to` of `target`, scaled with `filter` if their sizes
    /// differ, converted if their formats do. As `g2d_copy_with_scale`.
    pub fn copy(&self, source: &Surface, from: Rect, target: &Surface, to: Rect, filter: Filter) -> Result<(), Error> {
        let corners = (source.corners(from)?, target.corners(to)?);
        self.begin()?;
        // The target is not read: only the source goes into it.
        self.set_target(target, SELECT_BGCOLOR, corners.1.0, corners.1.1);
        self.set_source(source, from, to, filter, corners.0);
        self.write(ROP4, ROP3_SRC);
        self.run();
        Ok(())
    }

    /// Puts `from` of `source` over `to` of `target`, scaled as by [`copy`](Self::copy), by
    /// the source's alpha times `global` (255 for the source's alone): what is there shows
    /// through where the source is transparent. Porter-Duff's "over", as `g2d_scale_and_blend`
    /// does it with `G2D_OP_OVER`; for `global`, the coefficients of the vendor kernel's
    /// `ga_coeff_table` (`fimg2d4x_hw.c:706-710`). A target with alpha keeps its own.
    #[allow(clippy::too_many_arguments)]
    pub fn blend(
        &self,
        source: &Surface,
        from: Rect,
        alpha: Alpha,
        target: &Surface,
        to: Rect,
        filter: Filter,
        global: u8,
    ) -> Result<(), Error> {
        let corners = (source.corners(from)?, target.corners(to)?);
        self.begin()?;
        self.set_target(target, SELECT_MEMORY, corners.1.0, corners.1.1);
        self.set_source(source, from, to, filter, corners.0);
        let premultiply = if alpha == Alpha::Straight { COMMAND_SRC_PREMULT } else { 0 };
        self.write(BITBLT_COMMAND, COMMAND_ALPHA_BLEND | premultiply);
        // Result = Sc * 1 + Dc * (1 - Sa), or with the global alpha, Sc * Ga + Dc * (1 - Sa * Ga).
        let dst_coeff = COEFF_SRC_ALPHA << BLEND_DST_COEFF_SHIFT | BLEND_INV_DST_COEFF;
        let function = if global == u8::MAX {
            dst_coeff
        } else {
            // "if global alpha is 0x80, must set 0x80808080" (`fimg2d4x_enable_alpha`).
            self.write(ALPHA, u32::from(global) * 0x0101_0101);
            COEFF_GLOBAL_ALPHA | dst_coeff | BLEND_ALPHA_MUL_GLOBAL
        };
        self.write(BLEND_FUNCTION, function);
        self.write(ROUND_MODE, ROUND_PREMULT_1);
        self.run();
        Ok(())
    }

    /// Whether the operation started last has ended: its interrupt is pending, though nothing
    /// takes the interrupt (`fimg2d4x_is_blit_done`).
    pub fn is_done(&self) -> bool {
        self.read(INTC_PEND) & INT_BLIT != 0
    }

    /// Waits up to `timeout_us` for the end of the operation started last. Returns at once if
    /// none runs.
    pub fn wait(&self, mct: Mct, timeout_us: u64) -> Result<(), Error> {
        if !RUNNING.load(Ordering::Acquire) {
            return Ok(());
        }
        let begin = mct.count();
        while !self.is_done() {
            if mct.count() - begin > timeout_us * COUNTS_PER_US {
                return Err(Error::Timeout(self.read(INTC_PEND)));
            }
        }
        self.write(INTC_PEND, INT_BLIT);
        RUNNING.store(false, Ordering::Release);
        Ok(())
    }

    /// `INTC_PEND` and `FIFO_STAT`, for reports.
    pub fn status(&self) -> (u32, u32) {
        (self.read(INTC_PEND), self.read(FIFO_STAT))
    }

    /// Checks that nothing runs, and clears every setting of the last operation.
    fn begin(&self) -> Result<(), Error> {
        if RUNNING.load(Ordering::Acquire) {
            if !self.is_done() {
                return Err(Error::Busy);
            }
            RUNNING.store(false, Ordering::Release);
        }
        self.write(SOFT_RESET, SOFT_RESET_SFR_CLEAR);
        Ok(())
    }

    fn set_target(&self, target: &Surface, select: u32, left_top: u32, right_bottom: u32) {
        self.write(DST_SELECT, select);
        self.write(DST_BASE_ADDR, target.address as u32);
        self.write(DST_STRIDE, target.stride as u32);
        self.write(DST_COLOR_MODE, target.format.mode());
        self.write(DST_LEFT_TOP, left_top);
        self.write(DST_RIGHT_BOTTOM, right_bottom);
    }

    fn set_source(&self, source: &Surface, from: Rect, to: Rect, filter: Filter, corners: (u32, u32)) {
        self.write(SRC_SELECT, SELECT_MEMORY);
        self.write(SRC_BASE_ADDR, source.address as u32);
        self.write(SRC_STRIDE, source.stride as u32);
        self.write(SRC_COLOR_MODE, source.format.mode());
        self.write(SRC_LEFT_TOP, corners.0);
        self.write(SRC_RIGHT_BOTTOM, corners.1);
        self.write(SRC_REPEAT_MODE, REPEAT_CLAMP);
        if (from.width, from.height) != (to.width, to.height) {
            // Source pixels per target pixel, 16.16 (`g2d_get_scaling`).
            let ratio = |source: usize, target: usize| ((source << 16) / target) as u32;
            self.write(SRC_XSCALE, ratio(from.width, to.width));
            self.write(SRC_YSCALE, ratio(from.height, to.height));
            let mode = if filter == Filter::Nearest { SCALE_NEAREST } else { SCALE_BILINEAR_CLAMPED };
            self.write(SRC_SCALE_CTRL, mode);
        }
    }

    /// Starts what is set, the interrupt enabled for its pending bit only (`fimg2d4x_run`).
    fn run(&self) {
        self.write(INTEN, INT_BLIT);
        self.write(INTC_PEND, INT_BLIT);
        RUNNING.store(true, Ordering::Release);
        self.write(BITBLT_START, START_BITBLT);
    }
}
