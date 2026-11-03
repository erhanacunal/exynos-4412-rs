//! WAV files: the RIFF header of uncompressed PCM, and its samples.
//!
//! A WAV file is a RIFF file of form `WAVE`: a `fmt ` chunk with the format, and a `data` chunk
//! with the samples, interleaved by channel, little-endian. Other chunks, such as `LIST`, are
//! skipped. Only PCM (format tag 1) is taken, at 8 to 32 bits a sample.
//!
//! ```ignore
//! let wav = Wav::parse(bytes)?;
//! if wav.bits == 16 {
//!     for frame in wav.frames_i16() { /* frame[0], frame[1] ... */ }
//! }
//! ```

/// Why a file was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a RIFF file of form `WAVE`.
    NotWave,
    /// A chunk runs past the end of the file.
    Truncated,
    /// No `fmt ` chunk before the `data` chunk, or no `data` chunk.
    MissingChunk,
    /// Not PCM: the format tag.
    NotPcm(u16),
    /// A format this does not take: no channels, or a sample size not a whole number of bytes.
    BadFormat,
}

/// The format and the samples of a PCM WAV file.
#[derive(Clone, Copy, Debug)]
pub struct Wav<'a> {
    /// Channels, interleaved in each frame.
    pub channels: u16,
    /// Frames a second.
    pub sample_rate: u32,
    /// Bits of each sample.
    pub bits: u16,
    /// The `data` chunk, whole frames only.
    pub data: &'a [u8],
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl<'a> Wav<'a> {
    /// Reads the header of `bytes`, a whole file.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(Error::NotWave);
        }
        // The RIFF size may be wrong in files written while recording: the chunks decide.
        let mut at = 12;
        let mut format = None;
        while at + 8 <= bytes.len() {
            let id = &bytes[at..at + 4];
            let size = u32_at(bytes, at + 4) as usize;
            let body = at + 8;
            let end = body.checked_add(size).ok_or(Error::Truncated)?;
            if id == b"fmt " {
                if size < 16 || end > bytes.len() {
                    return Err(Error::Truncated);
                }
                format = Some((u16_at(bytes, body), u16_at(bytes, body + 2), u32_at(bytes, body + 4), u16_at(bytes, body + 14)));
            } else if id == b"data" {
                let (tag, channels, sample_rate, bits) = format.ok_or(Error::MissingChunk)?;
                if tag != 1 {
                    return Err(Error::NotPcm(tag));
                }
                if channels == 0 || bits == 0 || bits % 8 != 0 || bits > 32 {
                    return Err(Error::BadFormat);
                }
                // A file cut short keeps the frames it has.
                let data = &bytes[body..end.min(bytes.len())];
                let frame = usize::from(channels) * usize::from(bits / 8);
                let data = &data[..data.len() - data.len() % frame];
                return Ok(Self { channels, sample_rate, bits, data });
            }
            // Chunks are padded to an even size.
            at = end + (size & 1);
        }
        Err(Error::MissingChunk)
    }

    /// Bytes of one frame.
    pub const fn frame_bytes(&self) -> usize {
        self.channels as usize * (self.bits as usize / 8)
    }

    /// Frames in the file.
    pub const fn frames(&self) -> usize {
        self.data.len() / self.frame_bytes()
    }

    /// The sample of `channel` in frame `index`, for 16-bit files. `None` past the end, or if
    /// the file is not 16-bit.
    pub fn sample_i16(&self, index: usize, channel: u16) -> Option<i16> {
        if self.bits != 16 || channel >= self.channels || index >= self.frames() {
            return None;
        }
        let at = index * self.frame_bytes() + usize::from(channel) * 2;
        Some(i16::from_le_bytes([self.data[at], self.data[at + 1]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(fmt: &[u8], data: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[0..4].copy_from_slice(b"RIFF");
        out[8..12].copy_from_slice(b"WAVE");
        out[12..16].copy_from_slice(b"fmt ");
        out[16..20].copy_from_slice(&(fmt.len() as u32).to_le_bytes());
        out[20..20 + fmt.len()].copy_from_slice(fmt);
        let at = 20 + fmt.len();
        out[at..at + 4].copy_from_slice(b"data");
        out[at + 4..at + 8].copy_from_slice(&(data.len() as u32).to_le_bytes());
        out[at + 8..at + 8 + data.len()].copy_from_slice(data);
        out
    }

    #[test]
    fn mono_16_bit() {
        let fmt = [1, 0, 1, 0, 0x80, 0xBB, 0, 0, 0, 0x77, 1, 0, 2, 0, 16, 0];
        let bytes = file(&fmt, &[1, 0, 0xFF, 0xFF, 7]);
        let wav = Wav::parse(&bytes).unwrap();
        assert_eq!((wav.channels, wav.sample_rate, wav.bits, wav.frames()), (1, 48000, 16, 2));
        assert_eq!(wav.sample_i16(0, 0), Some(1));
        assert_eq!(wav.sample_i16(1, 0), Some(-1));
        assert_eq!(wav.sample_i16(2, 0), None);
    }
}
