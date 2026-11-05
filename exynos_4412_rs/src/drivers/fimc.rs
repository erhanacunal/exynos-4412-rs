//! The FIMCs at [`soc::FIMC_BASE`]: the camera interface's scaler, colour converter and rotator,
//! used here from memory to memory. A picture in two-plane YCbCr 4:2:0 (NV12), linear or in the
//! MFC's 64x32 tiles (NV12MT), goes in; it comes out as `0x00RRGGBB` pixels, scaled to any size
//! and turned by quarter turns, into a framebuffer, say. The FIMC works alone once started: the
//! CPU only waits for the end ([`Fimc::wait`]), asleep until the FIMC's interrupt.
//!
//! Registers and fields: Linux 5.15.198 `drivers/media/platform/exynos4-is/fimc-reg.h`, cited by
//! line. What each setting is for, and the scaler's ratios, follow what that driver programs
//! (`fimc-reg.c`, `fimc-core.c`), cited where it matters. The FIMCs of this SoC are the
//! `exynos4212-fimc` variant: `exynos4412.dtsi:690-722`, the main scaler's extended ratios, and
//! the output rotator in all but FIMC 3.

use core::ptr;

use crate::drivers::cmu::Cmu;
use crate::drivers::timer::Mct;
use crate::{sleep, soc};

/// `sclk_fimc` here: 160 MHz, from the 800 MHz MPLL. Linux asks for 166 MHz
/// (`fimc-core.c:1136`, `lclk_frequency` of `fimc_drvdata_exynos4x12`), which the MPLL divides down to the same.
pub const DEFAULT_HZ: u32 = 160_000_000;

// fimc-reg.h:16-17.
const CISRCFMT: usize = 0x00;
const CISRCFMT_ITU601_8BIT: u32 = 1 << 31;
// fimc-reg.h:35-60.
const CIGCTRL: usize = 0x08;
const CIGCTRL_SWRST: u32 = 1 << 31;
const CIGCTRL_IRQ_LEVEL: u32 = 1 << 20;
const CIGCTRL_IRQ_CLR: u32 = 1 << 19;
const CIGCTRL_IRQ_ENABLE: u32 = 1 << 16;
/// 0: the colour conversion of ITU-R BT.601, 1: BT.709.
const CIGCTRL_CSC_ITU709: u32 = 1 << 5;
// fimc-reg.h:72-74: output DMA start addresses, four of each.
const CIOYSA: usize = 0x18;
const CIOCBSA: usize = 0x28;
const CIOCRSA: usize = 0x38;
const OUT_ADDRESSES: usize = 4;
// fimc-reg.h:77-92.
const CITRGFMT: usize = 0x48;
const CITRGFMT_RGB: u32 = 3 << 29;
const CITRGFMT_FLIP_180: u32 = 3 << 14;
const CITRGFMT_OUTROT90: u32 = 1 << 13;
const CITRGFMT_HSIZE_SHIFT: u32 = 16;
// fimc-reg.h:95-111.
const CIOCTRL: usize = 0x4C;
const CIOCTRL_ORDER422_MASK: u32 = 3;
const CIOCTRL_YCBCR_PLANE_MASK: u32 = 1 << 3;
const CIOCTRL_ALPHA_OUT_MASK: u32 = 0xFF << 4;
const CIOCTRL_RGB16FMT_MASK: u32 = 3 << 16;
const CIOCTRL_ORDER2P_MASK: u32 = 3 << 24;
// fimc-reg.h:115-116.
const CISCPRERATIO: usize = 0x50;
const CISCPREDST: usize = 0x54;
// fimc-reg.h:120-144.
const CISCCTRL: usize = 0x58;
const CISCCTRL_SCALEUP_H: u32 = 1 << 30;
const CISCCTRL_SCALEUP_V: u32 = 1 << 29;
const CISCCTRL_CSCR2Y_WIDE: u32 = 1 << 28;
const CISCCTRL_CSCY2R_WIDE: u32 = 1 << 27;
const CISCCTRL_LCDPATHEN_FIFO: u32 = 1 << 26;
const CISCCTRL_SCALERSTART: u32 = 1 << 15;
const CISCCTRL_OUTRGB_FMT_RGB888: u32 = 2 << 11;
// fimc-reg.h:147.
const CITAREA: usize = 0x5C;
// fimc-reg.h:151-168.
const CISTATUS: usize = 0x64;
const CISTATUS_FRAME_END: u32 = 1 << 17;
// fimc-reg.h:173-177.
const CIIMGCPT: usize = 0xC0;
const CIIMGCPT_IMGCPTEN: u32 = 1 << 31;
const CIIMGCPT_IMGCPTEN_SC: u32 = 1 << 30;
const CIIMGCPT_CPT_FREN_ENABLE: u32 = 1 << 25;
// fimc-reg.h:183: image effects, all off.
const CIIMGEFF: usize = 0xD0;
// fimc-reg.h:197-204: input DMA start addresses, and the real size.
const CIIYSA: usize = 0xD4;
const CIICBSA: usize = 0xD8;
const CIICRSA: usize = 0xDC;
const CIREAL_ISIZE: usize = 0xF8;
const CIREAL_ISIZE_AUTOLOAD_EN: u32 = 1 << 31;
const CIREAL_ISIZE_ADDR_CH_DIS: u32 = 1 << 30;
// fimc-reg.h:207-236.
const MSCTRL: usize = 0xFC;
const MSCTRL_IN_BURST_COUNT_SHIFT: u32 = 24;
const MSCTRL_IN_BURST_COUNT_MASK: u32 = 0xF << 24;
const MSCTRL_2P_IN_ORDER_MASK: u32 = 3 << 16;
const MSCTRL_C_INT_IN_2PLANE: u32 = 1 << 15;
const MSCTRL_FLIP_MASK: u32 = 3 << 13;
const MSCTRL_FIFO_CTRL_FULL: u32 = 1 << 12;
const MSCTRL_ORDER422_MASK: u32 = 3 << 4;
const MSCTRL_INPUT_MEMORY: u32 = 1 << 3;
const MSCTRL_INFORMAT_MASK: u32 = 3 << 1;
const MSCTRL_INFORMAT_YCBCR420: u32 = 0;
const MSCTRL_ENVID: u32 = 1 << 0;
// fimc-reg.h:239-252.
const CIOYOFF: usize = 0x168;
const CIOCBOFF: usize = 0x16C;
const CIOCROFF: usize = 0x170;
const CIIYOFF: usize = 0x174;
const CIICBOFF: usize = 0x178;
const CIICROFF: usize = 0x17C;
const ORGISIZE: usize = 0x180;
const ORGOSIZE: usize = 0x184;
// fimc-reg.h:255.
const CIEXTEN: usize = 0x188;
// fimc-reg.h:261-265.
const CIDMAPARAM: usize = 0x18C;
const CIDMAPARAM_R_64X32: u32 = 3 << 29;
const CIDMAPARAM_TILE_MASK: u32 = 3 << 29 | 3 << 13;
// fimc-reg.h:278.
const CIFCNTSEQ: usize = 0x1FC;

