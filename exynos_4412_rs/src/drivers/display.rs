//! FIMD display controller, the board's panel and a framebuffer to draw in.
//!
//! This drives one full-screen window in 32-bit XRGB format, with the output going straight
//! to the RGB pins. Waits for a vsync sleep until the frame interrupt, through the interrupt
//! combiner ([`sleep::until`]), or poll if the MCT is not running. Not here: the other windows
//! and the System MMU (off, so the FIMD reads physical addresses). The backlight's brightness
//! is a PWM: [`set_brightness`].
//!
//! Register offsets and fields are verified against Linux 5.15.198
//! `include/video/samsung_fimd.h`, cited as `samsung_fimd.h:<line>`. `exynos_drm_fimd.c` is
//! `drivers/gpu/drm/exynos/exynos_drm_fimd.c` and `pm_domains.c` is
//! `drivers/soc/samsung/pm_domains.c`, of the same tree. `willow` is the board's vendor kernel,
//! 3.0.101.
//!
//! # Order
//!
//! Nothing in the FIMD may be touched while its power domain is off or its register clock is
//! closed: [`Fimd::is_accessible`] tells. [`Fimd::power_on`], then [`Fimd::start`], make it run.
//!
//! # Concurrency
//!
//! One core at a time. The system register update masks IRQs on the calling core.

use core::ptr;
use core::sync::atomic::{AtomicU8, Ordering};

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::register_structs;
use tock_registers::registers::{ReadOnly, ReadWrite};

use super::cmu::Cmu;
use super::gpio::{self, Drive, Function, Pin, Pull};
use super::pwm;
use super::timer::Mct;
use crate::{cpu, mmu, sleep, soc};

register_structs! {
    Registers {
        /// samsung_fimd.h:18.
        (0x00 => vidcon0: ReadWrite<u32>),
        /// samsung_fimd.h:73.
        (0x04 => vidcon1: ReadWrite<u32>),
        (0x08 => _reserved0),
        /// samsung_fimd.h:117.
        (0x10 => vidtcon0: ReadWrite<u32>),
        /// samsung_fimd.h:140.
        (0x14 => vidtcon1: ReadWrite<u32>),
        /// samsung_fimd.h:161.
        (0x18 => vidtcon2: ReadWrite<u32>),
        (0x1C => _reserved1),
        /// samsung_fimd.h:176 `WINCON(win)`.
        (0x20 => wincon: [ReadWrite<u32>; WINDOWS]),
        /// samsung_fimd.h:234.
        (0x34 => shadowcon: ReadWrite<u32>),
        (0x38 => _reserved2),
        /// samsung_fimd.h:243 `VIDOSD_BASE`, exynos_drm_fimd.c:43 `VIDOSD_A(0)`.
        (0x40 => vidosd0a: ReadWrite<u32>),
        /// exynos_drm_fimd.c:44 `VIDOSD_B(0)`.
        (0x44 => vidosd0b: ReadWrite<u32>),
        /// exynos_drm_fimd.c:46-49 `VIDOSD_C(0)`: the size in pixels, on window 0.
        (0x48 => vidosd0c: ReadWrite<u32>),
        (0x4C => _reserved3),
        /// samsung_fimd.h:297 `VIDW_BUF_START(0)`.
        (0xA0 => buf_start: ReadWrite<u32>),
        (0xA4 => _reserved4),
        /// samsung_fimd.h:300 `VIDW_BUF_END(0)`.
        (0xD0 => buf_end: ReadWrite<u32>),
        (0xD4 => _reserved5),
        /// samsung_fimd.h:302 `VIDW_BUF_SIZE(0)`.
        (0x100 => buf_size: ReadWrite<u32>),
        (0x104 => _reserved6),
        /// samsung_fimd.h:318.
        (0x130 => vidintcon0: ReadWrite<u32>),
        /// samsung_fimd.h:363: pending interrupts, cleared by writing 1.
        (0x134 => vidintcon1: ReadWrite<u32>),
        (0x138 => _reserved6b),
        /// samsung_fimd.h:406 `WINxMAP(0)`.
        (0x180 => win0map: ReadWrite<u32>),
        (0x184 => _reserved7),
        /// samsung_fimd.h:298 `VIDW_BUF_START_S(0)`: the address in use, from the shadow register.
        (0x40A0 => buf_start_shadow: ReadOnly<u32>),
        (0x40A4 => @END),
    }
}

/// exynos_drm_fimd.c:90 `WINDOWS_NR`.
const WINDOWS: usize = 5;

