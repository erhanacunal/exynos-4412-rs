//! NMEA 0183 sentences, as GPS receivers send them: a line assembler, the checksum, and a fix
//! kept up to date from `RMC`, `GGA` and `GSV` sentences of any talker (`GP`, `GN`, `GL`...).
//!
//! A sentence is `$`, comma-separated fields, `*`, two hex digits of the XOR of every byte
//! between `$` and `*`, then `\r\n`; at most 82 bytes with both ends. Numbers are decimal text
//! with any number of decimals: they are kept in fixed point here, no floating point.

/// Bytes a sentence may have here, `$` to the checksum, before it is dropped: the standard's 82
/// less `\r\n`, with room for receivers that go over it.
pub const MAX_SENTENCE: usize = 120;

/// Collects bytes into sentences.
///
/// Anything outside a sentence is skipped: the bytes before the first `$`, and binary messages
/// (u-blox UBX) between sentences. A sentence with a byte that is not printable ASCII, or longer
/// than [`MAX_SENTENCE`], is dropped.
#[derive(Clone, Debug)]
pub struct LineBuffer {
    bytes: [u8; MAX_SENTENCE],
    len: usize,
    /// Inside a sentence that will be dropped at its end.
    dropping: bool,
    dropped: u32,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LineBuffer {
    /// An empty buffer.
    pub const fn new() -> Self {
        Self { bytes: [0; MAX_SENTENCE], len: 0, dropping: false, dropped: 0 }
    }

    /// Takes one byte. Returns the sentence it ends, `$` to the checksum, without `\r\n`.
    pub fn push(&mut self, byte: u8) -> Option<&[u8]> {
        match byte {
            b'$' => {
                if self.len > 0 || self.dropping {
                    self.dropped += 1;
                }
                self.bytes[0] = b'$';
                self.len = 1;
                self.dropping = false;
                None
            }
            b'\r' | b'\n' => {
                let len = core::mem::take(&mut self.len);
                if core::mem::take(&mut self.dropping) {
                    self.dropped += 1;
                    return None;
                }
                (len > 0).then(|| &self.bytes[..len])
            }
            _ if self.len == 0 => None,
            0x20..=0x7E if self.len < MAX_SENTENCE => {
                self.bytes[self.len] = byte;
                self.len += 1;
                None
            }
            _ => {
                self.len = 0;
                self.dropping = true;
                None
            }
        }
    }

    /// Sentences dropped so far: too long, broken by a binary byte, or cut by the next `$`.
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }
}

/// Why a sentence was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No `$` at the start or no `*` with two hex digits at the end.
    Framing,
    /// The checksum does not match.
    Checksum {
        /// The two hex digits after `*`.
        expected: u8,
        /// The XOR of the bytes.
        computed: u8,
    },
    /// A field of a known sentence could not be read.
    Field {
        /// The field's place, 0 being the address (`GPRMC`).
        index: u8,
    },
}

/// Checks the frame and checksum of `sentence`, `$` to the checksum, and returns what is
/// between `$` and `*`.
pub fn verify(sentence: &[u8]) -> Result<&[u8], Error> {
    let [b'$', rest @ ..] = sentence else {
        return Err(Error::Framing);
    };
    let [body @ .., b'*', high, low] = rest else {
        return Err(Error::Framing);
    };
    let expected = (hex_digit(*high).ok_or(Error::Framing)? << 4) | hex_digit(*low).ok_or(Error::Framing)?;
    let computed = body.iter().fold(0, |sum, byte| sum ^ byte);
    if computed == expected { Ok(body) } else { Err(Error::Checksum { expected, computed }) }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Time of day, UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Time {
    /// 0 to 23.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
    /// 0 to 60, a leap second.
    pub second: u8,
    /// The decimals the receiver sends, as milliseconds.
    pub millisecond: u16,
}

/// Calendar date, UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Date {
    /// From the two digits sent, 2000 to 2099.
    pub year: u16,
    /// 1 to 12.
    pub month: u8,
    /// 1 to 31.
    pub day: u8,
}

