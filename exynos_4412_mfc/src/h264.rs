// SPDX-License-Identifier: GPL-2.0-or-later
//! H.264 decoding: one instance, its answers awaited asleep until the MFC's interrupt.
//!
//! The order follows Linux 5.15.198's `s5p_mfc_try_run_v5` (`s5p_mfc_opr_v5.c`) through an
//! instance's states: `OPEN_INSTANCE` (`s5p_mfc_open_inst_cmd_v5`, `s5p_mfc_cmd_v5.c`), the
//! sequence header (`s5p_mfc_init_decode_v5`), the frame buffers (`s5p_mfc_set_dec_frame_buffer_v5`),
//! then one `FRAME_START` per picture (`s5p_mfc_decode_one_frame_v5`) and `LAST_FRAME` to empty
//! the decoder at the end. What the answers mean follows `s5p_mfc_handle_seq_done` and
//! `s5p_mfc_handle_frame` (`s5p_mfc.c`).
//!
//! The stream goes in Annex B form, start codes included: first the sequence and picture
//! parameter sets ([`Decoder::header`]), then each picture's slices in one call
//! ([`Decoder::decode`]), as Linux requires for H.264. The pictures come out in NV12MT
//! ([`crate::tiles`]), in frame buffers the decoder keeps: a picture it hands out stays the
//! caller's until [`Decoder::release`].

use core::ptr;

use exynos_4412_rs::mmu;

use crate::{Error, Mfc, read, regs, write};

/// The most frame buffers: `SI_CH0_RELEASE_BUF` has a bit for each, and there are 32 luma
/// address registers before `H264_MV_ADR`.
pub const MAX_FRAMES: usize = 32;

/// How an instance is set up.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The stream buffer, in port A: the largest picture, or the header, must fit. At most
    /// [`regs::MAX_CPB_BYTES`].
    pub stream_bytes: usize,
    /// Frame buffers beyond the ones the stream needs (`SI_BUF_NUMBER`): pictures the caller
    /// may hold while decoding goes on.
    pub extra_frames: usize,
    /// `Some(n)`: a picture is shown at most `n` pictures after it is decoded, even before the
    /// stream's reordering allows (`V4L2_CID_MPEG_MFC51_VIDEO_DECODER_H264_DISPLAY_DELAY`); 0
    /// shows each at once, right for streams that do not reorder. `None`: the stream's order.
    pub display_delay: Option<u8>,
}

impl Default for Config {
    fn default() -> Self {
        Self { stream_bytes: 1 << 20, extra_frames: 2, display_delay: None }
    }
}

/// What the sequence header says, and the frame buffers made for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequence {
    pub width: usize,
    pub height: usize,
    /// Frame buffers the stream needs (`SI_BUF_NUMBER`), and how many there are.
    pub needed: usize,
    pub frames: usize,
    /// The planes' width in bytes and their rows: the picture's size, aligned
    /// ([`regs::NV12MT_HALIGN`], [`regs::NV12MT_VALIGN`]).
    pub buf_width: usize,
    pub buf_height: usize,
    pub chroma_height: usize,
    /// Each frame buffer's planes, in bytes.
    pub luma_bytes: usize,
    pub chroma_bytes: usize,
    /// Of the header given: `SI_CONSUMED_BYTES`.
    pub consumed: u32,
}

/// A decoded picture in one of the frame buffers: its index and the planes' addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub index: usize,
    pub luma: usize,
    pub chroma: usize,
}

/// What a [`Decoder::decode`] or [`Decoder::finish`] did.
#[derive(Clone, Copy, Debug)]
pub struct Decoded {
    /// `SI_DISPLAY_STATUS` bits 0-2: [`regs::DEC_STATUS_DECODING_ONLY`] and the like;
    /// [`regs::DEC_STATUS_DECODING_EMPTY`] after [`Decoder::finish`] means nothing is left.
    pub status: u32,
    /// The picture to show now, if any. It is the caller's until [`Decoder::release`].
    pub frame: Option<Frame>,
    /// `DECODE_FRAME_TYPE`: 1 I, 2 P, 3 B, 0 none decoded.
    pub frame_type: u32,
    /// `SI_CONSUMED_BYTES`.
    pub consumed: u32,
    /// The firmware's warning, from an `ERR_RET` that still decoded the picture; 0 if none.
    pub warning: u32,
    /// Microseconds from the command to the answer: the firmware's time with [`Decoder::decode`],
    /// and whatever the caller did in between with [`Decoder::start`] and [`Decoder::wait`].
    pub us: u64,
}