/// samsung_fimd.h:60 `VIDCON0_CLKVAL_F`: this field holds the divider minus 1.
const VIDCON0_CLKVAL_SHIFT: u32 = 6;
/// samsung_fimd.h:59 `VIDCON0_CLKVAL_F_LIMIT`, plus 1.
const MAX_DIVIDER: u32 = 256;
/// samsung_fimd.h:61. The vendor kernel sets it, `VCLKEN_FREERUN`:
/// `willow` `drivers/video/samsung/s3cfb_fimd6x.c:203`.
const VIDCON0_VCLK_FREE: u32 = 1 << 5;
/// samsung_fimd.h:62.
const VIDCON0_CLKDIR: u32 = 1 << 4;
/// samsung_fimd.h:70.
const VIDCON0_ENVID: u32 = 1 << 1;
/// samsung_fimd.h:71.
const VIDCON0_ENVID_F: u32 = 1 << 0;

/// samsung_fimd.h:76 `VIDCON1_LINECNT_GET`.
const VIDCON1_LINECNT_SHIFT: u32 = 16;
const VIDCON1_LINECNT_MASK: u32 = 0x7FF;
/// samsung_fimd.h:79 `VIDCON1_VSTATUS_SHIFT`.
const VIDCON1_VSTATUS_SHIFT: u32 = 13;
/// samsung_fimd.h:80 `VIDCON1_VSTATUS_VSYNC`, as the field's value.
const VSTATUS_VSYNC: u32 = 0;
/// samsung_fimd.h:328-342,361: the frame interrupt, at the start of vsync, and the interrupts'
/// enable. The video interrupt at `FRAMESEL1` stays off.
const VIDINTCON0_FRAMESEL0_MASK: u32 = 3 << 15;
const VIDINTCON0_FRAMESEL0_VSYNC: u32 = 1 << 15;
const VIDINTCON0_FRAMESEL1_MASK: u32 = 3 << 13;
const VIDINTCON0_INT_FRAME: u32 = 1 << 12;
const VIDINTCON0_INT_ENABLE: u32 = 1 << 0;
/// samsung_fimd.h:365.
const VIDINTCON1_INT_FRAME: u32 = 1 << 1;
/// The FIMD's `vsync` source in its combiner group (`exynos4.dtsi:709-710`), the one Linux
/// takes for the frame interrupt (`exynos_drm_fimd.c`, `fimd_probe`).
const COMBINER_VSYNC_SOURCE: usize = 1;
/// Linux `drivers/irqchip/exynos-combiner.c:21,61,194`: each four groups share registers at
/// `(group / 4) * 0x10`, a byte per group, a bit per source.
const COMBINER_ENABLE_SET: usize = 0x0;
/// How long a wait for the next frame may take, in microseconds: six frames.
const FRAME_TIMEOUT_US: u64 = 100_000;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

/// The VCLK keeps running during blanking, as the vendor kernel has it: `willow`
/// `arch/arm/plat-s5p/include/plat/regs-fb-s5p.h:196` `VCLK_RUN_VDEN_DIS`, used by
/// `drivers/video/samsung/s3cfb_fimd6x.c:267`. samsung_fimd.h:84 is the same field, without
/// this value.
const VIDCON1_VCLK_RUN_VDEN_DIS: u32 = 3 << 9;
/// samsung_fimd.h:88.
const VIDCON1_INV_VCLK: u32 = 1 << 7;
/// samsung_fimd.h:89.
const VIDCON1_INV_HSYNC: u32 = 1 << 6;
/// samsung_fimd.h:90.
const VIDCON1_INV_VSYNC: u32 = 1 << 5;
/// samsung_fimd.h:91.
const VIDCON1_INV_VDEN: u32 = 1 << 4;

/// samsung_fimd.h:126,131 `VIDTCON0_VBPD`, `VIDTCON0_VFPD`, and the same in `VIDTCON1`,
/// samsung_fimd.h:149,154. The sync width is the field at bit 0.
const TCON_BACK_PORCH_SHIFT: u32 = 16;
const TCON_FRONT_PORCH_SHIFT: u32 = 8;
/// samsung_fimd.h:166 `VIDTCON2_LINEVAL`, and samsung_fimd.h:248,260 the same place for the
/// X coordinate of `VIDOSDxA` and `VIDOSDxB`.
const SIZE_HIGH_SHIFT: u32 = 11;
/// samsung_fimd.h:165,171 `VIDTCON2_*_LIMIT`, plus 1. This module does not use the extension
/// bits `_E`, which a 2048-pixel limit makes unnecessary.
const MAX_SIZE: usize = 0x800;

/// samsung_fimd.h:197.
const WINCON_ENWIN: u32 = 1 << 0;
/// samsung_fimd.h:209 `WINCON0_BPPMODE_24BPP_888`.
const WINCON_BPP_888: u32 = 0xB << 2;
/// samsung_fimd.h:190. Set for this pixel format by exynos_drm_fimd.c:686-688, XRGB8888. The
/// vendor kernel sets it too, `FB_SWAP_WORD`: `willow` `drivers/video/samsung/s3cfb_fimd6x.c:622`.
const WINCON_WSWP: u32 = 1 << 15;
/// samsung_fimd.h:194 `WINCONx_BURSTLEN_16WORD`.
const WINCON_BURST_16: u32 = 0 << 9;