/// The kind of sentence [`Fix::update`] took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Recommended minimum: time, date, validity, position, speed, course.
    Rmc,
    /// Fix data: time, position, quality, satellites used, HDOP, altitude.
    Gga,
    /// Satellites in view, one of a group of sentences.
    Gsv,
    /// Valid, but not one this module reads.
    Other,
}

/// What the receiver last said. A field the last sentence left empty becomes `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fix {
    /// `RMC` and `GGA`.
    pub time: Option<Time>,
    /// `RMC`.
    pub date: Option<Date>,
    /// `RMC` status `A`: the position is valid.
    pub valid: bool,
    /// Degrees times 10^7, north positive. `RMC` and `GGA`.
    pub latitude: Option<i32>,
    /// Degrees times 10^7, east positive. `RMC` and `GGA`.
    pub longitude: Option<i32>,
    /// Ground speed, thousandths of a knot. `RMC`.
    pub speed_milliknots: Option<u32>,
    /// Course over ground, hundredths of a degree from true north. `RMC`.
    pub course_centidegrees: Option<u16>,
    /// `GGA` quality: 0 no fix, 1 GPS, 2 differential, 6 estimated...
    pub quality: u8,
    /// Satellites used. `GGA`.
    pub satellites_used: u8,
    /// Horizontal dilution of precision, times 100. `GGA`.
    pub hdop_centi: Option<u16>,
    /// Above mean sea level, decimetres. `GGA`.
    pub altitude_dm: Option<i32>,
    /// Satellites in view. `GSV`.
    pub satellites_in_view: u8,
}

impl Fix {
    /// Takes one sentence, `$` to the checksum: checks it, and updates the fields it carries.
    /// A sentence with a bad field changes nothing.
    pub fn update(&mut self, sentence: &[u8]) -> Result<Kind, Error> {
        let body = verify(sentence)?;
        let mut fields = [&[][..]; 20];
        let mut count = 0;
        for field in body.split(|byte| *byte == b',') {
            if count == fields.len() {
                break;
            }
            fields[count] = field;
            count += 1;
        }
        let fields = &fields[..count];
        let address = fields[0];
        let kind = match address.get(address.len().saturating_sub(3)..) {
            Some(b"RMC") if address.len() == 5 => Kind::Rmc,
            Some(b"GGA") if address.len() == 5 => Kind::Gga,
            Some(b"GSV") if address.len() == 5 => Kind::Gsv,
            _ => return Ok(Kind::Other),
        };
        let field = |index: usize| fields.get(index).copied().unwrap_or(&[]);
        let bad = |index: usize| Error::Field { index: index as u8 };
        let mut next = *self;
        match kind {
            Kind::Rmc => {
                next.time = optional(field(1), time).ok_or(bad(1))?;
                next.valid = field(2) == b"A";
                next.latitude = optional2(field(3), field(4), |v, h| coordinate(v, h, b'N', b'S')).ok_or(bad(3))?;
                next.longitude = optional2(field(5), field(6), |v, h| coordinate(v, h, b'E', b'W')).ok_or(bad(5))?;
                next.speed_milliknots =
                    optional(field(7), |v| u32::try_from(decimal(v, 3)?).ok()).ok_or(bad(7))?;
                next.course_centidegrees =
                    optional(field(8), |v| u16::try_from(decimal(v, 2)?).ok()).ok_or(bad(8))?;
                next.date = optional(field(9), date).ok_or(bad(9))?;
            }
            Kind::Gga => {
                next.time = optional(field(1), time).ok_or(bad(1))?;
                next.latitude = optional2(field(2), field(3), |v, h| coordinate(v, h, b'N', b'S')).ok_or(bad(2))?;
                next.longitude = optional2(field(4), field(5), |v, h| coordinate(v, h, b'E', b'W')).ok_or(bad(4))?;
                next.quality = small(field(6)).ok_or(bad(6))?;
                next.satellites_used = small(field(7)).ok_or(bad(7))?;
                next.hdop_centi = optional(field(8), |v| u16::try_from(decimal(v, 2)?).ok()).ok_or(bad(8))?;
                next.altitude_dm = optional(field(9), |v| i32::try_from(decimal(v, 1)?).ok()).ok_or(bad(9))?;
            }
            Kind::Gsv => next.satellites_in_view = small(field(3)).ok_or(bad(3))?,
            Kind::Other => {}
        }
        *self = next;
        Ok(kind)
    }
}