/// An H.264 decoding instance. Its memory comes from the ports of the [`Mfc`] it borrows and
/// goes back to it when it is dropped, the instance closed.
pub struct Decoder<'a> {
    mfc: &'a mut Mfc,
    mark: (usize, usize),
    instance: u32,
    config: Config,
    desc: usize,
    shared: usize,
    stream: usize,
    sequence: Option<Sequence>,
    luma: [usize; MAX_FRAMES],
    chroma: [usize; MAX_FRAMES],
    /// Frame buffers the caller holds: a bit for each.
    held: u32,
    /// When the picture being decoded was started, in MCT counts.
    running: Option<u64>,
}

/// Zeroes `len` bytes at `start` and writes them out to DRAM.
fn zero(start: usize, len: usize) {
    // SAFETY: the callers pass memory just taken from a port, which nothing else uses.
    unsafe { ptr::write_bytes(start as *mut u8, 0, len) };
    mmu::clean_dcache_range(start, len);
}

const COUNTS_PER_US: u64 = exynos_4412_rs::soc::MCT_CLOCK_HZ as u64 / 1_000_000;

impl<'a> Decoder<'a> {
    /// Takes the instance's buffers from the ports (`s5p_mfc_alloc_instance_buffer_v5`,
    /// `s5p_mfc_alloc_dec_temp_buffers_v5`) and opens it.
    pub fn open(mfc: &'a mut Mfc, config: Config) -> Result<Self, Error> {
        if config.stream_bytes == 0 || config.stream_bytes > regs::MAX_CPB_BYTES {
            return Err(Error::BadStream(config.stream_bytes));
        }
        let mark = mfc.mark();
        let taken = (|| {
            let context = mfc.take(false, regs::H264_CTX_BYTES)?;
            let desc = mfc.take(false, regs::DESC_BUF_BYTES)?;
            let shared = mfc.take(false, regs::SHARED_BUF_BYTES)?;
            let stream = mfc.take(false, config.stream_bytes)?;
            Ok((context, desc, shared, stream))
        })();
        let (context, desc, shared, stream) = match taken {
            Ok(taken) => taken,
            Err(error) => {
                mfc.give_back(mark);
                return Err(error);
            }
        };
        zero(context, regs::H264_CTX_BYTES);
        zero(desc, regs::DESC_BUF_BYTES);
        zero(shared, regs::SHARED_BUF_BYTES);

        let args = [regs::CODEC_H264_DEC, 0, mfc.offset_a(context), regs::H264_CTX_BYTES as u32];
        let opened = mfc.command(regs::H2R_CMD_OPEN_INSTANCE, args).and_then(|()| mfc.wait_answer(regs::R2H_CMD_OPEN_INSTANCE_RET));
        let instance = match opened {
            Ok(instance) => instance,
            Err(error) => {
                mfc.give_back(mark);
                return Err(error);
            }
        };
        Ok(Self {
            mfc,
            mark,
            instance,
            config,
            desc,
            shared,
            stream,
            sequence: None,
            luma: [0; MAX_FRAMES],
            chroma: [0; MAX_FRAMES],
            held: 0,
            running: None,
        })
    }

    /// The firmware's number for this instance.
    pub const fn instance(&self) -> u32 {
        self.instance
    }

    /// What [`header`](Self::header) found.
    pub const fn sequence(&self) -> Option<Sequence> {
        self.sequence
    }

