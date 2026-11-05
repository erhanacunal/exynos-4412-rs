// SPDX-License-Identifier: GPL-2.0-or-later
//! The Exynos 4412's MFC v5.1 video codec, driven through its firmware.
//!
//! This crate is GPL-2.0-or-later, unlike `exynos_4412_rs` (MIT), on which it builds: the order
//! of operations, the commands and the buffer sizes follow Linux 5.15.198's
//! `drivers/media/platform/s5p-mfc` (Copyright (C) 2010-2011 Samsung Electronics Co., Ltd.),
//! cited by file and line. The code is written for this crate. An application that links it is
//! GPL as well.
//!
//! The firmware, `s5p-mfc.fw`, is Samsung's and is not in this repository: the application
//! brings it in at build time.
//!
//! # Memory
//!
//! The codec reaches DRAM through two ports, A and B, each addressed from a base the host sets
//! ([`Banks`]). Linux reserves 36 MiB and 8 MiB for them (`exynos-mfc-reserved-memory.dtsi`). The
//! firmware runs from the start of port A. Nothing here uses the system MMUs in front of the
//! ports: addresses are physical.
//!
//! # So far
//!
//! [`Mfc::init`]: power, clock, reset, firmware into port A, its start, `SYS_INIT`, and its
//! version (`s5p_mfc_init_hw`, `s5p_mfc_ctrl.c`). Decoding comes next.

#![no_std]

pub mod regs;

use core::ptr;

use exynos_4412_rs::drivers::mfc::{self, PowerDomainTimeout};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{mmu, soc};

/// `s5p_mfc_common.h:51`: how long the controller and the command register get.
const BUS_TIMEOUT_MS: u64 = 500;
/// `s5p_mfc_common.h:49`: how long the firmware gets to answer.
const ANSWER_TIMEOUT_MS: u64 = 2000;
/// `s5p_mfc_reset`: the wait after resetting all but the memory controller.
const RESET_SETTLE_MS: u64 = 10;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The power domain did not turn on.
    Power(PowerDomainTimeout),
    /// A base address is not aligned to [`regs::BASE_ALIGN`], or port A is smaller than the
    /// firmware's buffer.
    BadBanks,
    /// The firmware is empty or larger than its buffer.
    BadFirmware(usize),
    /// The memory controller stayed busy after the reset: `MC_STATUS`.
    ResetTimeout(u32),
    /// The firmware did not take the last command: `HOST2RISC_CMD`.
    CommandTimeout(u32),
    /// No answer of the kind expected: what `RISC2HOST_CMD` read last.
    AnswerTimeout { want: u32, got: u32 },
    /// The firmware answered with an error, or with another answer: `RISC2HOST_CMD` and
    /// `RISC2HOST_ARG2`, its error code.
    Firmware { want: u32, got: u32, error: u32 },
}

impl From<PowerDomainTimeout> for Error {
    fn from(timeout: PowerDomainTimeout) -> Self {
        Self::Power(timeout)
    }
}

/// The physical base addresses of the two ports, and how much memory each may use from there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Banks {
    pub a: usize,
    pub a_len: usize,
    pub b: usize,
    pub b_len: usize,
}

impl Banks {
    fn valid(&self) -> bool {
        self.a.is_multiple_of(regs::BASE_ALIGN) && self.b.is_multiple_of(regs::BASE_ALIGN) && self.a_len >= regs::FW_BUFFER_BYTES
    }
}

/// The firmware's build date, from `FW_VERSION`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version(pub u32);

