//! The Mali-400 MP4 GPU: its power domain and clock, what its blocks say they are, and their
//! resets. Nothing is rendered yet.
//!
//! Paths are relative to the Linux 5.15.198 tree. Register offsets come from
//! `drivers/gpu/drm/lima/lima_regs.h`, cited as `lima_regs.h:<line>`, and the blocks' places in
//! the GPU from `lima_device.c:50-73`, the Mali-400 column. Used as a register map only.
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
const PP_INT_RAWSTAT: usize = 0x1020;
const PP_INT_CLEAR: usize = 0x1024;
const PP_INT_MASK: usize = 0x1028;
const PP_IRQ_RESET_COMPLETED: u32 = 1 << 12;

/// lima_regs.h:233-250.
const MMU_DTE_ADDR: usize = 0x00;
const MMU_STATUS: usize = 0x04;
const MMU_COMMAND: usize = 0x08;
const MMU_COMMAND_HARD_RESET: u32 = 0x06;
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
        let (base, block) = match pp {
            Some(index) => (PP_MMU[index], Block::PpMmu(index)),
            None => (GP_MMU, Block::GpMmu),
        };
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