/// samsung_fimd.h:237 `SHADOWCON_CHx_ENABLE(0)`.
const SHADOWCON_CH0: u32 = 1 << 0;
/// samsung_fimd.h:235 `SHADOWCON_WINx_PROTECT(0)`.
const SHADOWCON_PROTECT0: u32 = 1 << 10;

/// samsung_fimd.h:308 `VIDW_BUF_SIZE_OFFSET`: bytes between lines.
const BUF_SIZE_OFFSET_SHIFT: u32 = 13;

/// samsung_fimd.h:407 `WINxMAP_MAP`.
const WINMAP_ENABLE: u32 = 1 << 24;

/// Offset of `LCDBLKC_CFG` in the system registers: exynos_drm_fimd.c:136 `lcdblk_offset`, and
/// the same in `willow` `arch/arm/mach-exynos/setup-fimd0.c:70-72`.
const LCDBLK_CFG: usize = 0x210;
/// exynos_drm_fimd.c:138 `lcdblk_bypass_shift`: the FIMD output skips the image enhancer.
const LCDBLK_FIMD_BYPASS: u32 = 1 << 1;

/// What to write to the domain to turn it on, and what its status reads then: pm_domains.c:46-52,
/// with `exynos4210_cfg` (pm_domains.c:76-78) for `samsung,exynos4210-pd` (pm_domains.c:86-87).
const DOMAIN_ON: u32 = 0x7;
/// pm_domains.c:52, status at `base + 4`.
const DOMAIN_STATUS: usize = 4;

/// Turns a stuck power domain or a stopped scan into an error instead of a hang.
const POLL_LIMIT: u32 = 10_000_000;

/// Power-down control of the board's LVDS transmitter, active low. `willow`
/// `arch/arm/mach-exynos/setup-fb-s5p.c:220,225`, `exynos4412-mehmet.dts:740-745`.
pub const LVDS_PDN: Pin = gpio::GPM1.pin(0);
/// How long the vendor kernel waits after releasing [`LVDS_PDN`], in milliseconds: `willow`
/// `arch/arm/mach-exynos/setup-fb-s5p.c:221`.
pub const LVDS_SETTLE_MS: u32 = 300;
/// Backlight control, high for light: `TOUT1`, PWM channel 1 (`exynos4412-mehmet.dts`,
/// `backlight` and `pwm1_out`). Verified by `gpio_test`.
pub const BACKLIGHT: Pin = gpio::GPD0.pin(1);
/// The backlight's PWM channel and period, 16 kHz: `exynos4412-mehmet.dts`, `pwms = <&pwm 1
/// 62500 1>`, and `willow` `mach-mehmet.c:3760`.
const BACKLIGHT_PWM: usize = 1;
const BACKLIGHT_PERIOD_NS: u32 = 62_500;
/// The brightness [`set_backlight`] turns on at: level 5 of the device tree's
/// `brightness-levels`, half of full. Full brightness drains the battery fast.
pub const DEFAULT_BRIGHTNESS: u8 = 128;

/// Why the display did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The power domain did not turn on. Holds the last status read.
    PowerDomain(u32),
    /// [`Cmu::sclk_fimd0_hz`] does not know the rate.
    UnknownClock,
    /// The pixel clock is out of reach from this rate of `sclk_fimd0`.
    BadClock(u32),
    /// The framebuffer is not the size of the panel, or the panel is too large.
    BadSize,
    /// The FIMD never reached vsync.
    NoVsync,
}

/// Timing of a panel, in pixels and lines, as a device tree's `display-timings` gives it.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Visible pixels per line.
    pub width: usize,
    /// Visible lines.
    pub height: usize,
    /// Wanted pixel clock.
    pub pixel_clock_hz: u32,
    /// Horizontal front porch.
    pub h_front: u32,
    /// Horizontal back porch.
    pub h_back: u32,
    /// Horizontal sync width.
    pub h_sync: u32,
    /// Vertical front porch.
    pub v_front: u32,
    /// Vertical back porch.
    pub v_back: u32,
    /// Vertical sync width.
    pub v_sync: u32,
    /// HSYNC is active low.
    pub invert_hsync: bool,
    /// VSYNC is active low.
    pub invert_vsync: bool,
    /// Data enable is active low.
    pub invert_vden: bool,
    /// The panel samples on the rising edge of the pixel clock.
    pub invert_vclk: bool,
}

impl Timing {
    /// Frames per thousand seconds at a given pixel clock.
    pub const fn refresh_millihertz(&self, pixel_clock_hz: u32) -> u32 {
        let line = self.width as u64 + (self.h_front + self.h_back + self.h_sync) as u64;
        let frame = line * (self.height as u64 + (self.v_front + self.v_back + self.v_sync) as u64);
        (pixel_clock_hz as u64 * 1000 / frame) as u32
    }
}

