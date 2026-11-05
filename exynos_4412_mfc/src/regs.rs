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
/// `:51-63`: a decoder's buffers, as offsets from their port's base ([`OFFSET_SHIFT`]). One
/// register per frame buffer, from these, four bytes apart.
pub const DEC_CHROMA_ADR: usize = 0x0600;
pub const DEC_LUMA_ADR: usize = 0x0700;
pub const H264_VERT_NB_MV_ADR: usize = 0x068C;
pub const H264_NB_IP_ADR: usize = 0x0690;
pub const H264_MV_ADR: usize = 0x0780;
/// `:151`: the loop filter, written 0 when decoding H.264.
pub const ENC_LF_CTRL: usize = 0x0848;

/// `:158-160`.
pub const SI_RTN_CHID: usize = 0x2000;
pub const SI_CH0_INST_ID: usize = 0x2040;
pub const SI_CH1_INST_ID: usize = 0x2080;
/// `:162-175`: what the firmware found in the stream, and the picture it decoded or shows.
pub const SI_VRESOL: usize = 0x2004;
pub const SI_HRESOL: usize = 0x2008;
pub const SI_BUF_NUMBER: usize = 0x200C;
pub const SI_DISPLAY_Y_ADR: usize = 0x2010;
pub const SI_DISPLAY_C_ADR: usize = 0x2014;
pub const SI_CONSUMED_BYTES: usize = 0x2018;
pub const SI_DISPLAY_STATUS: usize = 0x201C;
pub const DECODE_FRAME_TYPE: usize = 0x2020;
pub const SI_DECODE_STATUS: usize = 0x202C;
/// `:177-181`: channel 0's stream and descriptor buffers.
pub const SI_CH0_SB_ST_ADR: usize = 0x2044;
pub const SI_CH0_SB_FRM_SIZE: usize = 0x2048;
pub const SI_CH0_DESC_ADR: usize = 0x204C;
pub const SI_CH0_CPB_SIZE: usize = 0x2058;
pub const SI_CH0_DESC_SIZE: usize = 0x205C;
/// `:318-330`.
pub const SI_CH0_DPB_CONF_CTRL: usize = 0x2068;
pub const SI_CH0_RELEASE_BUF: usize = 0x2060;
pub const SI_CH0_HOST_WR_ADR: usize = 0x2064;

/// `SI_CH0_DPB_CONF_CTRL` fields (`:319-326`).
pub const DPB_COUNT_MASK: u32 = 0xFFFF;
pub const DPB_FLUSH: u32 = 1 << 14;
pub const DDELAY_VAL_SHIFT: u32 = 16;
pub const DDELAY_VAL_MASK: u32 = 0xFF;
pub const DDELAY_ENABLE: u32 = 1 << 30;

/// `:199-203`: `SI_DISPLAY_STATUS` and `SI_DECODE_STATUS`, bits 0-2.
pub const DEC_STATUS_MASK: u32 = 7;
pub const DEC_STATUS_DECODING_ONLY: u32 = 0;
pub const DEC_STATUS_DECODING_DISPLAY: u32 = 1;
pub const DEC_STATUS_DISPLAY_ONLY: u32 = 2;
pub const DEC_STATUS_DECODING_EMPTY: u32 = 3;
/// `:225`.
pub const DECODE_FRAME_MASK: u32 = 7;

/// `:335`: `OPEN_INSTANCE`'s codec.
pub const CODEC_H264_DEC: u32 = 0;

/// `:347-353`: commands to an instance, through `SI_CH0_INST_ID`.
pub const CH_SEQ_HEADER: u32 = 1;
pub const CH_FRAME_START: u32 = 2;
pub const CH_LAST_FRAME: u32 = 3;
pub const CH_INIT_BUFS: u32 = 4;
pub const CH_MASK: u32 = 7;
pub const CH_SHIFT: u32 = 16;

/// `:357-358,360`: host to firmware.
pub const H2R_CMD_EMPTY: u32 = 0;
pub const H2R_CMD_OPEN_INSTANCE: u32 = 1;
pub const H2R_CMD_CLOSE_INSTANCE: u32 = 2;
pub const H2R_CMD_SYS_INIT: u32 = 3;

/// `:365-380`: firmware to host.
pub const R2H_CMD_EMPTY: u32 = 0;
pub const R2H_CMD_OPEN_INSTANCE_RET: u32 = 1;
pub const R2H_CMD_CLOSE_INSTANCE_RET: u32 = 2;
pub const R2H_CMD_SEQ_DONE_RET: u32 = 4;
pub const R2H_CMD_FRAME_DONE_RET: u32 = 5;
pub const R2H_CMD_SYS_INIT_RET: u32 = 8;
pub const R2H_CMD_FW_STATUS_RET: u32 = 9;
pub const R2H_CMD_INIT_BUFFERS_RET: u32 = 15;
pub const R2H_CMD_ERR_RET: u32 = 32;

/// `:393-397`: error codes in `RISC2HOST_ARG2`, the decoding one in bits 0-15. From
/// `ERR_WARNINGS_START` on, the frame was decoded anyway (`s5p_mfc.c`, `S5P_MFC_R2H_CMD_ERR_RET`),
/// as for the three before it.
pub const ERR_NO_VALID_SEQ_HDR: u32 = 67;
pub const ERR_INCOMPLETE_FRAME: u32 = 124;
pub const ERR_TIMEOUT: u32 = 140;
pub const ERR_WARNINGS_START: u32 = 145;
pub const ERR_DEC_MASK: u32 = 0xFFFF;

/// `:406-423`: words of the shared memory, by byte offset.
pub const SHARED_START_BYTE_NUM: usize = 0x18;
pub const SHARED_LUMA_DPB_SIZE: usize = 0x64;
pub const SHARED_CHROMA_DPB_SIZE: usize = 0x68;
pub const SHARED_MV_SIZE: usize = 0x6C;

/// `:435`: buffer addresses go to the firmware as offsets from their port's base, shifted by this.
pub const OFFSET_SHIFT: u32 = 11;
/// `:440-444`: the descriptor and shared memory buffers, and the largest coded picture.
pub const DESC_BUF_BYTES: usize = 128 * 1024;
pub const SHARED_BUF_BYTES: usize = 8 * 1024;
pub const MAX_CPB_BYTES: usize = 4 << 20;
/// `:438`: an H.264 instance's context.
pub const H264_CTX_BYTES: usize = 600 * 1024;
/// `:234-235,243`: an H.264 decoder's two neighbour buffers, at the start of its bank 1, and the
/// alignment of all of a decoder's buffers.
pub const DEC_NB_IP_BYTES: usize = 32 * 1024;
pub const DEC_VERT_NB_MV_BYTES: usize = 16 * 1024;
pub const DEC_BUF_ALIGN: usize = 8 * 1024;
/// `:248-249`: frame buffers' width and height are aligned to these.
pub const NV12MT_HALIGN: usize = 128;
pub const NV12MT_VALIGN: usize = 32;

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
