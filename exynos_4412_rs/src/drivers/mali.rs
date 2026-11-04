//! The Mali-400 MP4 GPU: its power domain and clock, what its blocks say they are, their
//! resets, its MMUs, and frames for the pixel processors that only clear.
//!
//! Paths are relative to the Linux 5.15.198 tree. Register offsets come from
//! `drivers/gpu/drm/lima/lima_regs.h`, cited as `lima_regs.h:<line>`, and the blocks' places in
//! the GPU from `lima_device.c:50-73`, the Mali-400 column. Used as a register map only. The
//! layout of a PP frame and of the stream of tiles it reads come from Mesa 25.3.3's lima driver
//! (MIT), `src/gallium/drivers/lima/`: `genxml/common.xml:222-263` (`PP Frame`),
//! `lima_gpu.h:43-56` (write-back registers) and `lima_job.c:501-566,747-871`.
//!
//! # Memory
//!
//! The GPU reads and writes DRAM through its MMUs, not through the CPU's caches: clean what the
//! CPU wrote for it ([`mmu::clean_dcache_range`]), and drop the lines of what it will write
//! first ([`mmu::discard_dcache_range`]). [`IdentityMap`] gives the GPU the addresses the CPU
//! uses.
//!
//! # Order
//!
//! 1. The supply, BUCK4 `vdd_g3d` (`exynos4412-mehmet.dts:1197-1202`, `mali-supply` at :1584):
//!    always on, as U-Boot left it. [`Max77686::dvs_buck`](crate::drivers::max77686) reads it.
//! 2. [`Mali::power_on`]: the power domain, then the clock gate, then `sclk_g3d`.
//! 3. Only then the GPU's registers: with the domain off or the gate shut, a read does not
//!    answer. [`Mali::is_accessible`] tells.
//!
//! # Interrupts
//!
//! One line per block, level-triggered: `exynos4412.dtsi:729-750`, GIC SPI numbers, here as
//! GIC IDs (SPI + 32).

use core::ptr;

use crate::drivers::cmu::Cmu;
use crate::mmu;
use crate::drivers::timer::Mct;
use crate::soc;

/// The GP's interrupt: `exynos4412.dtsi:729` SPI 127.
pub const GP_IRQ: u16 = 32 + 127;
/// The GP's MMU: SPI 122.
pub const GP_MMU_IRQ: u16 = 32 + 122;
/// PP0 to PP3: SPI 123 to 126.
pub const PP_IRQ: [u16; PP_COUNT] = [32 + 123, 32 + 124, 32 + 125, 32 + 126];
/// The PPs' MMUs: SPI 118 to 121.
pub const PP_MMU_IRQ: [u16; PP_COUNT] = [32 + 118, 32 + 119, 32 + 120, 32 + 121];
/// The GPU's power management unit: SPI 117.
pub const PMU_IRQ: u16 = 32 + 117;

/// Pixel processors: MP4.
pub const PP_COUNT: usize = 4;

/// `exynos4412.dtsi:756-771`: the lowest of the GPU's operating points, 160 MHz at 875 mV.
pub const SAFE_HZ: u32 = 160_000_000;
/// The voltage `SAFE_HZ` needs.
pub const SAFE_MICROVOLTS: u32 = 875_000;

// Where each block is, from `G3D_BASE`: lima_device.c:51-69.
const GP: usize = 0x0000;
const L2: usize = 0x1000;
const PMU: usize = 0x2000;
const GP_MMU: usize = 0x3000;
const PP_MMU: [usize; PP_COUNT] = [0x4000, 0x5000, 0x6000, 0x7000];
const PP: [usize; PP_COUNT] = [0x8000, 0xA000, 0xC000, 0xE000];

/// lima_regs.h:14-34.
const PMU_POWER_UP: usize = 0x00;
const PMU_STATUS: usize = 0x08;
const PMU_INT_MASK: usize = 0x0C;
const PMU_INT_RAWSTAT: usize = 0x10;
const PMU_INT_CLEAR: usize = 0x18;
const PMU_INT_CMD: u32 = 1 << 0;
const PMU_SW_DELAY: usize = 0x1C;
/// lima_pmu.c:73-76: lower, and the GPU is unstable at high clocks.
const PMU_SW_DELAY_VALUE: u32 = 0xFFFF;
/// lima_regs.h:16-18: GP, L2, and the four PPs.
pub const PMU_ALL: u32 = 0x3F;