    /// Parses the sequence header (`SEQ_HEADER`), then makes the frame buffers it asks for and
    /// gives them to the firmware (`INIT_BUFS`). `data` is the sequence and picture parameter
    /// sets, with their start codes.
    pub fn header(&mut self, data: &[u8]) -> Result<Sequence, Error> {
        if self.running.is_some() {
            return Err(Error::Busy);
        }
        self.load(data)?;
        self.set_stream(0, data.len());
        write(regs::ENC_LF_CTRL, 0);
        let delay = match self.config.display_delay {
            Some(frames) => regs::DDELAY_ENABLE | (u32::from(frames) & regs::DDELAY_VAL_MASK) << regs::DDELAY_VAL_SHIFT,
            None => 0,
        };
        write(regs::SI_CH0_DPB_CONF_CTRL, delay);
        self.start_command(regs::CH_SEQ_HEADER);
        self.mfc.wait_answer(regs::R2H_CMD_SEQ_DONE_RET)?;

        let width = read(regs::SI_HRESOL);
        let height = read(regs::SI_VRESOL);
        let needed = read(regs::SI_BUF_NUMBER);
        let consumed = read(regs::SI_CONSUMED_BYTES);
        let frames = needed as usize + self.config.extra_frames;
        if width == 0 || height == 0 || width > 4096 || height > 4096 || frames > MAX_FRAMES {
            return Err(Error::BadSequence { width, height, buffers: needed });
        }
        let (width, height) = (width as usize, height as usize);

        // s5p_mfc_dec_calc_dpb_size_v5.
        let buf_width = width.next_multiple_of(regs::NV12MT_HALIGN);
        let buf_height = height.next_multiple_of(regs::NV12MT_VALIGN);
        let chroma_height = (height >> 1).next_multiple_of(regs::NV12MT_VALIGN);
        let luma_bytes = (buf_width * buf_height).next_multiple_of(regs::DEC_BUF_ALIGN);
        let chroma_bytes = (buf_width * chroma_height).next_multiple_of(regs::DEC_BUF_ALIGN);
        let mv_bytes = (buf_width * (buf_height >> 2).next_multiple_of(regs::NV12MT_VALIGN)).next_multiple_of(regs::DEC_BUF_ALIGN);

        // s5p_mfc_alloc_codec_buffers_v5: bank 1 in port A, bank 2 (the motion vectors) in B;
        // then the frame buffers, chroma in A and luma in B (s5p_mfc_queue_setup, s5p_mfc_dec.c).
        // Taken anew after a header given twice.
        self.mfc.give_back(self.after_instance());
        let bank1 = self.mfc.take(false, regs::DEC_VERT_NB_MV_BYTES + regs::DEC_NB_IP_BYTES)?;
        let bank2 = self.mfc.take(true, frames * mv_bytes)?;
        for i in 0..frames {
            self.chroma[i] = self.mfc.take(false, chroma_bytes)?;
            self.luma[i] = self.mfc.take(true, luma_bytes)?;
            // The firmware writes them from now on: no dirty line may be written back over it.
            mmu::discard_dcache_range(self.chroma[i], chroma_bytes);
            mmu::discard_dcache_range(self.luma[i], luma_bytes);
        }
        let sequence = Sequence {
            width,
            height,
            needed: needed as usize,
            frames,
            buf_width,
            buf_height,
            chroma_height,
            luma_bytes,
            chroma_bytes,
            consumed,
        };

        // s5p_mfc_run_init_dec_buffers: the header again as the stream, then the buffers.
        self.set_stream(0, data.len());
        let dpb = read(regs::SI_CH0_DPB_CONF_CTRL) & !regs::DPB_COUNT_MASK;
        write(regs::SI_CH0_DPB_CONF_CTRL, dpb | frames as u32);
        write(regs::H264_VERT_NB_MV_ADR, self.mfc.offset_a(bank1));
        write(regs::H264_NB_IP_ADR, self.mfc.offset_a(bank1 + regs::DEC_VERT_NB_MV_BYTES));
        for i in 0..frames {
            write(regs::DEC_LUMA_ADR + 4 * i, self.mfc.offset_b(self.luma[i]));
            write(regs::DEC_CHROMA_ADR + 4 * i, self.mfc.offset_a(self.chroma[i]));
            write(regs::H264_MV_ADR + 4 * i, self.mfc.offset_b(bank2 + i * mv_bytes));
        }
        self.write_shared(regs::SHARED_LUMA_DPB_SIZE, luma_bytes as u32);
        self.write_shared(regs::SHARED_CHROMA_DPB_SIZE, chroma_bytes as u32);
        self.write_shared(regs::SHARED_MV_SIZE, mv_bytes as u32);
        self.start_command(regs::CH_INIT_BUFS);
        self.mfc.wait_answer(regs::R2H_CMD_INIT_BUFFERS_RET)?;
        self.held = 0;
        self.sequence = Some(sequence);
        Ok(sequence)
    }

    /// Decodes one picture: `data` is all its slices, with their start codes.
    pub fn decode(&mut self, data: &[u8]) -> Result<Decoded, Error> {
        self.start(data)?;
        self.wait()
    }

    /// At the end of the stream: one picture still held back by the decoder, if any (`LAST_FRAME`
    /// with an empty stream, `s5p_mfc_run_dec_frame`). Call it until the status is
    /// [`regs::DEC_STATUS_DECODING_EMPTY`].
    pub fn finish(&mut self) -> Result<Decoded, Error> {
        self.start_finish()?;
        self.wait()
    }