/// `None` (a bad field) or `Some(None)` (an empty one) or `Some(Some(value))`.
fn optional<T>(field: &[u8], parse: impl FnOnce(&[u8]) -> Option<T>) -> Option<Option<T>> {
    if field.is_empty() { Some(None) } else { parse(field).map(Some) }
}

fn optional2<T>(field: &[u8], second: &[u8], parse: impl FnOnce(&[u8], &[u8]) -> Option<T>) -> Option<Option<T>> {
    if field.is_empty() { Some(None) } else { parse(field, second).map(Some) }
}

/// A number 0 to 255; empty is 0.
fn small(field: &[u8]) -> Option<u8> {
    if field.is_empty() { Some(0) } else { u8::try_from(decimal(field, 0)?).ok() }
}

/// A decimal number times 10^`digits`, its further decimals cut off. An optional sign.
fn decimal(field: &[u8], digits: u32) -> Option<i64> {
    let (negative, field) = match field {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, field),
    };
    let mut parts = field.splitn(2, |byte| *byte == b'.');
    let whole = parts.next()?;
    let fraction = parts.next().unwrap_or(&[]);
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for byte in whole {
        value = value.checked_mul(10)?.checked_add(i64::from(digit(*byte)?))?;
    }
    for index in 0..digits as usize {
        let next = match fraction.get(index) {
            Some(byte) => digit(*byte)?,
            None => 0,
        };
        value = value.checked_mul(10)?.checked_add(i64::from(next))?;
    }
    if fraction.iter().skip(digits as usize).any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    Some(if negative { -value } else { value })
}

fn digit(byte: u8) -> Option<u8> {
    byte.is_ascii_digit().then(|| byte - b'0')
}

/// `hhmmss` with optional decimals.
fn time(field: &[u8]) -> Option<Time> {
    let value = decimal(field, 3)?;
    let (whole, millisecond) = (value / 1000, (value % 1000) as u16);
    let (hour, minute, second) = (whole / 10_000, whole / 100 % 100, whole % 100);
    (hour < 24 && minute < 60 && second < 61 && field.len() >= 6).then_some(Time {
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        millisecond,
    })
}

/// `ddmmyy`, years from 2000.
fn date(field: &[u8]) -> Option<Date> {
    if field.len() != 6 {
        return None;
    }
    let value = decimal(field, 0)?;
    let (day, month, year) = (value / 10_000, value / 100 % 100, value % 100);
    ((1..=31).contains(&day) && (1..=12).contains(&month)).then_some(Date {
        year: 2000 + year as u16,
        month: month as u8,
        day: day as u8,
    })
}