/// lima_regs.h:37-42.
const L2_SIZE: usize = 0x04;
const L2_STATUS: usize = 0x08;
const L2_STATUS_COMMAND_BUSY: u32 = 1 << 0;
const L2_COMMAND: usize = 0x10;
const L2_COMMAND_CLEAR_ALL: u32 = 1 << 0;

/// lima_regs.h:60-105.
const GP_CMD: usize = 0x20;
const GP_CMD_SOFT_RESET: u32 = 1 << 10;
const GP_INT_RAWSTAT: usize = 0x24;
const GP_INT_CLEAR: usize = 0x28;
const GP_INT_MASK: usize = 0x2C;
const GP_IRQ_RESET_COMPLETED: u32 = 1 << 19;
const GP_STATUS: usize = 0x68;
const GP_VERSION: usize = 0x6C;

/// lima_regs.h:161-188.
const PP_VERSION: usize = 0x1000;
const PP_STATUS: usize = 0x1008;
const PP_STATUS_RENDERING_ACTIVE: u32 = 1 << 0;
const PP_CTRL: usize = 0x100C;
const PP_CTRL_SOFT_RESET: u32 = 1 << 7;
const PP_CTRL_FORCE_RESET: u32 = 1 << 5;
const PP_CTRL_START_RENDERING: u32 = 1 << 6;
const PP_PERF_CNT_0_LIMIT: usize = 0x1088;
const PP_BUS_ERROR_STATUS: usize = 0x1050;
/// lima_regs.h:176-187.
const PP_IRQ_END_OF_FRAME: u32 = 1 << 0;
/// Everything but the end of a frame or a tile and the reset: lima_regs.h:178-187.
const PP_IRQ_ERRORS: u32 = 0x0FFC;
/// lima_regs.h:152,157: the frame registers, then three sets of write-back registers.
const PP_FRAME: usize = 0x0000;
const PP_WB: [usize; 3] = [0x0100, 0x0200, 0x0300];
const PP_INT_RAWSTAT: usize = 0x1020;
const PP_INT_CLEAR: usize = 0x1024;
const PP_INT_MASK: usize = 0x1028;
const PP_IRQ_RESET_COMPLETED: u32 = 1 << 12;

/// lima_regs.h:233-250.
const MMU_DTE_ADDR: usize = 0x00;
const MMU_STATUS: usize = 0x04;
const MMU_COMMAND: usize = 0x08;
const MMU_COMMAND_ENABLE_PAGING: u32 = 0x00;
const MMU_COMMAND_ZAP_CACHE: u32 = 0x04;
const MMU_COMMAND_HARD_RESET: u32 = 0x06;
const MMU_STATUS_PAGING_ENABLED: u32 = 1 << 0;
const MMU_PAGE_FAULT_ADDR: usize = 0x0C;
const MMU_INT_RAWSTAT: usize = 0x14;
const MMU_INT_MASK: usize = 0x1C;
/// lima_regs.h:256-257: page faults and read bus errors.
const MMU_INT_USED: u32 = 0x3;
/// lima_regs.h:260-262: present, readable, writable.
const PAGE_PRESENT: u32 = 1 << 0;
const PAGE_READ_WRITE: u32 = (1 << 1) | (1 << 2);
const PAGE_SIZE: usize = 4096;
/// Each directory entry covers 4 MiB: lima_vm.c:21.
const TABLE_SPAN: usize = 1 << 22;
const ENTRIES: usize = 1024;
/// lima_mmu.c:103-104: what a write of this reads back as, the low 12 bits dropped.
const MMU_DTE_PROBE: u32 = 0xCAFE_BABE;
const MMU_DTE_PROBE_READ: u32 = 0xCAFE_B000;

/// `LOCAL_PWR_CFG`, both in the configuration and in the status of a domain, as for the FIMD's.
const DOMAIN_ON: u32 = 7;
const DOMAIN_STATUS: usize = 0x04;

