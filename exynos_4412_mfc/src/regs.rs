// SPDX-License-Identifier: GPL-2.0-or-later
//! Registers of the MFC v5.1 and the values the firmware exchanges through them.
//!
//! From Linux 5.15.198, `drivers/media/platform/s5p-mfc/regs-mfc.h` (cited by line), Copyright
//! (C) 2011 Samsung Electronics Co., Ltd.

/// `:23`: the reset of the RISC and the blocks, one bit each.
pub const SW_RESET: usize = 0x0000;
/// `:24`.
pub const RISC_HOST_INT: usize = 0x0008;
/// `:27-31`: a command to the firmware and its four arguments.
pub const HOST2RISC_CMD: usize = 0x0030;
pub const HOST2RISC_ARG1: usize = 0x0034;
pub const HOST2RISC_ARG2: usize = 0x0038;
pub const HOST2RISC_ARG3: usize = 0x003C;
pub const HOST2RISC_ARG4: usize = 0x0040;
/// `:34-39`: what the firmware answers, and its arguments.
pub const RISC2HOST_CMD: usize = 0x0044;
pub const RISC2HOST_CMD_MASK: u32 = 0x1FFFF;
pub const RISC2HOST_ARG1: usize = 0x0048;
pub const RISC2HOST_ARG2: usize = 0x004C;
/// `:41`: `0xYYMMDD`, once the firmware has started.
pub const FW_VERSION: usize = 0x0058;
/// `:46-48`: where the two memory ports' address spaces start, and the memory controller's
/// busy bits.
pub const MC_DRAMBASE_ADR_A: usize = 0x0508;
pub const MC_DRAMBASE_ADR_B: usize = 0x050C;
pub const MC_STATUS: usize = 0x0510;
/// `:158-160`.
pub const SI_RTN_CHID: usize = 0x2000;
pub const SI_CH0_INST_ID: usize = 0x2040;
pub const SI_CH1_INST_ID: usize = 0x2080;

/// `:357,360`: host to firmware.
pub const H2R_CMD_EMPTY: u32 = 0;
pub const H2R_CMD_SYS_INIT: u32 = 3;

/// `:373-374,380`: firmware to host.
pub const R2H_CMD_EMPTY: u32 = 0;
pub const R2H_CMD_SYS_INIT_RET: u32 = 8;
pub const R2H_CMD_FW_STATUS_RET: u32 = 9;
pub const R2H_CMD_ERR_RET: u32 = 32;

/// `:445`: the firmware's buffer, at the start of port A; `SYS_INIT` takes this size, not the
/// file's.
pub const FW_BUFFER_BYTES: usize = 384 * 1024;
/// `s5p_mfc_common.h:38`: both ports' base addresses are aligned to this.
pub const BASE_ALIGN: usize = 1 << 17;

/// `SW_RESET` values of `s5p_mfc_reset` and `s5p_mfc_init_hw` (`s5p_mfc_ctrl.c`).
pub const RESET_RISC: u32 = 0x3F6;
pub const RESET_ALL_BUT_MC: u32 = 0x3E2;
pub const RESET_NONE_BUT_RISC: u32 = 0x3FE;
pub const RESET_RELEASE_ALL: u32 = 0x3FF;