    /// [`decode`](Self::decode) in two halves: this one starts the firmware on the picture and
    /// returns, so the CPU can do other work meanwhile, such as drawing a picture it holds;
    /// [`wait`](Self::wait) is the other.
    pub fn start(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.running.is_some() {
            return Err(Error::Busy);
        }
        if data.is_empty() {
            return Err(Error::BadStream(0));
        }
        self.load(data)?;
        self.set_stream(0, data.len());
        self.begin(regs::CH_FRAME_START)
    }

    /// [`finish`](Self::finish)'s first half: see [`start`](Self::start).
    pub fn start_finish(&mut self) -> Result<(), Error> {
        if self.running.is_some() {
            return Err(Error::Busy);
        }
        self.set_stream(0, 0);
        self.begin(regs::CH_LAST_FRAME)
    }

    /// Whether a picture is being decoded: [`start`](Self::start) without [`wait`](Self::wait) yet.
    pub const fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Gives a frame buffer back to the decoder.
    pub fn release(&mut self, frame: Frame) {
        self.held &= !(1 << frame.index);
    }

    /// The planes of a picture handed out, as the firmware wrote them: [`crate::tiles`].
    pub fn planes(&self, frame: &Frame) -> (&[u8], &[u8]) {
        let sequence = self.sequence.expect("a frame comes after the header");
        // SAFETY: both are frame buffers of this instance, taken from the ports for it, which the
        // firmware does not write while the caller holds them (their bit in RELEASE_BUF is clear),
        // even while it decodes another picture.
        // `release` needs `&mut self`, so these borrows end first.
        unsafe {
            (
                core::slice::from_raw_parts(frame.luma as *const u8, sequence.luma_bytes),
                core::slice::from_raw_parts(frame.chroma as *const u8, sequence.chroma_bytes),
            )
        }
    }

    /// Sends an instance command for a picture.
    fn begin(&mut self, command: u32) -> Result<(), Error> {
        let Some(sequence) = self.sequence else {
            return Err(Error::BadStream(0));
        };
        // Frame buffers the caller holds stay out: the firmware writes none of them.
        let all = if sequence.frames == MAX_FRAMES { u32::MAX } else { (1 << sequence.frames) - 1 };
        write(regs::SI_CH0_RELEASE_BUF, all & !self.held);
        let dpb = read(regs::SI_CH0_DPB_CONF_CTRL) & !regs::DPB_FLUSH;
        write(regs::SI_CH0_DPB_CONF_CTRL, dpb);
        self.running = Some(self.mfc.mct().count());
        self.start_command(command);
        Ok(())
    }

    /// Waits for the picture [`start`](Self::start) or [`start_finish`](Self::start_finish) sent
    /// and reads what came of it, as `s5p_mfc_handle_frame`.
    pub fn wait(&mut self) -> Result<Decoded, Error> {
        let (Some(begin), Some(sequence)) = (self.running.take(), self.sequence) else {
            return Err(Error::NotRunning);
        };
        let answer = self.mfc.answer(regs::R2H_CMD_FRAME_DONE_RET)?;
        let us = (self.mfc.mct().count() - begin) / COUNTS_PER_US;
        let warning = match answer.got {
            regs::R2H_CMD_FRAME_DONE_RET => 0,
            regs::R2H_CMD_ERR_RET if is_warning(answer.error) => answer.error,
            got => return Err(Error::Firmware { want: regs::R2H_CMD_FRAME_DONE_RET, got, error: answer.error }),
        };
        let status = read(regs::SI_DISPLAY_STATUS) & regs::DEC_STATUS_MASK;
        let frame_type = read(regs::DECODE_FRAME_TYPE) & regs::DECODE_FRAME_MASK;
        let consumed = read(regs::SI_CONSUMED_BYTES);
        let frame = match status {
            regs::DEC_STATUS_DECODING_DISPLAY | regs::DEC_STATUS_DISPLAY_ONLY => {
                let frame = self.display_frame(&sequence)?;
                self.held |= 1 << frame.index;
                // Lines the CPU pulled in while the firmware wrote are stale.
                mmu::invalidate_dcache_range(frame.luma, sequence.luma_bytes);
                mmu::invalidate_dcache_range(frame.chroma, sequence.chroma_bytes);
                Some(frame)
            }
            _ => None,
        };
        Ok(Decoded { status, frame, frame_type, consumed, warning, us })
    }