/// The power domain's status register reads 7 when on, as the others of the PMU.
const DOMAIN_ON: u32 = 7;
const DOMAIN_STATUS: usize = 0x04;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const DOMAIN_TIMEOUT_US: u64 = 10_000;
/// `fimc_hw_reset` (`fimc-reg.c:31`): the reset is held this long.
const RESET_US: u64 = 10;

/// `exynos4412.dtsi:692`, `samsung,pix-limits`: the widest source the scaler takes, and the
/// widest picture the output rotator turns (its width before turning).
const SCALER_MAX_WIDTH: usize = 4224;
const ROTATOR_MAX_WIDTH: usize = 1920;
/// The largest a picture can be in each direction: 12-bit size fields.
const MAX_SIZE: usize = 4095;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The camera power domain did not turn on: its last status.
    PowerDomain(u32),
    /// A size out of what the FIMC takes, a scale too large (64 times or more down), or a
    /// rotation FIMC 3 cannot do.
    BadSize,
    /// The picture did not end in time: `CISTATUS`.
    Timeout(u32),
}

/// Turns, clockwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    /// As it is.
    None,
    /// A quarter turn: the top row becomes the right column.
    Quarter,
    /// Upside down.
    Half,
    /// Three quarter turns: the top row becomes the left column.
    ThreeQuarters,
}

impl Rotation {
    const fn turns_sideways(self) -> bool {
        matches!(self, Self::Quarter | Self::ThreeQuarters)
    }
}

/// The colour conversion's equation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSpace {
    /// ITU-R BT.601, for standard definition.
    Bt601,
    /// ITU-R BT.709, for high definition. Linux chooses it for an output 1280 or more wide
    /// (`fimc-reg.c:158`).
    Bt709,
}