/// `dddmm.mmmm` and its hemisphere, in degrees times 10^7.
fn coordinate(field: &[u8], hemisphere: &[u8], positive: u8, negative: u8) -> Option<i32> {
    // Minutes to 10^-7 of a degree need 10^-7 * 60 of a minute: nine decimals keep the rounding
    // below the last digit.
    let value = decimal(field, 9)?;
    let scale = 1_000_000_000;
    let degrees = value / (100 * scale);
    let minutes = value % (100 * scale);
    if minutes >= 60 * scale {
        return None;
    }
    let magnitude = degrees * 10_000_000 + minutes / 6_000;
    let magnitude = i32::try_from(magnitude).ok()?;
    match hemisphere {
        [byte] if *byte == positive => Some(magnitude),
        [byte] if *byte == negative => Some(-magnitude),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_checksum(body: &str) -> [u8; 128] {
        let sum = body.bytes().fold(0u8, |sum, byte| sum ^ byte);
        let mut out = [0u8; 128];
        let text = body.as_bytes();
        out[0] = b'$';
        out[1..1 + text.len()].copy_from_slice(text);
        let hex = b"0123456789ABCDEF";
        out[1 + text.len()] = b'*';
        out[2 + text.len()] = hex[usize::from(sum >> 4)];
        out[3 + text.len()] = hex[usize::from(sum & 15)];
        out
    }

    fn sentence(buffer: &[u8; 128]) -> &[u8] {
        let len = buffer.iter().position(|byte| *byte == 0).unwrap();
        &buffer[..len]
    }

    #[test]
    fn rmc_and_gga() {
        let mut fix = Fix::default();
        let rmc = with_checksum("GPRMC,123519.50,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W");
        assert_eq!(fix.update(sentence(&rmc)), Ok(Kind::Rmc));
        assert_eq!(fix.time, Some(Time { hour: 12, minute: 35, second: 19, millisecond: 500 }));
        assert_eq!(fix.date, Some(Date { year: 2094, month: 3, day: 23 }));
        assert!(fix.valid);
        // 48 degrees 7.038 minutes = 48.1173 degrees.
        assert_eq!(fix.latitude, Some(481_173_000));
        assert_eq!(fix.longitude, Some(115_166_666));
        assert_eq!(fix.speed_milliknots, Some(22_400));
        assert_eq!(fix.course_centidegrees, Some(8_440));

        let gga = with_checksum("GPGGA,123520,4807.038,S,01131.000,W,1,08,0.9,545.4,M,46.9,M,,");
        assert_eq!(fix.update(sentence(&gga)), Ok(Kind::Gga));
        assert_eq!(fix.latitude, Some(-481_173_000));
        assert_eq!(fix.longitude, Some(-115_166_666));
        assert_eq!((fix.quality, fix.satellites_used, fix.hdop_centi, fix.altitude_dm), (1, 8, Some(90), Some(5454)));
    }

    #[test]
    fn no_fix_yet() {
        let mut fix = Fix::default();
        let rmc = with_checksum("GPRMC,,V,,,,,,,,,,N");
        assert_eq!(fix.update(sentence(&rmc)), Ok(Kind::Rmc));
        assert_eq!(fix, Fix::default());
        let gsv = with_checksum("GPGSV,1,1,03,05,,,25,12,,,30,20,,,");
        assert_eq!(fix.update(sentence(&gsv)), Ok(Kind::Gsv));
        assert_eq!(fix.satellites_in_view, 3);
    }

    #[test]
    fn checksum_and_fields() {
        let mut fix = Fix::default();
        assert!(matches!(verify(b"$GPTXT,01*00"), Err(Error::Checksum { expected: 0, .. })));
        assert_eq!(verify(b"$GPTXT,01*62"), Ok(&b"GPTXT,01"[..]));
        assert_eq!(verify(b"GPRMC*00"), Err(Error::Framing));
        assert_eq!(verify(b"$GPRMC*0"), Err(Error::Framing));
        let bad = with_checksum("GPGGA,1x3520,,,,,0,00,,,M,,M,,");
        assert_eq!(fix.update(sentence(&bad)), Err(Error::Field { index: 1 }));
        let other = with_checksum("GPTXT,01,01,02,u-blox ag");
        assert_eq!(fix.update(sentence(&other)), Ok(Kind::Other));
    }

    #[test]
    fn line_buffer() {
        let mut lines = LineBuffer::new();
        let mut got = 0;
        for byte in b"junk\xB5\x62$GPA*00\r\n$GP\xB5B*00\r\n$GPC*00\r\n" {
            if let Some(line) = lines.push(*byte) {
                assert!(line == b"$GPA*00" || line == b"$GPC*00");
                got += 1;
            }
        }
        assert_eq!((got, lines.dropped()), (2, 1));
    }
}