/// The board's panel, `samsung,ltn101al03`: `exynos4412-mehmet.dts:1423-1443`. The polarities
/// agree with `willow` `drivers/video/samsung/s3cfb_ltn101al03.c:39-42`.
pub const LTN101AL03: Timing = Timing {
    width: 1280,
    height: 800,
    pixel_clock_hz: 66_666_667,
    h_front: 16,
    h_back: 64,
    h_sync: 48,
    v_front: 1,
    v_back: 12,
    v_sync: 3,
    invert_hsync: true,
    invert_vsync: true,
    invert_vden: false,
    invert_vclk: false,
};

/// Puts the LCD pins in their function, at the drive strength of `exynos4412-mehmet.dts:911-917`:
/// `lcd_clk` and `lcd_data24`, `exynos4412-pinctrl.dtsi:362-400`.
pub fn configure_pins() {
    let lcd_pins = [(gpio::GPF0, 8), (gpio::GPF1, 8), (gpio::GPF2, 8), (gpio::GPF3, 4)];
    for (bank, count) in lcd_pins {
        for number in 0..count {
            let pin = bank.pin(number);
            pin.set_function(Function::Alt(2));
            pin.set_pull(Pull::None);
            pin.set_drive(Drive::Level4);
        }
    }
}

/// Turns the LVDS transmitter on or off. After turning it on, wait [`LVDS_SETTLE_MS`] before
/// the backlight, as the vendor kernel does.
pub fn set_lvds(on: bool) {
    LVDS_PDN.set_pull(Pull::Down);
    LVDS_PDN.set_drive(Drive::Level4);
    LVDS_PDN.set_output(on);
}

/// Turns the backlight on, at [`DEFAULT_BRIGHTNESS`], or off.
pub fn set_backlight(on: bool) {
    set_brightness(if on { DEFAULT_BRIGHTNESS } else { 0 });
}

/// Sets the backlight's brightness, 0 (off) to 255 (full): the share of each 62.5 us period
/// that [`BACKLIGHT`] is high. 0 and 255 make it a plain output, low or high, the PWM stopped;
/// anything else puts it in its `TOUT1` function (`pwm1_out`, `exynos4412-pinctrl.dtsi:299-304`).
/// If the PWM cannot run (the rate of `aclk100` unknown), any level but 0 is full.
pub fn set_brightness(level: u8) {
    BRIGHTNESS.store(level, Ordering::Relaxed);
    let high_ns = (u64::from(BACKLIGHT_PERIOD_NS) * u64::from(level) / 255) as u32;
    if level != 0 && level != 255 && pwm::start(BACKLIGHT_PWM, BACKLIGHT_PERIOD_NS, high_ns).is_ok() {
        BACKLIGHT.set_pull(Pull::None);
        BACKLIGHT.set_drive(Drive::Level1);
        BACKLIGHT.set_function(Function::Alt(2));
        return;
    }
    BACKLIGHT.set_output(level != 0);
    pwm::stop(BACKLIGHT_PWM);
}

/// The last level [`set_brightness`] set, 0 before any.
static BRIGHTNESS: AtomicU8 = AtomicU8::new(0);

/// The level the backlight was last set to: 0 (off) to 255.
pub fn brightness() -> u8 {
    BRIGHTNESS.load(Ordering::Relaxed)
}

/// Turns the backlight off, then the LVDS transmitter: the reverse of the order they go on.
/// U-Boot leaves both on after its logo. [`entry!`](crate::entry) calls this before `main`, as
/// the backlight drains the battery; an application that shows something turns them on itself.
pub fn panel_off() {
    set_backlight(false);
    set_lvds(false);
}

/// Packs a color for a [`Framebuffer`].
pub const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    (red as u32) << 16 | (green as u32) << 8 | blue as u32
}

/// Pixels the FIMD reads from DRAM, in `0x00RRGGBB` form, line after line.
///
/// It holds a `'static` buffer because the FIMD keeps reading it, with no way for the borrow
/// checker to know. The CPU draws in one of two ways:
///
/// - Uncached, from [`new`](Self::new) when it can: through [`mmu::DRAM_UNCACHED`], the
///   uncached view of the buffer. Each write goes to DRAM, combined with its neighbours, so
///   [`flush`](Self::flush) and [`flush_rect`](Self::flush_rect) only wait for them, and a device
///   that wrote the buffer needs nothing done before the CPU reads it. Filling is about six times
///   as fast as cached and flushed (`gpu_rect_test`: 7.8 M pixels in 17 ms against 109 ms), but
///   each read waits for DRAM: drawing that reads what it draws on, blending, wants the other.
/// - Cached, from [`new_cached`](Self::new_cached), or when the buffer is not where the uncached
///   view reaches: drawing reaches the screen only after a flush, the FIMD not seeing the caches.
#[derive(Debug)]
pub struct Framebuffer {
    pixels: &'static mut [u32],
    width: usize,
    height: usize,
    /// The address of the uncached view of `pixels`, the CPU's way to them; 0 when cached. The
    /// cached view then holds no dirty line of the buffer: nothing writes there.
    uncached: usize,
}