/// The source picture: two planes, luma and interleaved CbCr (Cb first), 4:2:0.
#[derive(Clone, Copy, Debug)]
pub struct Source {
    /// Physical address of the luma plane. Only [`Fimc::start`]'s counts.
    pub luma: usize,
    /// Physical address of the CbCr plane. Only [`Fimc::start`]'s counts.
    pub chroma: usize,
    /// In 64x32 tiles (NV12MT), as the MFC writes, or linear.
    pub tiled: bool,
    /// The planes' width in pixels.
    pub buf_width: usize,
    /// The luma plane's rows.
    pub buf_height: usize,
    /// The picture's width, from the planes' left edge.
    pub width: usize,
    /// The picture's height, from the planes' top.
    pub height: usize,
    /// Samples from 0 to 255 (`true`), not 16 to 235 (`false`, the video range).
    pub full_range: bool,
    /// The equation the samples are for.
    pub color_space: ColorSpace,
}

/// Where the picture goes: `0x00RRGGBB` pixels, `line_pixels` from one line to the next.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// Physical address of the picture's top left pixel. Only [`Fimc::start`]'s counts.
    pub address: usize,
    /// Pixels from the start of one line to the next.
    pub line_pixels: usize,
    /// The picture's width as it lands, turned.
    pub width: usize,
    /// The picture's height as it lands, turned.
    pub height: usize,
}

/// One of the four FIMCs.
#[derive(Clone, Copy, Debug)]
pub struct Fimc {
    index: usize,
}

/// Raw status of the camera power domain.
pub fn power_domain_status() -> u32 {
    // SAFETY: this register of the PMU is device memory that stays mapped for the whole program.
    unsafe { ptr::read_volatile((soc::PD_CAM_BASE + DOMAIN_STATUS) as *const u32) }
}

/// The ratio and its shift for the pre-scaler, `fimc_get_scaler_factor` (`fimc-core.c:207`): the
/// largest power of two up to 32 by which `source` is at least `target` times larger, 1 when
/// scaling up. `None` from 64 times down.
fn prescale(source: usize, target: usize) -> Option<(u32, u32)> {
    if source >= 64 * target {
        return None;
    }
    Some((0..6).rev().find(|&shift| source >= target << shift).map_or((1, 0), |shift| (1 << shift, shift as u32)))
}