/// Longest wait for anything here.
const TIMEOUT_US: u64 = 10_000;
const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

/// What failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The power domain did not come on: its status.
    PowerDomain(u32),
    /// The registers cannot be read: the domain is off or the gate shut.
    NotAccessible,
    /// No answer in time from this block.
    Timeout(Block),
    /// A PP raised an error: its `INT_RAWSTAT`, and its MMU's `STATUS` and page fault address.
    Pp {
        /// Which PP.
        index: usize,
        /// `INT_RAWSTAT`.
        rawstat: u32,
        /// Its MMU's `STATUS`.
        mmu_status: u32,
        /// Its MMU's last page fault address.
        fault: u32,
    },
    /// An MMU's table register did not read back as written.
    MmuReadBack {
        /// Which MMU: [`Block::GpMmu`] or [`Block::PpMmu`].
        block: Block,
        /// What it read.
        read: u32,
    },
}

/// One of the GPU's blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    /// The geometry processor.
    Gp,
    /// A pixel processor.
    Pp(usize),
    /// The GP's MMU.
    GpMmu,
    /// A PP's MMU.
    PpMmu(usize),
    /// The shared L2 cache.
    L2,
    /// The power management unit.
    Pmu,
}

/// Raw status of the GPU's power domain.
pub fn power_domain_status() -> u32 {
    // SAFETY: this register of the PMU is device memory that stays mapped for the whole program.
    unsafe { ptr::read_volatile((soc::PD_G3D_BASE + DOMAIN_STATUS) as *const u32) }
}

/// What the GPU's blocks say they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The GP's `VERSION`.
    pub gp: u32,
    /// Each PP's `VERSION`.
    pub pp: [u32; PP_COUNT],
    /// The L2 cache's `SIZE`.
    pub l2_size: u32,
}

impl Identity {
    /// The GP's product: lima_gp.c:253-265.
    pub const fn gp_name(&self) -> &'static str {
        match self.gp >> 16 {
            0x0A07 => "Mali-200",
            0x0C07 => "Mali-300",
            0x0B07 => "Mali-400",
            0x0D07 => "Mali-450",
            _ => "unknown",
        }
    }

    /// A PP's product: lima_pp.c:205-217.
    pub const fn pp_name(&self, index: usize) -> &'static str {
        match self.pp[index] >> 16 {
            0xC807 => "Mali-200",
            0xCE07 => "Mali-300",
            0xCD07 => "Mali-400",
            0xCF07 => "Mali-450",
            _ => "unknown",
        }
    }

    /// Major and minor revision of a `VERSION`.
    pub const fn revision(version: u32) -> (u8, u8) {
        ((version >> 8) as u8, version as u8)
    }

    /// The L2 cache: KiB, ways, line bytes, external bus bits (lima_l2_cache.c:86-90).
    pub const fn l2(&self) -> (u32, u32, u32, u32) {
        let size = self.l2_size;
        let log_bytes = (size >> 16) & 0xFF;
        let kib = if log_bytes >= 10 { 1 << (log_bytes - 10) } else { 0 };
        (kib, 1 << ((size >> 8) & 0x1F), 1 << (size & 0x1F), 1 << ((size >> 24) & 0x1F))
    }
}

/// Handle to the GPU. Holds no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mali;

impl Mali {
    /// Creates a handle.
    pub const fn new() -> Self {
        Self
    }

    /// Whether the GPU's registers can be touched: domain on and gate open.
    pub fn is_accessible(&self) -> bool {
        power_domain_status() & DOMAIN_ON == DOMAIN_ON && Cmu::new().g3d_clocks().gate_open()
    }

