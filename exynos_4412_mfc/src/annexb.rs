// SPDX-License-Identifier: GPL-2.0-or-later
//! Cutting an H.264 stream in Annex B form (start codes) into what [`crate::h264::Decoder`]
//! takes: the parameter sets before the first slice ([`header`]), then one access unit, a whole
//! picture, at a time ([`pictures`]). This is the H.264 standard's own structure (7.4.1.2.3),
//! nothing of Linux's.

/// NAL unit types: a slice of a picture, not IDR and IDR.
const NAL_SLICE: u8 = 1;
const NAL_IDR_SLICE: u8 = 5;

/// The NAL units of an Annex B stream: each with its start code, up to the next one, and its
/// type.
#[derive(Clone)]
pub struct Nals<'a> {
    data: &'a [u8],
    at: usize,
}

/// One NAL unit: where it is in the stream, start code included, and its type.
#[derive(Clone, Copy, Debug)]
pub struct Nal {
    pub start: usize,
    pub end: usize,
    pub kind: u8,
    /// The byte after the header: the first of a slice's header.
    first: u8,
}

impl Nal {
    const fn is_slice(&self) -> bool {
        self.kind == NAL_SLICE || self.kind == NAL_IDR_SLICE
    }

    /// A slice whose `first_mb_in_slice`, ue(v), is 0, coded as a lone 1 bit: the first slice of
    /// a picture.
    const fn starts_picture(&self) -> bool {
        self.is_slice() && self.first & 0x80 != 0
    }
}

impl<'a> Nals<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, at: next_start(data, 0) }
    }
}

/// Where the start code at or after `from` begins, a leading zero byte included.
fn next_start(data: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return if i > from && data[i - 1] == 0 { i - 1 } else { i };
        }
        i += 1;
    }
    data.len()
}

impl Iterator for Nals<'_> {
    type Item = Nal;

    fn next(&mut self) -> Option<Nal> {
        if self.at + 3 > self.data.len() {
            return None;
        }
        let start = self.at;
        let header = start + if self.data[start + 2] == 1 { 3 } else { 4 };
        let end = next_start(self.data, header);
        self.at = end;
        let byte = |i: usize| self.data.get(i).copied().unwrap_or(0);
        Some(Nal { start, end, kind: byte(header) & 0x1F, first: byte(header + 1) })
    }
}

/// Everything before the first slice: the sequence and picture parameter sets, and whatever
/// else comes with them.
pub fn header(stream: &[u8]) -> &[u8] {
    let end = Nals::new(stream).find(Nal::is_slice).map_or(stream.len(), |nal| nal.start);
    &stream[..end]
}

/// The access units after the header, each from its first NAL unit to its last slice: a new one
/// begins with a NAL unit that is not a slice, or with a picture's first slice, once a slice
/// has come.
pub fn pictures(stream: &[u8]) -> Pictures<'_> {
    let header = header(stream).len();
    Pictures { data: stream, nals: Nals { data: stream, at: header } }
}

/// See [`pictures`].
pub struct Pictures<'a> {
    data: &'a [u8],
    nals: Nals<'a>,
}

impl<'a> Iterator for Pictures<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let mut start = None;
        let mut end = 0;
        let mut slices = false;
        loop {
            let mut ahead = self.nals.clone();
            let Some(nal) = ahead.next() else { break };
            if slices && (!nal.is_slice() || nal.starts_picture()) {
                break;
            }
            start.get_or_insert(nal.start);
            end = nal.end;
            slices |= nal.is_slice();
            self.nals = ahead;
        }
        // Units after the last slice belong to no picture.
        if slices { start.map(|start| &self.data[start..end]) } else { None }
    }
}