impl Framebuffer {
    /// Wraps a buffer of at least `width * height` pixels, drawn in uncached when the buffer is
    /// inside the first [`mmu::DRAM_UNCACHED_SIZE`] bytes of DRAM and starts and ends on whole
    /// lines of [`mmu::DCACHE_LINE`] bytes, and the MMU is on ([`mmu::enable`]); cached
    /// otherwise. `None` if it is smaller.
    ///
    /// What the caches hold of the buffer is written out first, then dropped, so what was in it
    /// stays.
    pub fn new(pixels: &'static mut [u32], width: usize, height: usize) -> Option<Self> {
        let mut buffer = Self::new_cached(pixels, width, height)?;
        let (start, len) = (buffer.address(), size_of_val(buffer.pixels));
        let whole_lines = start % mmu::DCACHE_LINE == 0 && len % mmu::DCACHE_LINE == 0;
        let reachable = mmu::is_enabled() && mmu::uncached(start + len - 1).is_some();
        if let (true, true, Some(uncached)) = (whole_lines, reachable, mmu::uncached(start)) {
            mmu::clean_dcache_range(start, len);
            mmu::invalidate_dcache_range(start, len);
            buffer.uncached = uncached;
        }
        Some(buffer)
    }

    /// Wraps a buffer of at least `width * height` pixels, drawn in cached. `None` if it is
    /// smaller.
    pub fn new_cached(pixels: &'static mut [u32], width: usize, height: usize) -> Option<Self> {
        (pixels.len() >= width.checked_mul(height)?).then_some(Self { pixels, width, height, uncached: 0 })
    }

    /// Whether the CPU draws through the uncached view: see [`new`](Self::new).
    pub const fn is_uncached(&self) -> bool {
        self.uncached != 0
    }

    /// Pixels per line.
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Lines.
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Where the pixels start: an identity-mapped, so physical, address.
    pub fn address(&self) -> usize {
        self.pixels.as_ptr() as usize
    }

    /// The pixels, line after line. Uncached, as DRAM has them. Cached, as the CPU's caches have
    /// them: after a device wrote them,
    /// [`mmu::invalidate_dcache_range`](crate::mmu::invalidate_dcache_range) first.
    pub fn pixels(&self) -> &[u32] {
        if self.is_uncached() {
            // SAFETY: the uncached view of the buffer, which `new` checked it covers, as long as
            // `self` is borrowed; nothing writes through the CPU while it is.
            unsafe { core::slice::from_raw_parts(self.uncached as *const u32, self.width * self.height) }
        } else {
            &self.pixels[..self.width * self.height]
        }
    }

    /// Bytes from one line to the next.
    pub const fn stride(&self) -> usize {
        self.width * size_of::<u32>()
    }

    /// `count` pixels of line `row` from column `x`, the way the CPU draws them. The caller keeps
    /// them inside the buffer.
    fn span(&mut self, row: usize, x: usize, count: usize) -> &mut [u32] {
        let start = row * self.width + x;
        if self.is_uncached() {
            // SAFETY: inside the buffer, by the caller, in its uncached view, as long as `self` is
            // borrowed mutably. The cached view has no dirty line there (`new`), and `pixels` is
            // not written through, so no write-back comes over these.
            unsafe { core::slice::from_raw_parts_mut((self.uncached as *mut u32).add(start), count) }
        } else {
            &mut self.pixels[start..start + count]
        }
    }

    /// Sets one pixel. Ignores one outside the buffer.
    pub fn set_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x < self.width && y < self.height {
            self.span(y, x, 1)[0] = color;
        }
    }

    /// Writes `pixels` into line `y` from column `x`, clipped to the buffer. Uncached, the way to
    /// draw what is not one color: prepared in ordinary memory, one span of a line goes in a
    /// block copy, much faster than the same pixels one by one through
    /// [`set_pixel`](Self::set_pixel).
    pub fn write_span(&mut self, x: usize, y: usize, pixels: &[u32]) {
        if y >= self.height || x >= self.width {
            return;
        }
        let count = pixels.len().min(self.width - x);
        self.span(y, x, count).copy_from_slice(&pixels[..count]);
    }

    /// Fills a rectangle, clipped to the buffer.
    pub fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: u32) {
        let right = x.saturating_add(width).min(self.width);
        let bottom = y.saturating_add(height).min(self.height);
        if x >= right {
            return;
        }
        for row in y..bottom {
            self.span(row, x, right - x).fill(color);
        }
    }

    /// Writes the whole buffer out to DRAM, where the FIMD reads it.
    pub fn flush(&self) {
        if self.is_uncached() {
            wait_writes();
        } else {
            mmu::clean_dcache_range(self.address(), self.stride() * self.height);
        }
    }

    /// Writes one rectangle out to DRAM, clipped to the buffer.
    pub fn flush_rect(&self, x: usize, y: usize, width: usize, height: usize) {
        if self.is_uncached() {
            wait_writes();
            return;
        }
        let right = x.saturating_add(width).min(self.width);
        let bottom = y.saturating_add(height).min(self.height);
        if x >= right {
            return;
        }
        for row in y..bottom {
            let start = self.address() + (row * self.width + x) * size_of::<u32>();
            mmu::clean_dcache_range(start, (right - x) * size_of::<u32>());
        }
    }
}