    /// Turns on the power domain and the clock gate, and runs the core clock at the highest rate
    /// not above `hz` from the MPLL. Returns the rate. The supply must be up to that rate's
    /// voltage: see the [module documentation](self).
    pub fn power_on(&self, hz: u32, mct: Mct) -> Result<u32, Error> {
        if power_domain_status() & DOMAIN_ON != DOMAIN_ON {
            // SAFETY: as in `power_domain_status`, the configuration register before it. Nothing
            // uses this domain while it is off.
            unsafe { ptr::write_volatile(soc::PD_G3D_BASE as *mut u32, DOMAIN_ON) };
            let begin = mct.count();
            while power_domain_status() & DOMAIN_ON != DOMAIN_ON {
                if mct.count() - begin > TIMEOUT_US * COUNTS_PER_US {
                    return Err(Error::PowerDomain(power_domain_status()));
                }
            }
        }
        let cmu = Cmu::new();
        cmu.open_g3d_gate();
        Ok(cmu.set_g3d_clock(hz))
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: every offset is a register of the GPU's 64 KiB, device memory that stays mapped
        // for the whole program; callers check `is_accessible` first.
        unsafe { ptr::read_volatile((soc::G3D_BASE + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: as in `read`.
        unsafe { ptr::write_volatile((soc::G3D_BASE + offset) as *mut u32, value) }
    }

    fn check(&self) -> Result<(), Error> {
        if self.is_accessible() { Ok(()) } else { Err(Error::NotAccessible) }
    }

    fn wait(&self, block: Block, mct: Mct, mut done: impl FnMut() -> bool) -> Result<u64, Error> {
        let begin = mct.count();
        loop {
            if done() {
                return Ok((mct.count() - begin) / COUNTS_PER_US);
            }
            if mct.count() - begin > TIMEOUT_US * COUNTS_PER_US {
                return Err(Error::Timeout(block));
            }
        }
    }

    /// What the blocks say they are.
    pub fn identify(&self) -> Result<Identity, Error> {
        self.check()?;
        let mut pp = [0; PP_COUNT];
        for (version, base) in pp.iter_mut().zip(PP) {
            *version = self.read(base + PP_VERSION);
        }
        Ok(Identity { gp: self.read(GP + GP_VERSION), pp, l2_size: self.read(L2 + L2_SIZE) })
    }

    /// The GPU's own power status: a set bit is a block off, [`PMU_ALL`] the blocks.
    pub fn pmu_status(&self) -> Result<u32, Error> {
        self.check()?;
        Ok(self.read(PMU + PMU_STATUS))
    }

    /// Powers up the blocks in `mask` that are off, as lima_pmu.c:66-87 does, and returns how
    /// many microseconds it took.
    pub fn pmu_power_up(&self, mask: u32, mct: Mct) -> Result<u64, Error> {
        self.check()?;
        self.write(PMU + PMU_INT_MASK, 0);
        self.write(PMU + PMU_SW_DELAY, PMU_SW_DELAY_VALUE);
        let off = self.read(PMU + PMU_STATUS) & mask;
        if off == 0 {
            return Ok(0);
        }
        self.write(PMU + PMU_POWER_UP, off);
        let us = self.wait(Block::Pmu, mct, || self.read(PMU + PMU_INT_RAWSTAT) & PMU_INT_CMD != 0)?;
        self.write(PMU + PMU_INT_CLEAR, PMU_INT_CMD);
        Ok(us)
    }

    /// The GP's `STATUS`.
    pub fn gp_status(&self) -> Result<u32, Error> {
        self.check()?;
        Ok(self.read(GP + GP_STATUS))
    }

    /// Resets the GP, as lima_gp.c:68-101 does, and returns how many microseconds it took. With
    /// `raise`, its interrupt line is left raised by the reset's completion, for
    /// [`clear_interrupts`](Self::clear_interrupts).
    pub fn reset_gp(&self, mct: Mct, raise: bool) -> Result<u64, Error> {
        self.check()?;
        self.write(GP + GP_INT_MASK, 0);
        self.write(GP + GP_INT_CLEAR, GP_IRQ_RESET_COMPLETED);
        self.write(GP + GP_CMD, GP_CMD_SOFT_RESET);
        let us = self.wait(Block::Gp, mct, || self.read(GP + GP_INT_RAWSTAT) & GP_IRQ_RESET_COMPLETED != 0)?;
        if raise {
            self.write(GP + GP_INT_MASK, GP_IRQ_RESET_COMPLETED);
        } else {
            self.write(GP + GP_INT_CLEAR, self.read(GP + GP_INT_RAWSTAT));
        }
        Ok(us)
    }

    /// Resets PP `index`, as lima_pp.c:104-135 does, and returns how many microseconds it took.
    /// `raise` as for [`reset_gp`](Self::reset_gp).
    pub fn reset_pp(&self, index: usize, mct: Mct, raise: bool) -> Result<u64, Error> {
        self.check()?;
        let base = PP[index];
        self.write(base + PP_INT_MASK, 0);
        self.write(base + PP_INT_CLEAR, self.read(base + PP_INT_RAWSTAT));
        self.write(base + PP_CTRL, PP_CTRL_SOFT_RESET);
        let us = self.wait(Block::Pp(index), mct, || {
            self.read(base + PP_STATUS) & PP_STATUS_RENDERING_ACTIVE == 0
                && self.read(base + PP_INT_RAWSTAT) == PP_IRQ_RESET_COMPLETED
        })?;
        if raise {
            self.write(base + PP_INT_MASK, PP_IRQ_RESET_COMPLETED);
        } else {
            self.write(base + PP_INT_CLEAR, self.read(base + PP_INT_RAWSTAT));
        }
        Ok(us)
    }

    /// Clears what the GP and the PPs have raised and masks their interrupts.
    pub fn clear_interrupts(&self) -> Result<(), Error> {
        self.check()?;
        self.write(GP + GP_INT_MASK, 0);
        self.write(GP + GP_INT_CLEAR, self.read(GP + GP_INT_RAWSTAT));
        for base in PP {
            self.write(base + PP_INT_MASK, 0);
            self.write(base + PP_INT_CLEAR, self.read(base + PP_INT_RAWSTAT));
        }
        Ok(())
    }

    /// Checks that an MMU takes a table address, then resets it, as lima_mmu.c:62-72,103-107
    /// do. `pp` is the PP whose MMU, `None` for the GP's. Returns the MMU's `STATUS` after.
    pub fn reset_mmu(&self, pp: Option<usize>, mct: Mct) -> Result<u32, Error> {
        self.check()?;
        let (base, block) = Self::mmu_of(pp);
        self.write(base + MMU_DTE_ADDR, MMU_DTE_PROBE);
        let read = self.read(base + MMU_DTE_ADDR);
        if read != MMU_DTE_PROBE_READ {
            return Err(Error::MmuReadBack { block, read });
        }
        self.write(base + MMU_COMMAND, MMU_COMMAND_HARD_RESET);
        self.wait(block, mct, || self.read(base + MMU_DTE_ADDR) == 0)?;
        Ok(self.read(base + MMU_STATUS))
    }

    /// Empties the L2 cache, as lima_l2_cache.c:14-39 does.
    pub fn clear_l2(&self, mct: Mct) -> Result<u64, Error> {
        self.check()?;
        self.write(L2 + L2_COMMAND, L2_COMMAND_CLEAR_ALL);
        self.wait(Block::L2, mct, || self.read(L2 + L2_STATUS) & L2_STATUS_COMMAND_BUSY == 0)
    }
}

/// Page tables giving the GPU the CPU's addresses: an entry for each 4 KiB page of what is
/// mapped, read and write. `TABLES` tables of 4 KiB each map up to `TABLES` times 4 MiB.
#[repr(C, align(4096))]
pub struct IdentityMap<const TABLES: usize> {
    directory: [u32; ENTRIES],
    tables: [[u32; ENTRIES]; TABLES],
    used: usize,
}

/// Why [`IdentityMap::map`] could not map a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfTables;

impl<const TABLES: usize> IdentityMap<TABLES> {
    /// Maps nothing.
    pub const fn new() -> Self {
        Self { directory: [0; ENTRIES], tables: [[0; ENTRIES]; TABLES], used: 0 }
    }

    /// Maps `start..start + len`, widened to whole pages, to itself, and writes the tables out of
    /// the CPU's caches.
    pub fn map(&mut self, start: usize, len: usize) -> Result<(), OutOfTables> {
        let first = start & !(PAGE_SIZE - 1);
        let end = (start + len).next_multiple_of(PAGE_SIZE);
        let mut page = first;
        while page < end {
            let slot = page / TABLE_SPAN;
            if self.directory[slot] & PAGE_PRESENT == 0 {
                if self.used == TABLES {
                    return Err(OutOfTables);
                }
                let table = self.tables[self.used].as_ptr() as u32;
                self.directory[slot] = table | PAGE_PRESENT;
                self.used += 1;
            }
            let table = ((self.directory[slot] & !0xFFF) as usize - self.tables.as_ptr() as usize) / PAGE_SIZE;
            self.tables[table][(page % TABLE_SPAN) / PAGE_SIZE] = page as u32 | PAGE_PRESENT | PAGE_READ_WRITE;
            page += PAGE_SIZE;
        }
        mmu::clean_dcache_range(self as *const Self as usize, core::mem::size_of::<Self>());
        Ok(())
    }

    /// The address of the directory, for [`Mali::enable_mmu`].
    pub fn directory(&self) -> u32 {
        self.directory.as_ptr() as u32
    }
}

impl<const TABLES: usize> Default for IdentityMap<TABLES> {
    fn default() -> Self {
        Self::new()
    }
}

/// Pixel formats of a write-back: lima_format.c:35-38 in Mesa.
const WB_FORMAT_B8G8R8A8: u32 = 0x03;
/// `type` of a colour write-back: lima_job.c:787.
const WB_TYPE_COLOR: u32 = 0x02;

/// The registers of a PP frame: [`PP_FRAME_REGISTERS`] words, then up to three write-backs of
/// twelve. A frame from [`PpFrame::clear`] draws nothing: every tile is the clear colour.
#[derive(Clone, Copy, Debug)]
pub struct PpFrame {
    /// The frame registers: `genxml/common.xml:222-263`.
    pub regs: [u32; PP_FRAME_REGISTERS],
    /// The write-back registers: `lima_gpu.h:43-56`.
    pub wb: [[u32; 12]; 3],
}

/// lima_drm.h:77, `LIMA_PP_FRAME_REG_NUM`.
pub const PP_FRAME_REGISTERS: usize = 23;
/// Pixels on a side of a tile: lima_job.c:75-76.
pub const TILE: usize = 16;

impl PpFrame {
    /// A frame of `width` by `height` pixels that clears to `color`, 0xAARRGGBB, written to
    /// `target`, a linear buffer with lines `stride` bytes apart, 32 bits a pixel, as the FIMD
    /// shows them. Whole tiles are written, from `target` rounded down to 8 bytes: see
    /// [`Rect::split_for_gpu`]. Keep both inside the buffer. `render_state` is 64 bytes of zeros, 64-aligned: nothing is drawn with it. The
    /// stream comes per PP, from [`clear_stream`], in [`Mali::start_pp`]. Fields as
    /// lima_job.c:807-871 sets them.
    pub fn clear(width: usize, height: usize, color: u32, target: usize, stride: usize, render_state: usize) -> Self {
        let mut regs = [0; PP_FRAME_REGISTERS];
        regs[1] = render_state as u32;
        // Early Z.
        regs[3] = 1 << 1;
        regs[4] = 0x00FF_FFFF;
        // The clear colour, bytes red, green, blue, alpha from the lowest: the write-back then
        // stores it as B8G8R8A8, which reads as 0xAARRGGBB.
        let rgba = ((color >> 16) & 0xFF) | (color & 0xFF00) | ((color & 0xFF) << 16) | (color & 0xFF00_0000);
        regs[6..10].fill(rgba);
        regs[10] = (width - 1) as u32;
        regs[11] = (height - 1) as u32;
        regs[16] = 1;
        regs[17] = (height * 2 - 1) as u32;
        regs[18] = 0x77;
        regs[19] = 1;
        // Scale fragcoord and derivatives, flip dithering, fragcoord and derivatives.
        regs[21] = (1 << 2) | (1 << 3) | (1 << 9) | (1 << 10) | (1 << 11);
        // 8 bits for each channel.
        regs[22] = 0x8888;
        let mut wb = [[0; 12]; 3];
        wb[0][0] = WB_TYPE_COLOR;
        wb[0][1] = target as u32;
        wb[0][2] = WB_FORMAT_B8G8R8A8;
        wb[0][5] = (stride / 8) as u32;
        Self { regs, wb }
    }
}

/// A rectangle of pixels.
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

/// A rectangle split between the GPU and the CPU: [`Rect::split_for_gpu`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    /// What a PP frame may write: whole tiles from an even column.
    pub gpu: Option<Rect>,
    /// The rest, for the CPU: a left column, a right strip and a bottom strip.
    pub cpu: [Option<Rect>; 3],
}

impl Rect {
    /// Splits the rectangle into the part a PP may write without touching a pixel outside it,
    /// and the rest. On the board a PP's write-back:
    ///
    /// - writes every tile whole, 16 by 16 pixels from the frame's corner, whatever the bounding
    ///   box: a 50 by 30 frame wrote 64 by 32;
    /// - starts at its address rounded down to 8 bytes: a corner at an odd column of 32-bit
    ///   pixels wrote one column to its left.
    pub fn split_for_gpu(&self) -> Split {
        let Rect { x, y, width, height } = *self;
        let gpu_x = x.next_multiple_of(2).min(x + width);
        let gpu_width = (x + width - gpu_x) / TILE * TILE;
        let gpu_height = height / TILE * TILE;
        if gpu_width == 0 || gpu_height == 0 {
            return Split { gpu: None, cpu: [Some(*self).filter(|rect| rect.width > 0 && rect.height > 0), None, None] };
        }
        let left = (gpu_x > x).then_some(Rect { x, y, width: gpu_x - x, height });
        let right_x = gpu_x + gpu_width;
        let right = (right_x < x + width).then_some(Rect { x: right_x, y, width: x + width - right_x, height });
        let bottom = (gpu_height < height).then_some(Rect { x: gpu_x, y: y + gpu_height, width: gpu_width, height: height - gpu_height });
        Split { gpu: Some(Rect { x: gpu_x, y, width: gpu_width, height: gpu_height }), cpu: [left, right, bottom] }
    }
}

/// How a tile ends in a [`clear_stream`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileEnd {
    /// The tile's position alone: no list of polygons.
    Bare,
    /// The position, then a list of polygons at this address that holds only its end.
    EmptyList(u32),
}

/// Writes into `stream` the tiles of a `width` by `height` frame that PP `pp` of `pp_count`
/// renders: every `pp_count`th, interleaved among the PPs as lima_job.c:534-550 does, though in
/// rows rather than its Hilbert curve; then the stream's end. Returns
/// the words written. `stream` must hold 4 words for each of this PP's tiles, 2 for
/// [`TileEnd::Bare`], and 4 more.
pub fn clear_stream(stream: &mut [u32], width: usize, height: usize, pp: usize, pp_count: usize, end: TileEnd) -> usize {
    let (tiles_x, tiles_y) = (width.div_ceil(TILE), height.div_ceil(TILE));
    let mut at = 0;
    for tile in (pp..tiles_x * tiles_y).step_by(pp_count) {
        let (x, y) = ((tile % tiles_x) as u32, (tile / tiles_x) as u32);
        stream[at] = 0;
        stream[at + 1] = 0xB800_0000 | x | (y << 8);
        at += 2;
        if let TileEnd::EmptyList(list) = end {
            stream[at] = 0xE000_0002 | ((list >> 3) & !0xE000_0003);
            stream[at + 1] = 0xB000_0000;
            at += 2;
        }
    }
    stream[at..at + 4].copy_from_slice(&[0, 0xBC00_0000, 0, 0]);
    at + 4
}

/// An empty list of polygons for [`TileEnd::EmptyList`]: its end, as a stream's.
pub const EMPTY_LIST: [u32; 4] = [0, 0xBC00_0000, 0, 0];

impl Mali {
    /// Points an MMU at `directory` and turns paging on, as lima_mmu.c:62-80 does. `pp` as for
    /// [`reset_mmu`](Self::reset_mmu).
    pub fn enable_mmu(&self, pp: Option<usize>, directory: u32, mct: Mct) -> Result<(), Error> {
        self.reset_mmu(pp, mct)?;
        let (base, block) = Self::mmu_of(pp);
        self.write(base + MMU_INT_MASK, MMU_INT_USED);
        self.write(base + MMU_DTE_ADDR, directory);
        self.write(base + MMU_COMMAND, MMU_COMMAND_ZAP_CACHE);
        self.write(base + MMU_COMMAND, MMU_COMMAND_ENABLE_PAGING);
        self.wait(block, mct, || self.read(base + MMU_STATUS) & MMU_STATUS_PAGING_ENABLED != 0)?;
        Ok(())
    }

