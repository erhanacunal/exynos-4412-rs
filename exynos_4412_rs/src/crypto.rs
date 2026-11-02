//! The cryptography WPA2-Personal asks of a client: SHA-1, HMAC-SHA1, PBKDF2 for the key from the
//! passphrase, the PRF of IEEE 802.11, and AES-128 key unwrapping.
//!
//! Written from the standards named at each item, and checked by the tests at the end against
//! their published vectors and values from Python's `hashlib` and `hmac`. It only transforms bytes.
//!
//! This code does not try to resist timing or cache attacks, beyond [`equal`]: its AES uses tables
//! indexed by secret bytes. An attacker would need to run code on this CPU or time it closely.

const SHA1_BLOCK: usize = 64;
/// Bytes of a SHA-1 digest.
pub const SHA1_LENGTH: usize = 20;
/// Iterations for a WPA key: IEEE Std 802.11-2016, J.4.1.
const PSK_ITERATIONS: u32 = 4096;
/// Bytes of an AES block, and of the key of AES-128: FIPS 197.
const AES_BLOCK: usize = 16;
/// Rounds of AES-128: FIPS 197, figure 4.
const AES_ROUNDS: usize = 10;
/// Key wrap's integrity value: RFC 3394, 2.2.3.1.
const WRAP_IV: [u8; 8] = [0xA6; 8];

/// Compares without stopping at the first difference, so the time taken does not tell where it is.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |all, (x, y)| all | (x ^ y)) == 0
}

/// SHA-1: FIPS 180-4, 6.1.
#[derive(Clone, Copy)]
pub struct Sha1 {
    state: [u32; 5],
    buffer: [u8; SHA1_BLOCK],
    used: usize,
    length: u64,
}