/// Waits until the CPU's writes so far have reached memory: an uncached one may still be in the
/// store buffer.
fn wait_writes() {
    // SAFETY: a barrier; it touches no memory or register.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
}

/// Registers as found, for reports. See [`Fimd::registers`].
#[derive(Clone, Copy, Debug)]
pub struct RegisterDump {
    /// `VIDCON0`.
    pub vidcon0: u32,
    /// `VIDCON1`.
    pub vidcon1: u32,
    /// `VIDTCON0`.
    pub vidtcon0: u32,
    /// `VIDTCON1`.
    pub vidtcon1: u32,
    /// `VIDTCON2`.
    pub vidtcon2: u32,
    /// `WINCON(0)`.
    pub wincon0: u32,
    /// `SHADOWCON`.
    pub shadowcon: u32,
    /// `VIDW_BUF_START(0)`.
    pub buf_start: u32,
    /// `VIDW_BUF_START_S(0)`.
    pub buf_start_shadow: u32,
}

/// Raw status of the FIMD's power domain.
pub fn power_domain_status() -> u32 {
    // SAFETY: this register of the PMU is device memory that stays mapped for the whole program.
    unsafe { ptr::read_volatile((soc::PD_LCD0_BASE + DOMAIN_STATUS) as *const u32) }
}

/// Raw `LCDBLK_CFG` system register.
pub fn lcdblk_cfg() -> u32 {
    // SAFETY: as in `power_domain_status`, for the system registers.
    unsafe { ptr::read_volatile((soc::SYSREG_BASE + LCDBLK_CFG) as *const u32) }
}

/// Handle to the FIMD. Holds no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fimd;

impl Fimd {
    /// Creates a handle.
    pub const fn new() -> Self {
        Self
    }

    // Only for use once `is_accessible` holds.
    fn regs(&self) -> &Registers {
        // SAFETY: FIMD_BASE is this MMIO block, device memory that stays mapped for the whole
        // program, and the struct has interior mutable cells only.
        unsafe { &*(soc::FIMD_BASE as *const Registers) }
    }

    /// Whether the FIMD's registers can be touched: domain on and register clock running.
    pub fn is_accessible(&self) -> bool {
        power_domain_status() & DOMAIN_ON == DOMAIN_ON && Cmu::new().fimd_clocks().bus_clock_on()
    }

    /// The registers as they are now. `None` if they cannot be read at this point.
    pub fn registers(&self) -> Option<RegisterDump> {
        if !self.is_accessible() {
            return None;
        }
        let regs = self.regs();
        Some(RegisterDump {
            vidcon0: regs.vidcon0.get(),
            vidcon1: regs.vidcon1.get(),
            vidtcon0: regs.vidtcon0.get(),
            vidtcon1: regs.vidtcon1.get(),
            vidtcon2: regs.vidtcon2.get(),
            wincon0: regs.wincon[0].get(),
            shadowcon: regs.shadowcon.get(),
            buf_start: regs.buf_start.get(),
            buf_start_shadow: regs.buf_start_shadow.get(),
        })
    }

    /// Turns on the power domain and the clocks, and leaves the FIMD stopped, whatever the
    /// bootloader had it doing.
    pub fn power_on(&self) -> Result<(), Error> {
        if power_domain_status() & DOMAIN_ON != DOMAIN_ON {
            // SAFETY: as in `power_domain_status`. Nothing uses this domain at this point.
            unsafe {
                ptr::write_volatile(soc::PD_LCD0_BASE as *mut u32, DOMAIN_ON);
            }
            let mut polls = 0;
            while power_domain_status() & DOMAIN_ON != DOMAIN_ON {
                polls += 1;
                if polls == POLL_LIMIT {
                    return Err(Error::PowerDomain(power_domain_status()));
                }
            }
        }
        let cmu = Cmu::new();
        cmu.open_fimd_gates();
        self.stop();
        cmu.set_fimd_clock();
        Ok(())
    }

    /// Stops the output at once. The domain and clocks must be on.
    pub fn stop(&self) {
        let regs = self.regs();
        regs.vidcon0.set(regs.vidcon0.get() & !(VIDCON0_ENVID | VIDCON0_ENVID_F));
    }

    /// Whether the output runs: accessible, with `ENVID` set.
    pub fn is_running(&self) -> bool {
        self.is_accessible() && self.regs().vidcon0.get() & VIDCON0_ENVID != 0
    }

    /// Starts the output again after [`stop`](Self::stop), with the timing and the window
    /// [`start`](Self::start) set, which the stop kept.
    pub fn resume(&self) {
        let regs = self.regs();
        regs.vidcon0.set(regs.vidcon0.get() | VIDCON0_ENVID | VIDCON0_ENVID_F);
    }

