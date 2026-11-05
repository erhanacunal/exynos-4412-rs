//! CRC-32 as zlib computes it (IEEE, reflected, `0xEDB88320`), from a table built at compile
//! time: fast enough for images that come over WiFi at a few MB/s.

const POLY: u32 = 0xEDB8_8320;

const TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// The running value before any byte.
pub const START: u32 = !0;

/// Takes `bytes` into the running value `crc`.
pub fn update(crc: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(crc, |crc, &byte| TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8))
}

/// The running value as the CRC of what it took.
pub const fn finish(crc: u32) -> u32 {
    !crc
}