    /// The frame buffer `SI_DISPLAY_Y_ADR` names. Linux compares it, shifted, with the buffer's
    /// address; an offset from port B's base is taken too.
    fn display_frame(&self, sequence: &Sequence) -> Result<Frame, Error> {
        let raw = read(regs::SI_DISPLAY_Y_ADR);
        let address = (raw as usize) << regs::OFFSET_SHIFT;
        let base_b = self.mfc.banks().b;
        (0..sequence.frames)
            .find(|&i| self.luma[i] == address || self.luma[i] - base_b == address)
            .map(|index| Frame { index, luma: self.luma[index], chroma: self.chroma[index] })
            .ok_or(Error::UnknownFrame(raw))
    }

    /// Copies `data` into the stream buffer and out to DRAM.
    fn load(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() || data.len() > self.config.stream_bytes {
            return Err(Error::BadStream(data.len()));
        }
        // SAFETY: the stream buffer is this instance's, `stream_bytes` long; the firmware reads
        // it only during a command, and none is running.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), self.stream as *mut u8, data.len()) };
        mmu::clean_dcache_range(self.stream, data.len());
        Ok(())
    }

    /// `s5p_mfc_set_dec_stream_buffer_v5`.
    fn set_stream(&mut self, start_byte: u32, len: usize) {
        write(regs::SI_CH0_SB_ST_ADR, self.mfc.offset_a(self.stream));
        write(regs::SI_CH0_CPB_SIZE, self.config.stream_bytes as u32);
        write(regs::SI_CH0_SB_FRM_SIZE, len as u32);
        self.write_shared(regs::SHARED_START_BYTE_NUM, start_byte);
    }

    /// `s5p_mfc_set_dec_desc_buffer` and `s5p_mfc_set_shared_buffer`, as `s5p_mfc_try_run_v5` does
    /// before each run, then the command.
    fn start_command(&self, command: u32) {
        write(regs::SI_CH0_DESC_ADR, self.mfc.offset_a(self.desc));
        write(regs::SI_CH0_DESC_SIZE, regs::DESC_BUF_BYTES as u32);
        write(regs::SI_CH0_HOST_WR_ADR, (self.shared - self.mfc.banks().a) as u32);
        write(regs::SI_CH0_INST_ID, (command & regs::CH_MASK) << regs::CH_SHIFT | self.instance);
    }

    /// `s5p_mfc_write_info_v5`. Linux's shared memory is uncached; here its line is dropped first,
    /// in case the firmware wrote other words of it, and written out after.
    fn write_shared(&self, offset: usize, value: u32) {
        let address = self.shared + offset;
        let line = address & !(mmu::DCACHE_LINE - 1);
        mmu::invalidate_dcache_range(line, mmu::DCACHE_LINE);
        // SAFETY: a word of the instance's shared memory, which the firmware does not touch while
        // no command runs.
        unsafe { ptr::write_volatile(address as *mut u32, value) };
        mmu::clean_dcache_range(line, mmu::DCACHE_LINE);
    }

    /// The free memory right after the instance's own buffers.
    fn after_instance(&self) -> (usize, usize) {
        ((self.stream + self.config.stream_bytes).next_multiple_of(regs::DEC_BUF_ALIGN), self.mark.1)
    }
}

/// `s5p_mfc.c`, `S5P_MFC_R2H_CMD_ERR_RET`: errors after which the picture was decoded anyway.
fn is_warning(error: u32) -> bool {
    let error = error & regs::ERR_DEC_MASK;
    error >= regs::ERR_WARNINGS_START || matches!(error, regs::ERR_NO_VALID_SEQ_HDR | regs::ERR_INCOMPLETE_FRAME | regs::ERR_TIMEOUT)
}

impl Drop for Decoder<'_> {
    /// Closes the instance (`s5p_mfc_close_inst_cmd_v5`) and gives its memory back.
    fn drop(&mut self) {
        if self.running.is_some() {
            let _ = self.wait();
        }
        let closed = self.mfc.command(regs::H2R_CMD_CLOSE_INSTANCE, [self.instance, 0, 0, 0]);
        if closed.is_ok() {
            let _ = self.mfc.wait_answer(regs::R2H_CMD_CLOSE_INSTANCE_RET);
        }
        self.mfc.give_back(self.mark);
    }
}