    /// Configures the output and window 0 for `timing`, shows `framebuffer` and starts. Returns
    /// the pixel clock it got. [`power_on`](Self::power_on) must have run.
    ///
    /// The buffer's cached lines are not cleaned here: draw and flush first.
    pub fn start(&self, timing: &Timing, framebuffer: &Framebuffer) -> Result<u32, Error> {
        if framebuffer.width() != timing.width
            || framebuffer.height() != timing.height
            || timing.width == 0
            || timing.height == 0
            || timing.width > MAX_SIZE
            || timing.height > MAX_SIZE
        {
            return Err(Error::BadSize);
        }
        let source = Cmu::new().sclk_fimd0_hz().ok_or(Error::UnknownClock)?;
        let wanted = timing.pixel_clock_hz;
        let divider = (source + wanted / 2).checked_div(wanted).ok_or(Error::BadClock(source))?;
        if divider == 0 || divider > MAX_DIVIDER {
            return Err(Error::BadClock(source));
        }

        let regs = self.regs();
        self.stop();
        set_fimd_bypass();

        // Fields of the window apply together, from the next vsync after this is lifted.
        regs.shadowcon.set(regs.shadowcon.get() | SHADOWCON_PROTECT0);

        let mut vidcon1 = VIDCON1_VCLK_RUN_VDEN_DIS;
        if timing.invert_vclk {
            vidcon1 |= VIDCON1_INV_VCLK;
        }
        if timing.invert_hsync {
            vidcon1 |= VIDCON1_INV_HSYNC;
        }
        if timing.invert_vsync {
            vidcon1 |= VIDCON1_INV_VSYNC;
        }
        if timing.invert_vden {
            vidcon1 |= VIDCON1_INV_VDEN;
        }
        regs.vidcon1.set(vidcon1);
        regs.vidtcon0.set(tcon(timing.v_back, timing.v_front, timing.v_sync));
        regs.vidtcon1.set(tcon(timing.h_back, timing.h_front, timing.h_sync));
        let (last_x, last_y) = ((timing.width - 1) as u32, (timing.height - 1) as u32);
        regs.vidtcon2.set(last_y << SIZE_HIGH_SHIFT | last_x);

        for wincon in &regs.wincon[1..] {
            wincon.set(wincon.get() & !WINCON_ENWIN);
        }
        regs.wincon[0].set(WINCON_ENWIN | WINCON_BPP_888 | WINCON_WSWP | WINCON_BURST_16);
        regs.vidosd0a.set(0);
        regs.vidosd0b.set(last_x << SIZE_HIGH_SHIFT | last_y);
        regs.vidosd0c.set((timing.width * timing.height) as u32);

        let start = framebuffer.address() as u32;
        let line = framebuffer.stride() as u32;
        regs.buf_start.set(start);
        regs.buf_end.set(start + line * timing.height as u32);
        regs.buf_size.set(0 << BUF_SIZE_OFFSET_SHIFT | line);
        regs.win0map.set(0);

        regs.shadowcon.set(SHADOWCON_CH0);

        let clock = if divider > 1 {
            (divider - 1) << VIDCON0_CLKVAL_SHIFT | VIDCON0_CLKDIR
        } else {
            0
        };
        // VIDOUT, PNRMODE and CLKSEL stay 0: RGB output, RGB order (samsung_fimd.h:23,51), and
        // this SoC has no CLKSEL (exynos_drm_fimd.c:134-141, no `has_clksel`).
        regs.vidcon0.set(clock | VIDCON0_VCLK_FREE | VIDCON0_ENVID | VIDCON0_ENVID_F);
        Ok(source / divider)
    }

    /// Shows `framebuffer` instead, from the next vsync on, for double buffering: draw into one
    /// buffer while the FIMD scans the other, then switch. Returns at once; [`wait_shown`]
    /// waits until the switch has happened, after which the old buffer is free to draw into.
    /// The window's address registers change together: the shadow update is held off while they
    /// are written, as `fimd_shadow_protect_win` does (`exynos_drm_fimd.c:739`).
    ///
    /// [`start`](Self::start) must have run with a buffer of the same size.
    ///
    /// [`wait_shown`]: Self::wait_shown
    pub fn show(&self, framebuffer: &Framebuffer) -> Result<(), Error> {
        let regs = self.regs();
        let start = framebuffer.address() as u32;
        let len = (framebuffer.stride() * framebuffer.height()) as u32;
        if regs.buf_end.get().wrapping_sub(regs.buf_start.get()) != len {
            return Err(Error::BadSize);
        }
        regs.shadowcon.set(regs.shadowcon.get() | SHADOWCON_PROTECT0);
        regs.buf_start.set(start);
        regs.buf_end.set(start + len);
        regs.shadowcon.set(regs.shadowcon.get() & !SHADOWCON_PROTECT0);
        Ok(())
    }