impl Fimc {
    /// FIMC `index`, 0 to 3.
    ///
    /// # Panics
    ///
    /// If `index` is above 3.
    pub const fn new(index: usize) -> Self {
        assert!(index < 4, "there are four FIMCs");
        Self { index }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: every offset is a register of this FIMC's 4 KiB, device memory that stays
        // mapped for the whole program; callers use the FIMC only after `power_on`.
        unsafe { ptr::read_volatile((soc::FIMC_BASE[self.index] + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: as in `read`.
        unsafe { ptr::write_volatile((soc::FIMC_BASE[self.index] + offset) as *mut u32, value) }
    }

    fn modify(&self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }

    /// Turns the camera power domain on if it is off, opens the gates, runs `sclk_fimc` at the
    /// highest rate not above `hz` ([`DEFAULT_HZ`]) and resets the FIMC. Returns the rate.
    pub fn power_on(&self, hz: u32, mct: Mct) -> Result<u32, Error> {
        if power_domain_status() & DOMAIN_ON != DOMAIN_ON {
            // SAFETY: as in `power_domain_status`, the configuration register before it. Nothing
            // uses this domain while it is off.
            unsafe { ptr::write_volatile(soc::PD_CAM_BASE as *mut u32, DOMAIN_ON) };
            let begin = mct.count();
            while power_domain_status() & DOMAIN_ON != DOMAIN_ON {
                if mct.count() - begin > DOMAIN_TIMEOUT_US * COUNTS_PER_US {
                    return Err(Error::PowerDomain(power_domain_status()));
                }
            }
        }
        let cmu = Cmu::new();
        cmu.open_fimc_gates(self.index);
        let rate = cmu.set_fimc_clock(self.index, hz);
        self.reset(mct);
        Ok(rate)
    }

    /// The software reset, as `fimc_hw_reset` does it (`fimc-reg.c:19`), with the interrupt
    /// line set to level and on, though nothing here takes it: polling reads `CISTATUS`.
    pub fn reset(&self, mct: Mct) {
        self.modify(CISRCFMT, 0, CISRCFMT_ITU601_8BIT);
        self.modify(CIGCTRL, 0, CIGCTRL_SWRST | CIGCTRL_IRQ_LEVEL);
        let end = mct.count() + RESET_US * COUNTS_PER_US;
        while mct.count() < end {}
        self.modify(CIGCTRL, CIGCTRL_SWRST, CIGCTRL_IRQ_ENABLE);
        // Output DMA through the first four address registers only, all set alike.
        self.write(CIFCNTSEQ, (1 << OUT_ADDRESSES) - 1);
    }

    /// Sets the FIMC up for pictures of `source`'s format and size into `target`'s, turned by
    /// `rotation`, in the order of `fimc_device_run` (`fimc-m2m.c`). The addresses come with
    /// each [`start`](Self::start).
    pub fn configure(&self, source: &Source, target: &Target, rotation: Rotation) -> Result<(), Error> {
        let (sx, sy) = (source.width, source.height);
        // The scaler's target is the picture before it is turned.
        let (tx, ty) = if rotation.turns_sideways() { (target.height, target.width) } else { (target.width, target.height) };
        let sizes = [sx, sy, tx, ty, source.buf_width, source.buf_height, target.line_pixels];
        if sizes.iter().any(|size| !(16..=MAX_SIZE).contains(size))
            || sx > source.buf_width
            || sy > source.buf_height
            || sx > SCALER_MAX_WIDTH
            || target.width > target.line_pixels
            || sx % 2 != 0
            || sy % 2 != 0
        {
            return Err(Error::BadSize);
        }
        if rotation.turns_sideways() && (tx > ROTATOR_MAX_WIDTH || self.index == 3) {
            return Err(Error::BadSize);
        }
        let ((pre_h, h_shift), (pre_v, v_shift)) = match (prescale(sx, tx), prescale(sy, ty)) {
            (Some(h), Some(v)) => (h, v),
            _ => return Err(Error::BadSize),
        };

        // fimc_hw_set_input_path, fimc_hw_set_in_dma: from memory, one picture, 4:2:0 in two
        // planes, CbCr (fimc_set_yuv_order: the one order there is), no offset into the planes.
        self.write(CIIYOFF, 0);
        self.write(CIICBOFF, 0);
        self.write(CIICROFF, 0);
        self.write(ORGISIZE, (source.buf_height as u32) << 16 | source.buf_width as u32);
        self.write(CIREAL_ISIZE, (sy as u32) << 16 | sx as u32);
        self.modify(ORGISIZE, CIREAL_ISIZE_AUTOLOAD_EN, 0);
        self.modify(
            MSCTRL,
            MSCTRL_INFORMAT_MASK
                | MSCTRL_IN_BURST_COUNT_MASK
                | MSCTRL_INPUT_MEMORY
                | MSCTRL_C_INT_IN_2PLANE
                | MSCTRL_2P_IN_ORDER_MASK
                | MSCTRL_ORDER422_MASK
                | MSCTRL_FLIP_MASK
                | MSCTRL_ENVID,
            4 << MSCTRL_IN_BURST_COUNT_SHIFT | MSCTRL_INPUT_MEMORY | MSCTRL_FIFO_CTRL_FULL | MSCTRL_INFORMAT_YCBCR420 | MSCTRL_C_INT_IN_2PLANE,
        );
        self.modify(CIDMAPARAM, CIDMAPARAM_TILE_MASK, if source.tiled { CIDMAPARAM_R_64X32 } else { 0 });

        // fimc_set_scaler_info, fimc_hw_set_prescaler, fimc_hw_set_mainscaler (the extended
        // ratios, 14 bits of fraction, split over CISCCTRL and CIEXTEN).
        let main_h = ((sx as u32) << 14) / ((tx as u32) << h_shift);
        let main_v = ((sy as u32) << 14) / ((ty as u32) << v_shift);
        self.write(CISCPRERATIO, (10 - (h_shift + v_shift)) << 28 | pre_h << 16 | pre_v);
        self.write(CISCPREDST, ((sx as u32 / pre_h) << 16) | (sy as u32 / pre_v));
        let mut scaler = CISCCTRL_OUTRGB_FMT_RGB888 | (main_h >> 6) << 16 | (main_v >> 6);
        if tx >= sx {
            scaler |= CISCCTRL_SCALEUP_H;
        }
        if ty >= sy {
            scaler |= CISCCTRL_SCALEUP_V;
        }
        if source.full_range {
            scaler |= CISCCTRL_CSCR2Y_WIDE | CISCCTRL_CSCY2R_WIDE;
        }
        // Not started, to the output DMA rather than the FIMD's FIFO (fimc_hw_set_output_path).
        self.write(CISCCTRL, scaler);
        self.write(CIEXTEN, (main_h & 0x3F) << 10 | (main_v & 0x3F));

        // fimc_hw_set_target_format, fimc_hw_set_rotation: RGB, the size before turning, the
        // output rotator (FIMC_IO_DMA), a half turn as a flip of both axes.
        let turn = match rotation {
            Rotation::None => 0,
            Rotation::Quarter => CITRGFMT_OUTROT90,
            Rotation::Half => CITRGFMT_FLIP_180,
            Rotation::ThreeQuarters => CITRGFMT_OUTROT90 | CITRGFMT_FLIP_180,
        };
        self.write(CITRGFMT, CITRGFMT_RGB | turn | (tx as u32) << CITRGFMT_HSIZE_SHIFT | ty as u32);
        self.write(CITAREA, (tx * ty) as u32);
        self.write(CIIMGEFF, 0);

        // fimc_hw_set_out_dma: no offset, the target's lines `line_pixels` apart, one plane of
        // RGB with alpha 0, the equation.
        self.write(CIOYOFF, 0);
        self.write(CIOCBOFF, 0);
        self.write(CIOCROFF, 0);
        self.write(ORGOSIZE, (target.height as u32) << 16 | target.line_pixels as u32);
        let csc = if source.color_space == ColorSpace::Bt709 { CIGCTRL_CSC_ITU709 } else { 0 };
        self.modify(CIGCTRL, CIGCTRL_CSC_ITU709, csc);
        self.modify(
            CIOCTRL,
            CIOCTRL_ORDER2P_MASK | CIOCTRL_ORDER422_MASK | CIOCTRL_YCBCR_PLANE_MASK | CIOCTRL_RGB16FMT_MASK | CIOCTRL_ALPHA_OUT_MASK,
            0,
        );
        self.modify(CISCCTRL, CISCCTRL_LCDPATHEN_FIFO, 0);
        Ok(())
    }

    /// Starts the FIMC on the picture at `luma` and `chroma` into `target` (physical addresses),
    /// as [`configure`](Self::configure)d: `fimc_hw_set_input_addr`, `fimc_hw_set_output_addr`,
    /// `fimc_activate_capture`, `fimc_hw_activate_input_dma`. The CPU must not write either
    /// meanwhile, and what it wrote to the source must be in DRAM.
    pub fn start(&self, luma: usize, chroma: usize, target: usize) {
        self.modify(MSCTRL, MSCTRL_ENVID, 0);
        self.modify(CISTATUS, CISTATUS_FRAME_END, 0);
        self.modify(CIGCTRL, 0, CIGCTRL_IRQ_CLR);
        self.modify(CIREAL_ISIZE, 0, CIREAL_ISIZE_ADDR_CH_DIS);
        self.write(CIIYSA, luma as u32);
        self.write(CIICBSA, chroma as u32);
        self.write(CIICRSA, 0);
        self.modify(CIREAL_ISIZE, CIREAL_ISIZE_ADDR_CH_DIS, 0);
        for i in 0..OUT_ADDRESSES {
            self.write(CIOYSA + 4 * i, target as u32);
            self.write(CIOCBSA + 4 * i, 0);
            self.write(CIOCRSA + 4 * i, 0);
        }
        self.modify(CISCCTRL, 0, CISCCTRL_SCALERSTART);
        self.modify(CIIMGCPT, 0, CIIMGCPT_CPT_FREN_ENABLE | CIIMGCPT_IMGCPTEN_SC | CIIMGCPT_IMGCPTEN);
        self.modify(MSCTRL, 0, MSCTRL_ENVID);
    }

    /// Whether the picture [`start`](Self::start)ed has been written out: `CISTATUS`'s frame end,
    /// as the vendor kernel polls it (`fimc_hwget_frame_end`).
    pub fn is_done(&self) -> bool {
        self.read(CISTATUS) & CISTATUS_FRAME_END != 0
    }

    /// Waits up to `timeout_us` for the picture's end, asleep until the FIMC's interrupt
    /// ([`sleep::until`]), then stops the input DMA for the next [`start`](Self::start).
    pub fn wait(&self, mct: Mct, timeout_us: u64) -> Result<(), Error> {
        let deadline = mct.count() + timeout_us * COUNTS_PER_US;
        if !sleep::until(mct, &[soc::FIMC_IRQ[self.index]], Some(deadline), || self.is_done()) {
            return Err(Error::Timeout(self.status()));
        }
        self.modify(CISTATUS, CISTATUS_FRAME_END, 0);
        self.modify(CIGCTRL, 0, CIGCTRL_IRQ_CLR);
        self.modify(MSCTRL, MSCTRL_ENVID, 0);
        Ok(())
    }

    /// `CISTATUS`, for reports.
    pub fn status(&self) -> u32 {
        self.read(CISTATUS)
    }
}