    fn mmu_of(pp: Option<usize>) -> (usize, Block) {
        match pp {
            Some(index) => (PP_MMU[index], Block::PpMmu(index)),
            None => (GP_MMU, Block::GpMmu),
        }
    }

    /// Writes `frame` into PP `index`, with `stream` as its stream of tiles, and starts it, as
    /// lima_pp.c:365-381 does. The PP must be idle, after a reset, with its MMU on.
    pub fn start_pp(&self, index: usize, frame: &PpFrame, stream: u32) -> Result<(), Error> {
        self.check()?;
        let base = PP[index];
        self.write(base + PP_INT_CLEAR, self.read(base + PP_INT_RAWSTAT));
        for (i, value) in frame.regs.iter().enumerate() {
            let value = if i == 0 { stream } else { *value };
            self.write(base + PP_FRAME + i * 4, value);
        }
        for (wb, registers) in PP_WB.iter().zip(&frame.wb) {
            for (i, value) in registers.iter().enumerate() {
                self.write(base + wb + i * 4, *value);
            }
        }
        self.write(base + PP_CTRL, PP_CTRL_START_RENDERING);
        Ok(())
    }

    /// Waits until PP `index` ends its frame, and returns how many microseconds that took from
    /// the call. An error or `timeout_us` without an end leaves the PP to [`hard_reset_pp`]
    /// (Self::hard_reset_pp).
    pub fn wait_pp(&self, index: usize, mct: Mct, timeout_us: u64) -> Result<u64, Error> {
        self.check()?;
        let base = PP[index];
        let begin = mct.count();
        loop {
            let raw = self.read(base + PP_INT_RAWSTAT);
            if raw & PP_IRQ_ERRORS != 0 {
                return Err(self.pp_error(index));
            }
            if raw & PP_IRQ_END_OF_FRAME != 0 {
                let us = (mct.count() - begin) / COUNTS_PER_US;
                self.write(base + PP_INT_CLEAR, raw);
                return Ok(us);
            }
            if mct.count() - begin > timeout_us * COUNTS_PER_US {
                return Err(self.pp_error(index));
            }
        }
    }

