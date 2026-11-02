//! NVRAM file of the BCM4334: from the text form to the form the chip's firmware reads.
//!
//! Only transforms bytes. The result is what `brcmfmac/firmware.c` in Linux 5.15.198 makes of a file
//! like this board's: see `THIRD_PARTY_NOTICES.md`. It holds `key=value` entries, each ended by a
//! zero, then a zero, padding to four bytes, and a word giving the length. That word goes last, so
//! the firmware finds it at the end of the chip's RAM.

/// `brcmfmac/firmware.c:23` `BRCMF_FW_DEFAULT_BOARDREV`, added if the file has no such entry.
const DEFAULT_BOARDREV: &[u8] = b"boardrev=0xff";
const BOARDREV: &[u8] = b"boardrev";
/// `brcmfmac/firmware.c` treats this key as a comment.
const IGNORED_KEY: &[u8] = b"RAW1";

/// Characters of a key or a value: printable ASCII, without `#`, which starts a comment.
const fn is_entry_char(byte: u8) -> bool {
    byte != b'#' && byte >= 0x20 && byte < 0x7F
}

/// Packs the text of an NVRAM file into `output`, and returns how many bytes that takes, or `None`
/// if they do not fit. `output` needs room for the text plus 32 bytes.
pub fn pack(text: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut length = 0;
    let mut boardrev = false;
    for line in text.split(|&byte| byte == b'\n') {
        let start = line.iter().position(|&byte| !matches!(byte, b' ' | b'\r' | b'\t' | 0));
        let Some(start) = start else { continue };
        let entry = &line[start..];
        let Some(equals) = entry.iter().position(|&byte| !is_entry_char(byte) || byte == b' ' || byte == b'=')
        else {
            continue;
        };
        if entry[equals] != b'=' || entry.starts_with(IGNORED_KEY) {
            continue;
        }
        boardrev |= entry.starts_with(BOARDREV);
        let end = entry[equals + 1..]
            .iter()
            .position(|&byte| !is_entry_char(byte))
            .map_or(entry.len(), |position| equals + 1 + position);
        length = push(output, length, &entry[..end])?;
    }
    if length == 0 {
        return None;
    }
    if !boardrev {
        length = push(output, length, DEFAULT_BOARDREV)?;
    }

    let padded = (length + 1).next_multiple_of(4);
    output.get_mut(length..padded)?.fill(0);
    let words = (padded / 4) as u32;
    let token = (!words << 16) | (words & 0xFFFF);
    output.get_mut(padded..padded + 4)?.copy_from_slice(&token.to_le_bytes());
    Some(padded + 4)
}

fn push(output: &mut [u8], at: usize, entry: &[u8]) -> Option<usize> {
    let end = at + entry.len();
    output.get_mut(at..end)?.copy_from_slice(entry);
    *output.get_mut(end)? = 0;
    Some(end + 1)
}
