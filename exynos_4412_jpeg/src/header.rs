// SPDX-License-Identifier: GPL-2.0-only
//! What the codec needs to know of a JPEG stream before decoding it, as `s5p_jpeg_parse_hdr`
//! finds it (Linux 5.15.198, `jpeg-core.c:1111-1242`, Copyright (c) 2011-2014 Samsung
//! Electronics Co., Ltd.): the size and the subsampling of a baseline (`SOF0`) picture. The codec
//! itself reads the tables from the stream.

use crate::Subsampling;

const SOF0: u8 = 0xC0;
const DHT: u8 = 0xC4;
const RST0: u8 = 0xD0;
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const TEM: u8 = 0x01;

/// A baseline JPEG picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
    pub subsampling: Subsampling,
    /// 1 or 3.
    pub components: u8,
    /// Where the scan's `SOS` marker is.
    pub sos: usize,
}

/// Why a stream is not one the codec decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// It ends before both `SOF0` and `SOS`: also what a progressive or other non-baseline
    /// picture gives, since only `SOF0` counts.
    Truncated,
    /// A marker segment of length 2, empty.
    EmptySegment(u8),
    /// More than three components.
    Components(u8),
    /// The first component's sampling factors, `0xHV`, are not 4:4:4, 4:2:2 or 4:2:0.
    Sampling(u8),
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn word(&mut self) -> Option<usize> {
        Some(usize::from(self.byte()?) << 8 | usize::from(self.byte()?))
    }

    fn skip(&mut self, count: usize) {
        self.at = self.at.saturating_add(count);
    }
}

/// Finds the picture's size and subsampling.
pub fn parse(stream: &[u8]) -> Result<Header, HeaderError> {
    let mut reader = Reader { data: stream, at: 0 };
    let mut frame: Option<(usize, usize, u8, u8)> = None;
    let mut sos = None;
    while frame.is_none() || sos.is_none() {
        let Some(byte) = reader.byte() else {
            return Err(HeaderError::Truncated);
        };
        if byte != 0xFF {
            continue;
        }
        let marker = loop {
            match reader.byte() {
                Some(0xFF) => {}
                Some(marker) => break marker,
                None => return Err(HeaderError::Truncated),
            }
        };
        match marker {
            0 | SOI | EOI | TEM => {}
            RST0..=0xD7 => {}
            SOS => sos = Some(reader.at - 2),
            _ => {
                let Some(length) = reader.word() else {
                    return Err(HeaderError::Truncated);
                };
                let length = length.saturating_sub(2);
                if length == 0 && matches!(marker, SOF0 | DHT | DQT) {
                    return Err(HeaderError::EmptySegment(marker));
                }
                let next = reader.at + length;
                if marker == SOF0 {
                    reader.skip(1); // precision
                    let (Some(height), Some(width), Some(components)) = (reader.word(), reader.word(), reader.byte()) else {
                        return Err(HeaderError::Truncated);
                    };
                    if components > 3 {
                        return Err(HeaderError::Components(components));
                    }
                    let sampling = if components == 1 {
                        0x33
                    } else {
                        reader.skip(1); // the first component's identifier
                        reader.byte().ok_or(HeaderError::Truncated)?
                    };
                    frame = Some((width, height, components, sampling));
                }
                reader.at = next;
            }
        }
    }
    let (width, height, components, sampling) = frame.ok_or(HeaderError::Truncated)?;
    // jpeg-core.c:1073-1109, the Exynos 4x12's part.
    let subsampling = match sampling {
        0x11 => Subsampling::S444,
        0x21 => Subsampling::S422,
        0x22 => Subsampling::S420,
        0x33 => Subsampling::Gray,
        other => return Err(HeaderError::Sampling(other)),
    };
    Ok(Header { width, height, subsampling, components, sos: sos.ok_or(HeaderError::Truncated)? })
}