impl Sha1 {
    /// Starts a digest.
    pub const fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0],
            buffer: [0; SHA1_BLOCK],
            used: 0,
            length: 0,
        }
    }

    fn compress(state: &mut [u32; 5], block: &[u8; SHA1_BLOCK]) {
        let mut w = [0u32; 80];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for t in 16..80 {
            w[t] = (w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = *state;
        for (t, word) in w.iter().enumerate() {
            let (f, k) = match t {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (total, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *total = total.wrapping_add(value);
        }
    }

    /// Adds bytes to the digest.
    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.used != 0 {
            let taken = data.len().min(SHA1_BLOCK - self.used);
            self.buffer[self.used..self.used + taken].copy_from_slice(&data[..taken]);
            self.used += taken;
            data = &data[taken..];
            if self.used < SHA1_BLOCK {
                return;
            }
            let block = self.buffer;
            Self::compress(&mut self.state, &block);
            self.used = 0;
        }
        let (blocks, rest) = data.as_chunks::<SHA1_BLOCK>();
        for block in blocks {
            Self::compress(&mut self.state, block);
        }
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.used = rest.len();
    }

    /// Ends the digest and returns it.
    pub fn finish(mut self) -> [u8; SHA1_LENGTH] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.used != SHA1_BLOCK - 8 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut digest = [0; SHA1_LENGTH];
        for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

/// A key ready for HMAC-SHA1, RFC 2104. Keeping its two first blocks makes PBKDF2 far cheaper.
#[derive(Clone, Copy)]
pub struct Hmac {
    inner: Sha1,
    outer: Sha1,
}

impl Hmac {
    /// Prepares a key of any length.
    pub fn new(key: &[u8]) -> Self {
        let mut block = [0; SHA1_BLOCK];
        if key.len() > SHA1_BLOCK {
            let mut digest = Sha1::new();
            digest.update(key);
            block[..SHA1_LENGTH].copy_from_slice(&digest.finish());
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner = Sha1::new();
        let mut outer = Sha1::new();
        inner.update(&block.map(|byte| byte ^ 0x36));
        outer.update(&block.map(|byte| byte ^ 0x5C));
        Self { inner, outer }
    }

    /// The HMAC of the concatenation of `parts`.
    pub fn sign(&self, parts: &[&[u8]]) -> [u8; SHA1_LENGTH] {
        let mut inner = self.inner;
        for part in parts {
            inner.update(part);
        }
        let mut outer = self.outer;
        outer.update(&inner.finish());
        outer.finish()
    }
}

/// The 256-bit key of a WPA network: PBKDF2-HMAC-SHA1 of its passphrase with its name as the salt,
/// RFC 8018 section 5.2 with the parameters of IEEE Std 802.11-2016, J.4.1.
pub fn wpa_key(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let hmac = Hmac::new(passphrase);
    let mut key = [0; 32];
    for (index, output) in key.chunks_mut(SHA1_LENGTH).enumerate() {
        let number = (index as u32 + 1).to_be_bytes();
        let mut round = hmac.sign(&[ssid, &number]);
        let mut total = round;
        for _ in 1..PSK_ITERATIONS {
            round = hmac.sign(&[&round]);
            for (sum, byte) in total.iter_mut().zip(round) {
                *sum ^= byte;
            }
        }
        output.copy_from_slice(&total[..output.len()]);
    }
    key
}

/// The pseudo-random function of IEEE Std 802.11-2016, 12.7.1.2: fills `output`.
pub fn prf(key: &[u8], label: &[u8], data: &[u8], output: &mut [u8]) {
    let hmac = Hmac::new(key);
    for (index, chunk) in output.chunks_mut(SHA1_LENGTH).enumerate() {
        let digest = hmac.sign(&[label, &[0], data, &[index as u8]]);
        chunk.copy_from_slice(&digest[..chunk.len()]);
    }
}

/// Multiplies by x in AES's field: FIPS 197, 4.2.1.
const fn times_x(value: u8) -> u8 {
    (value << 1) ^ (((value >> 7) & 1) * 0x1B)
}

/// Builds the S-box of FIPS 197, 5.1.1, from its definition, so no table can hold a wrong entry:
/// `p` walks the field by 3, and `q` by its inverse.
const fn make_sbox() -> [u8; 256] {
    let mut sbox = [0; 256];
    let mut p: u8 = 1;
    let mut q: u8 = 1;
    loop {
        p = p ^ times_x(p);
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        sbox[p as usize] =
            q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4) ^ 0x63;
        if p == 1 {
            break;
        }
    }
    sbox[0] = 0x63;
    sbox
}

const fn invert_sbox(sbox: &[u8; 256]) -> [u8; 256] {
    let mut inverse = [0; 256];
    let mut i = 0;
    while i < 256 {
        inverse[sbox[i] as usize] = i as u8;
        i += 1;
    }
    inverse
}

static SBOX: [u8; 256] = make_sbox();
static INVERSE_SBOX: [u8; 256] = invert_sbox(&SBOX);

/// AES-128: FIPS 197.
pub struct Aes128 {
    round_keys: [[u8; AES_BLOCK]; AES_ROUNDS + 1],
}

impl Aes128 {
    /// Expands a key: FIPS 197, 5.2.
    pub fn new(key: &[u8; AES_BLOCK]) -> Self {
        let mut round_keys = [[0; AES_BLOCK]; AES_ROUNDS + 1];
        round_keys[0] = *key;
        let mut constant = 1;
        for round in 1..=AES_ROUNDS {
            let last = round_keys[round - 1];
            let mut next = [0; AES_BLOCK];
            let mut word = [
                SBOX[usize::from(last[13])] ^ constant,
                SBOX[usize::from(last[14])],
                SBOX[usize::from(last[15])],
                SBOX[usize::from(last[12])],
            ];
            for column in 0..4 {
                for row in 0..4 {
                    word[row] ^= last[column * 4 + row];
                    next[column * 4 + row] = word[row];
                }
            }
            round_keys[round] = next;
            constant = times_x(constant);
        }
        Self { round_keys }
    }

    /// Decrypts a block in place: FIPS 197, 5.3.
    pub fn decrypt(&self, block: &mut [u8; AES_BLOCK]) {
        add_key(block, &self.round_keys[AES_ROUNDS]);
        for round in (0..AES_ROUNDS).rev() {
            let s = *block;
            // Inverse ShiftRows then InvSubBytes: row r moves r columns to the right.
            for column in 0..4 {
                for row in 0..4 {
                    block[((column + row) % 4) * 4 + row] = INVERSE_SBOX[usize::from(s[column * 4 + row])];
                }
            }
            add_key(block, &self.round_keys[round]);
            if round != 0 {
                inverse_mix_columns(block);
            }
        }
    }

    /// Encrypts a block in place, for the tests' access point: FIPS 197, 5.1.
    #[cfg(test)]
    pub fn encrypt(&self, block: &mut [u8; AES_BLOCK]) {
        add_key(block, &self.round_keys[0]);
        for round in 1..=AES_ROUNDS {
            let s = *block;
            for column in 0..4 {
                for row in 0..4 {
                    block[column * 4 + row] = SBOX[usize::from(s[((column + row) % 4) * 4 + row])];
                }
            }
            if round != AES_ROUNDS {
                for column in block.as_chunks_mut::<4>().0 {
                    let c = [column[0], column[1], column[2], column[3]];
                    for row in 0..4 {
                        column[row] = times_x(c[row])
                            ^ times_x(c[(row + 1) % 4])
                            ^ c[(row + 1) % 4]
                            ^ c[(row + 2) % 4]
                            ^ c[(row + 3) % 4];
                    }
                }
            }
            add_key(block, &self.round_keys[round]);
        }
    }
}

fn add_key(block: &mut [u8; AES_BLOCK], key: &[u8; AES_BLOCK]) {
    for (byte, key) in block.iter_mut().zip(key) {
        *byte ^= key;
    }
}

/// FIPS 197, 5.3.3: each column times {0e}, {0b}, {0d}, {09}.
fn inverse_mix_columns(block: &mut [u8; AES_BLOCK]) {
    for column in block.as_chunks_mut::<4>().0 {
        let c = [column[0], column[1], column[2], column[3]];
        for row in 0..4 {
            let x1 = c[row];
            let x2 = times_x(x1);
            let x4 = times_x(x2);
            let x8 = times_x(x4);
            let y1 = c[(row + 1) % 4];
            let y2 = times_x(y1);
            let y4 = times_x(y2);
            let y8 = times_x(y4);
            let z1 = c[(row + 2) % 4];
            let z4 = times_x(times_x(z1));
            let z8 = times_x(z4);
            let w1 = c[(row + 3) % 4];
            let w8 = times_x(times_x(times_x(w1)));
            column[row] = (x8 ^ x4 ^ x2) ^ (y8 ^ y2 ^ y1) ^ (z8 ^ z4 ^ z1) ^ (w8 ^ w1);
        }
    }
}

/// Undoes AES key wrap, RFC 3394 2.2.2. `output` must hold eight bytes less than `wrapped`, which
/// must be at least 16 bytes in steps of 8. Returns false if the integrity check fails or the
/// lengths do not fit, and `output` then means nothing.
pub fn key_unwrap(key: &[u8; AES_BLOCK], wrapped: &[u8], output: &mut [u8]) -> bool {
    if wrapped.len() < 16 || !wrapped.len().is_multiple_of(8) || output.len() != wrapped.len() - 8 {
        return false;
    }
    let aes = Aes128::new(key);
    let mut a = [0; 8];
    a.copy_from_slice(&wrapped[..8]);
    output.copy_from_slice(&wrapped[8..]);
    let blocks = output.len() / 8;
    for step in (0..6u64).rev() {
        for index in (0..blocks).rev() {
            let t = step * blocks as u64 + index as u64 + 1;
            let mut block = [0; AES_BLOCK];
            for (byte, count) in a.iter_mut().zip(t.to_be_bytes()) {
                *byte ^= count;
            }
            block[..8].copy_from_slice(&a);
            let r = &mut output[index * 8..index * 8 + 8];
            block[8..].copy_from_slice(r);
            aes.decrypt(&mut block);
            a.copy_from_slice(&block[..8]);
            r.copy_from_slice(&block[8..]);
        }
    }
    equal(&a, &WRAP_IV)
}

/// Wraps a key, RFC 3394 2.2.1, for the tests' access point.
#[cfg(test)]
pub fn key_wrap(key: &[u8; AES_BLOCK], plain: &[u8], output: &mut [u8]) {
    let aes = Aes128::new(key);
    let mut a = WRAP_IV;
    let blocks = plain.len() / 8;
    output[8..8 + plain.len()].copy_from_slice(plain);
    for step in 0..6u64 {
        for index in 0..blocks {
            let t = step * blocks as u64 + index as u64 + 1;
            let mut block = [0; AES_BLOCK];
            block[..8].copy_from_slice(&a);
            let r = &mut output[8 + index * 8..16 + index * 8];
            block[8..].copy_from_slice(r);
            aes.encrypt(&mut block);
            a.copy_from_slice(&block[..8]);
            for (byte, count) in a.iter_mut().zip(t.to_be_bytes()) {
                *byte ^= count;
            }
            r.copy_from_slice(&block[8..]);
        }
    }
    output[..8].copy_from_slice(&a);
}

/// Checks this code against one vector for each algorithm. Returns true if all hold. It takes some
/// milliseconds, for 8192 rounds of HMAC.
pub fn self_test() -> bool {
    // IEEE Std 802.11-2016, J.4.2, test vector 1.
    const WPA_KEY: [u8; 32] = [
        0xF4, 0x2C, 0x6F, 0xC5, 0x2D, 0xF0, 0xEB, 0xEF, 0x9E, 0xBB, 0x4B, 0x90, 0xB3, 0x8A, 0x5F, 0x90,
        0x2E, 0x83, 0xFE, 0x1B, 0x13, 0x5A, 0x70, 0xE2, 0x3A, 0xED, 0x76, 0x2E, 0x97, 0x10, 0xA1, 0x2E,
    ];
    // RFC 3394, 4.1.
    const KEK: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    const WRAPPED: [u8; 24] = [
        0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47, 0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B, 0x82,
        0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5,
    ];
    const UNWRAPPED: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
    ];
    let mut plain = [0; 16];
    wpa_key(b"password", b"IEEE") == WPA_KEY
        && key_unwrap(&KEK, &WRAPPED, &mut plain)
        && plain == UNWRAPPED
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(data: &[u8]) -> [u8; SHA1_LENGTH] {
        let mut sha = Sha1::new();
        sha.update(data);
        sha.finish()
    }

    #[test]
    fn sha1_fips_vectors() {
        assert_eq!(
            digest(b"abc"),
            [
                0xA9, 0x99, 0x3E, 0x36, 0x47, 0x06, 0x81, 0x6A, 0xBA, 0x3E, 0x25, 0x71, 0x78, 0x50,
                0xC2, 0x6C, 0x9C, 0xD0, 0xD8, 0x9D,
            ],
        );
        assert_eq!(
            digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            [
                0x84, 0x98, 0x3E, 0x44, 0x1C, 0x3B, 0xD2, 0x6E, 0xBA, 0xAE, 0x4A, 0xA1, 0xF9, 0x51,
                0x29, 0xE5, 0xE5, 0x46, 0x70, 0xF1,
            ],
        );
    }

    #[test]
    fn sha1_in_pieces_matches_at_once() {
        let data = [0x5A; 300];
        let mut sha = Sha1::new();
        for piece in data.chunks(7) {
            sha.update(piece);
        }
        assert_eq!(sha.finish(), digest(&data));
    }

    #[test]
    fn hmac_rfc2202_case_1() {
        assert_eq!(
            Hmac::new(&[0x0B; 20]).sign(&[b"Hi There"]),
            [
                0xB6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xE2, 0x8B, 0xC0, 0xB6, 0xFB, 0x37,
                0x8C, 0x8E, 0xF1, 0x46, 0xBE, 0x00,
            ],
        );
    }

    #[test]
    fn prf_matches_python() {
        let mut output = [0; 48];
        prf(&[0x0B; 20], b"prefix", b"Hi There", &mut output);
        assert_eq!(output, [
            0xBC, 0xD4, 0xC6, 0x50, 0xB3, 0x0B, 0x96, 0x84, 0x95, 0x18, 0x29, 0xE0, 0xD7, 0x5F, 0x9D, 0x54, 0xB8, 0x62, 0x17, 0x5E, 0xD9, 0xF0, 0x06, 0x06, 0xE1, 0x7D, 0x8D, 0xA3, 0x54, 0x02, 0xFF, 0xEE, 0x75, 0xDF, 0x78, 0xC3, 0xD3, 0x1E, 0x0F, 0x88, 0x9F, 0x01, 0x21, 0x20, 0xC0, 0x86, 0x2B, 0xEB,
        ]);
    }

    #[test]
    fn self_test_holds() {
        assert!(self_test());
    }

    #[test]
    fn aes_fips197_appendix_c1() {
        let key = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let plain = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let cipher = [
            0x69, 0xC4, 0xE0, 0xD8, 0x6A, 0x7B, 0x04, 0x30, 0xD8, 0xCD, 0xB7, 0x80, 0x70, 0xB4, 0xC5, 0x5A,
        ];
        let aes = Aes128::new(&key);
        let mut block = plain;
        aes.encrypt(&mut block);
        assert_eq!(block, cipher);
        aes.decrypt(&mut block);
        assert_eq!(block, plain);
    }

    #[test]
    fn unwrap_rejects_a_changed_byte() {
        let key = [7; 16];
        let mut wrapped = [0; 40];
        key_wrap(&key, &[9; 32], &mut wrapped);
        let mut plain = [0; 32];
        assert!(key_unwrap(&key, &wrapped, &mut plain));
        assert_eq!(plain, [9; 32]);
        wrapped[20] ^= 1;
        assert!(!key_unwrap(&key, &wrapped, &mut plain));
    }
}