    fn pp_error(&self, index: usize) -> Error {
        Error::Pp {
            index,
            rawstat: self.read(PP[index] + PP_INT_RAWSTAT),
            mmu_status: self.read(PP_MMU[index] + MMU_STATUS),
            fault: self.read(PP_MMU[index] + MMU_PAGE_FAULT_ADDR),
        }
    }

    /// PP `index`'s `STATUS`, `BUS_ERROR_STATUS` and its MMU's `INT_RAWSTAT`, for reports.
    pub fn pp_state(&self, index: usize) -> Result<(u32, u32, u32), Error> {
        self.check()?;
        Ok((
            self.read(PP[index] + PP_STATUS),
            self.read(PP[index] + PP_BUS_ERROR_STATUS),
            self.read(PP_MMU[index] + MMU_INT_RAWSTAT),
        ))
    }

    /// Stops PP `index` whatever it does, as lima_pp.c:171-195 does.
    pub fn hard_reset_pp(&self, index: usize, mct: Mct) -> Result<u64, Error> {
        self.check()?;
        let base = PP[index];
        self.write(base + PP_PERF_CNT_0_LIMIT, 0xC0FF_E000);
        self.write(base + PP_INT_MASK, 0);
        self.write(base + PP_CTRL, PP_CTRL_FORCE_RESET);
        let us = self.wait(Block::Pp(index), mct, || {
            self.write(base + PP_PERF_CNT_0_LIMIT, 0xC01A_0000);
            self.read(base + PP_PERF_CNT_0_LIMIT) == 0xC01A_0000
        })?;
        self.write(base + PP_PERF_CNT_0_LIMIT, 0);
        self.write(base + PP_INT_CLEAR, self.read(base + PP_INT_RAWSTAT));
        Ok(us)
    }
}