impl Version {
    /// Year (two digits), month and day, as `s5p_mfc_init_hw` prints them.
    pub const fn date(self) -> (u8, u8, u8) {
        ((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)
    }
}

/// What [`Mfc::init`] found.
#[derive(Clone, Copy, Debug)]
pub struct Started {
    /// `sclk_mfc`.
    pub hz: u32,
    pub version: Version,
    /// How long the firmware took to start, and `SYS_INIT` to answer, in microseconds.
    pub start_us: u64,
    pub init_us: u64,
}

/// The codec, its firmware running.
pub struct Mfc {
    banks: Banks,
    mct: Mct,
}

fn read(offset: usize) -> u32 {
    // SAFETY: every offset is a register of the MFC's 64 KiB, device memory that stays mapped for
    // the whole program; `Mfc` exists only after its power domain and gate are on.
    unsafe { ptr::read_volatile((soc::MFC_BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as in `read`.
    unsafe { ptr::write_volatile((soc::MFC_BASE + offset) as *mut u32, value) }
}

impl Mfc {
    /// Powers the codec on at `hz` (see [`mfc::VENDOR_HZ`]), copies `firmware` to the start of
    /// port A and starts it, as `s5p_mfc_init_hw` does for v5. Port A's first
    /// [`regs::FW_BUFFER_BYTES`] are the firmware's from then on.
    pub fn init(banks: Banks, firmware: &[u8], hz: u32, mct: Mct) -> Result<(Self, Started), Error> {
        if !banks.valid() {
            return Err(Error::BadBanks);
        }
        if firmware.is_empty() || firmware.len() > regs::FW_BUFFER_BYTES {
            return Err(Error::BadFirmware(firmware.len()));
        }
        let hz = mfc::power_on(hz, mct)?;
        let mfc = Self { banks, mct };

        // s5p_mfc_load_firmware: the image at port A's base, in DRAM before the RISC reads it.
        // SAFETY: port A's memory is the caller's to give, at least FW_BUFFER_BYTES long, and
        // nothing else uses it.
        unsafe { ptr::copy_nonoverlapping(firmware.as_ptr(), banks.a as *mut u8, firmware.len()) };
        mmu::clean_dcache_range(banks.a, firmware.len());

        mfc.reset()?;
        // s5p_mfc_init_memctrl, s5p_mfc_clear_cmds.
        write(regs::MC_DRAMBASE_ADR_A, banks.a as u32);
        write(regs::MC_DRAMBASE_ADR_B, banks.b as u32);
        write(regs::SI_CH0_INST_ID, u32::MAX);
        write(regs::SI_CH1_INST_ID, u32::MAX);
        write(regs::RISC2HOST_CMD, 0);
        write(regs::HOST2RISC_CMD, 0);
        // The RISC out of reset: it loads the firmware and says so.
        let begin = mct.count();
        write(regs::SW_RESET, regs::RESET_RELEASE_ALL);
        mfc.wait_answer(regs::R2H_CMD_FW_STATUS_RET)?;
        let start_us = (mct.count() - begin) * 1000 / COUNTS_PER_MS;

        let begin = mct.count();
        mfc.command(regs::H2R_CMD_SYS_INIT, [regs::FW_BUFFER_BYTES as u32, 0, 0, 0])?;
        mfc.wait_answer(regs::R2H_CMD_SYS_INIT_RET)?;
        let init_us = (mct.count() - begin) * 1000 / COUNTS_PER_MS;
        let version = Version(read(regs::FW_VERSION));
        Ok((mfc, Started { hz, version, start_us, init_us }))
    }

    /// The ports as given to [`init`](Self::init).
    pub const fn banks(&self) -> Banks {
        self.banks
    }

    /// `s5p_mfc_reset` for v5: the RISC, then everything but the memory controller, which is
    /// then waited for, then all but the RISC out of reset.
    fn reset(&self) -> Result<(), Error> {
        write(regs::SW_RESET, regs::RESET_RISC);
        write(regs::SW_RESET, regs::RESET_ALL_BUT_MC);
        self.wait_ms(RESET_SETTLE_MS);
        let begin = self.mct.count();
        while read(regs::MC_STATUS) & 0x3 != 0 {
            if self.elapsed_ms(begin) > BUS_TIMEOUT_MS {
                return Err(Error::ResetTimeout(read(regs::MC_STATUS)));
            }
        }
        write(regs::SW_RESET, 0);
        write(regs::SW_RESET, regs::RESET_NONE_BUT_RISC);
        Ok(())
    }

    /// `s5p_mfc_cmd_host2risc_v5`: waits for the command register to be free, then sends.
    fn command(&self, command: u32, args: [u32; 4]) -> Result<(), Error> {
        let begin = self.mct.count();
        while read(regs::HOST2RISC_CMD) != regs::H2R_CMD_EMPTY {
            if self.elapsed_ms(begin) > BUS_TIMEOUT_MS {
                return Err(Error::CommandTimeout(read(regs::HOST2RISC_CMD)));
            }
        }
        write(regs::HOST2RISC_ARG1, args[0]);
        write(regs::HOST2RISC_ARG2, args[1]);
        write(regs::HOST2RISC_ARG3, args[2]);
        write(regs::HOST2RISC_ARG4, args[3]);
        write(regs::HOST2RISC_CMD, command);
        Ok(())
    }

    /// Waits for the firmware's answer, polled where Linux takes the interrupt, and clears it as
    /// `s5p_mfc_clear_int_flags_v5` does. Any other answer is an error.
    fn wait_answer(&self, want: u32) -> Result<u32, Error> {
        let begin = self.mct.count();
        let got = loop {
            let got = read(regs::RISC2HOST_CMD) & regs::RISC2HOST_CMD_MASK;
            if got != regs::R2H_CMD_EMPTY {
                break got;
            }
            if self.elapsed_ms(begin) > ANSWER_TIMEOUT_MS {
                return Err(Error::AnswerTimeout { want, got });
            }
        };
        let error = read(regs::RISC2HOST_ARG2);
        let arg1 = read(regs::RISC2HOST_ARG1);
        write(regs::RISC_HOST_INT, 0);
        write(regs::RISC2HOST_CMD, 0);
        write(regs::SI_RTN_CHID, 0xFFFF);
        if got != want {
            return Err(Error::Firmware { want, got, error });
        }
        Ok(arg1)
    }

    fn elapsed_ms(&self, begin: u64) -> u64 {
        (self.mct.count() - begin) / COUNTS_PER_MS
    }

    fn wait_ms(&self, ms: u64) {
        let begin = self.mct.count();
        while self.elapsed_ms(begin) < ms {}
    }
}

/// Whether the codec's registers answer now. See [`mfc::is_accessible`].
pub fn is_accessible() -> bool {
    mfc::is_accessible()
}