    /// Waits until the FIMD scans `framebuffer`: see [`show`](Self::show). Asleep until each
    /// frame interrupt ([`sleep::until`]) when the MCT runs, polled otherwise.
    pub fn wait_shown(&self, framebuffer: &Framebuffer) -> Result<(), Error> {
        let wanted = framebuffer.address() as u32;
        if let Some(deadline) = self.frame_interrupt() {
            let shown = sleep::until(Mct::new(), &[soc::FIMD_IRQ], Some(deadline), || {
                self.regs().vidintcon1.set(VIDINTCON1_INT_FRAME);
                self.scan_address() == wanted
            });
            return if shown { Ok(()) } else { Err(Error::NoVsync) };
        }
        let mut polls = 0;
        while self.scan_address() != framebuffer.address() as u32 {
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::NoVsync);
            }
        }
        Ok(())
    }

    /// Turns on the frame interrupt, at the start of each vsync, and lets it through the
    /// combiner, if the MCT runs to time the wait out. Returns the wait's deadline then.
    fn frame_interrupt(&self) -> Option<u64> {
        let mct = Mct::new();
        if !mct.is_running() {
            return None;
        }
        let regs = self.regs();
        let wanted = VIDINTCON0_FRAMESEL0_VSYNC | VIDINTCON0_INT_FRAME | VIDINTCON0_INT_ENABLE;
        let now = regs.vidintcon0.get();
        if now & (VIDINTCON0_FRAMESEL0_MASK | VIDINTCON0_FRAMESEL1_MASK | wanted) != wanted {
            regs.vidintcon0.set((now & !(VIDINTCON0_FRAMESEL0_MASK | VIDINTCON0_FRAMESEL1_MASK)) | wanted);
        }
        let group = soc::FIMD_COMBINER_GROUP;
        let enable = (soc::COMBINER_BASE + group / 4 * 0x10 + COMBINER_ENABLE_SET) as *mut u32;
        // SAFETY: the combiner's enable-set register of the FIMD's group, device memory that
        // stays mapped; writing 1 enables that source only, the others are untouched.
        unsafe { ptr::write_volatile(enable, 1 << ((group % 4) * 8 + COMBINER_VSYNC_SOURCE)) };
        Some(mct.count() + FRAME_TIMEOUT_US * COUNTS_PER_US)
    }

    /// The FIMD's line counter, which moves only while it scans.
    pub fn line(&self) -> u32 {
        (self.regs().vidcon1.get() >> VIDCON1_LINECNT_SHIFT) & VIDCON1_LINECNT_MASK
    }

    /// The buffer address the FIMD is using, as opposed to the one programmed: a difference
    /// means the settings have not applied.
    pub fn scan_address(&self) -> u32 {
        self.regs().buf_start_shadow.get()
    }

    /// Waits for the next start of vsync.
    pub fn wait_vsync(&self) -> Result<(), Error> {
        let regs = self.regs();
        if let Some(deadline) = self.frame_interrupt() {
            regs.vidintcon1.set(VIDINTCON1_INT_FRAME);
            let pending = || regs.vidintcon1.get() & VIDINTCON1_INT_FRAME != 0;
            return if sleep::until(Mct::new(), &[soc::FIMD_IRQ], Some(deadline), pending) {
                regs.vidintcon1.set(VIDINTCON1_INT_FRAME);
                Ok(())
            } else {
                Err(Error::NoVsync)
            };
        }
        let in_vsync = || (regs.vidcon1.get() >> VIDCON1_VSTATUS_SHIFT) & 0x3 == VSTATUS_VSYNC;
        let mut polls = 0;
        while in_vsync() {
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::NoVsync);
            }
        }
        polls = 0;
        while !in_vsync() {
            polls += 1;
            if polls == POLL_LIMIT {
                return Err(Error::NoVsync);
            }
        }
        Ok(())
    }

    /// Shows one flat color on the whole window, read from a register and not from DRAM, which
    /// tells a panel problem from a framebuffer problem. `None` goes back to the framebuffer.
    /// Applies at the next vsync.
    pub fn set_solid_color(&self, color: Option<u32>) {
        self.regs().win0map.set(match color {
            Some(color) => WINMAP_ENABLE | (color & 0xFF_FFFF),
            None => 0,
        });
    }
}

const fn tcon(back: u32, front: u32, sync: u32) -> u32 {
    (back - 1) << TCON_BACK_PORCH_SHIFT | (front - 1) << TCON_FRONT_PORCH_SHIFT | (sync - 1)
}

fn set_fimd_bypass() {
    let address = (soc::SYSREG_BASE + LCDBLK_CFG) as *mut u32;
    let was_masked = cpu::mask_irq();
    // SAFETY: as in `lcdblk_cfg`. This sets only the bit that belongs to this path.
    unsafe {
        ptr::write_volatile(address, ptr::read_volatile(address) | LCDBLK_FIMD_BYPASS);
    }
    cpu::restore_irq(was_masked);
}
