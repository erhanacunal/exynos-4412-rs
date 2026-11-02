//! The client's side of the WPA2-Personal handshake with CCMP: IEEE Std 802.11-2016, 12.7.6 and
//! 12.7.7. It only transforms bytes: it takes the EAPOL frames an access point sends and gives
//! the ones to send back, and the keys to hand to the chip, which does the encryption itself.
//!
//! Its layout of a key frame is that of `struct wpa_eapol_key`, and its bits are `WPA_KEY_INFO_*`,
//! in `drivers/staging/ks7010/eap_packet.h:40-68` of Linux 5.15.198, used as a map only.
//!
//! Only networks with AES for both their traffic and their broadcasts, and a shared passphrase,
//! are supported: not TKIP, WPA3, nor those that demand protected management frames.

use crate::crypto::{self, Hmac};

/// Bytes the longest frame [`Supplicant::handle`] can make needs.
pub const REPLY_MAX: usize = KEY_DATA + OWN_RSN.len();
/// Longest element describing a network's security this keeps.
pub const RSN_MAX: usize = 64;

/// What the client asks for in its association request, and its second message: one version,
/// AES for broadcasts and traffic, and the shared passphrase. The element is defined by IEEE Std
/// 802.11-2016, 9.4.2.25; `include/linux/ieee80211.h:2824,3501,3515`.
pub const OWN_RSN: [u8; 22] = [
    48, 20, 1, 0, 0x00, 0x0F, 0xAC, CIPHER_CCMP, 1, 0, 0x00, 0x0F, 0xAC, CIPHER_CCMP, 1, 0, 0x00, 0x0F, 0xAC,
    AKM_PSK, 0, 0,
];

/// `include/linux/ieee80211.h:3501`.
const CIPHER_CCMP: u8 = 4;
/// `include/linux/ieee80211.h:3515`.
const AKM_PSK: u8 = 2;
const SUITE_PREFIX: [u8; 3] = [0x00, 0x0F, 0xAC];
/// Protection of management frames is required: `brcmfmac/cfg80211.c:62`.
const RSN_CAPABILITY_MFP_REQUIRED: u16 = 1 << 6;
/// `include/linux/ieee80211.h:2824`.
const ELEMENT_RSN: u8 = 48;
/// `include/linux/ieee80211.h:2978`.
const ELEMENT_VENDOR: u8 = 221;
/// A vendor element holding a group key: IEEE Std 802.11-2016, table 12-6.
const KDE_GROUP_KEY: [u8; 4] = [0x00, 0x0F, 0xAC, 1];

/// `drivers/staging/ks7010/eap_packet.h:33` `IEEE802_1X_TYPE_EAPOL_KEY`.
const EAPOL_TYPE_KEY: u8 = 3;
/// Our 802.1X version: IEEE Std 802.1X-2004. Access points take this from any client.
const EAPOL_VERSION: u8 = 1;
/// `drivers/net/wireless/ath/wil6210/txrx.h:436`.
const DESCRIPTOR_RSN: u8 = 2;
/// Offsets in an EAPOL frame: 4 bytes of header, then `struct wpa_eapol_key`.
const DESCRIPTOR: usize = 4;
const KEY_INFO: usize = 5;
const REPLAY: usize = 9;
const NONCE: usize = 17;
const RSC: usize = 65;
const MIC: usize = 81;
const KEY_DATA_LENGTH: usize = 97;
const KEY_DATA: usize = 99;
const MIC_LENGTH: usize = 16;

/// `drivers/staging/ks7010/eap_packet.h:54-68`.
const INFO_VERSION_MASK: u16 = 7;
const INFO_VERSION_SHA1_AES: u16 = 2;
const INFO_PAIRWISE: u16 = 1 << 3;
const INFO_INSTALL: u16 = 1 << 6;
const INFO_ACK: u16 = 1 << 7;
const INFO_MIC: u16 = 1 << 8;
const INFO_SECURE: u16 = 1 << 9;
const INFO_ENCRYPTED: u16 = 1 << 12;

/// Bytes of decrypted key data this takes.
const KEY_DATA_MAX: usize = 256;
/// Bytes of a key for AES in CCMP.
const TK_LENGTH: usize = 16;

/// Whether a network's security element, with its header, asks for what this module does.
pub fn rsn_supported(element: &[u8]) -> bool {
    fn suites(body: &[u8]) -> Option<(&[u8], &[u8])> {
        let count = usize::from(u16::from_le_bytes([*body.first()?, *body.get(1)?]));
        let end = 2 + count.checked_mul(4)?;
        Some((body.get(2..end)?, body.get(end..)?))
    }
    fn has(list: &[u8], value: u8) -> bool {
        list.as_chunks::<4>().0.iter().any(|suite| suite[..3] == SUITE_PREFIX && suite[3] == value)
    }
    let [ELEMENT_RSN, length, body @ ..] = element else {
        return false;
    };
    if body.len() != usize::from(*length) || body.len() < 6 || body[..2] != [1, 0] {
        return false;
    }
    if body[2..5] != SUITE_PREFIX || body[5] != CIPHER_CCMP {
        return false;
    }
    let Some((pairwise, rest)) = suites(&body[6..]) else {
        return false;
    };
    let Some((akms, rest)) = suites(rest) else {
        return false;
    };
    let capabilities = match rest {
        [low, high, ..] => u16::from_le_bytes([*low, *high]),
        _ => 0,
    };
    has(pairwise, CIPHER_CCMP) && has(akms, AKM_PSK) && capabilities & RSN_CAPABILITY_MFP_REQUIRED == 0
}

/// Why a frame was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The frame is cut short, or is not a key frame of this kind.
    NotKeyFrame,
    /// The access point uses another key descriptor version: its bits.
    Version(u16),
    /// Its bits are of no message of this handshake.
    Unexpected(u16),
    /// Its replay counter is not newer than the last one taken.
    Replayed,
    /// Its check value is wrong: forged, or sent by someone with another key.
    BadMic,
    /// A message 3 for other nonces than this client answered.
    WrongNonce,
    /// Its key data does not decrypt or does not parse.
    BadKeyData,
    /// Message 3 describes other security than the network advertises, the sign of a downgrade.
    RsnMismatch,
    /// Its key data has no group key.
    NoGroupKey,
    /// The group key has a length other than AES's: for instance TKIP.
    GroupKeyLength(usize),
}

impl Error {
    /// Whether the handshake cannot end well, as opposed to one frame being worth ignoring.
    pub const fn is_fatal(self) -> bool {
        matches!(self, Self::Version(_) | Self::RsnMismatch | Self::GroupKeyLength(_))
    }
}

/// A broadcast key to hand to the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupKey {
    /// Its number, from 1 to 3.
    pub index: u8,
    /// The key.
    pub key: [u8; TK_LENGTH],
    /// The sequence counter it continues from.
    pub rsc: [u8; 6],
}

/// Which message arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// Message 1 of the 4-way handshake.
    First,
    /// Message 3 of the 4-way handshake.
    Third,
    /// Message 1 of the group key handshake.
    Group,
}

/// What to do after a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// What arrived.
    pub message: Message,
    /// Bytes of the EAPOL frame to send, always unencrypted and always before installing the keys
    /// below: message 4 encrypted with the new key would not reach the access point.
    pub reply_length: usize,
    /// The key for traffic with the access point to install, if new.
    pub pairwise: Option<[u8; TK_LENGTH]>,
    /// The broadcast key to install, if new.
    pub group: Option<GroupKey>,
}

/// The keys from a handshake: IEEE Std 802.11-2016, 12.7.1.3, table 12-8.
#[derive(Clone, Copy)]
struct Ptk {
    kck: [u8; 16],
    kek: [u8; 16],
    tk: [u8; TK_LENGTH],
}

/// The state of a connection's handshakes.
pub struct Supplicant {
    pmk: [u8; 32],
    own: [u8; 6],
    access_point: [u8; 6],
    network_rsn: [u8; RSN_MAX],
    network_rsn_length: usize,
    snonce: [u8; 32],
    anonce: [u8; 32],
    last_replay: Option<[u8; 8]>,
    pending: Option<Ptk>,
    current: Option<Ptk>,
    installed_tk: Option<[u8; TK_LENGTH]>,
    installed_group: Option<(u8, [u8; TK_LENGTH])>,
}

impl Supplicant {
    /// Starts for one access point: the key from [`crypto::wpa_key`], the addresses, the security
    /// element it advertises, with its header, and a nonce no earlier connection used. Returns
    /// `None` if the network asks for what this does not support.
    pub fn new(
        pmk: [u8; 32],
        own: [u8; 6],
        access_point: [u8; 6],
        network_rsn: &[u8],
        snonce: [u8; 32],
    ) -> Option<Self> {
        if !rsn_supported(network_rsn) || network_rsn.len() > RSN_MAX {
            return None;
        }
        let mut copy = [0; RSN_MAX];
        copy[..network_rsn.len()].copy_from_slice(network_rsn);
        Some(Self {
            pmk,
            own,
            access_point,
            network_rsn: copy,
            network_rsn_length: network_rsn.len(),
            snonce,
            anonce: [0; 32],
            last_replay: None,
            pending: None,
            current: None,
            installed_tk: None,
            installed_group: None,
        })
    }

    /// This client's address.
    pub const fn own(&self) -> [u8; 6] {
        self.own
    }

    /// The access point's address.
    pub const fn access_point(&self) -> [u8; 6] {
        self.access_point
    }

    /// Whether traffic and broadcast keys are both in place.
    pub const fn is_complete(&self) -> bool {
        self.installed_tk.is_some() && self.installed_group.is_some()
    }

    /// Takes an EAPOL frame from the access point. `reply` needs [`REPLY_MAX`] bytes.
    pub fn handle(&mut self, frame: &[u8], reply: &mut [u8]) -> Result<Step, Error> {
        let frame = key_frame(frame)?;
        let info = u16::from_be_bytes([frame[KEY_INFO], frame[KEY_INFO + 1]]);
        if info & INFO_VERSION_MASK != INFO_VERSION_SHA1_AES {
            return Err(Error::Version(info & INFO_VERSION_MASK));
        }
        let mut replay = [0; 8];
        replay.copy_from_slice(&frame[REPLAY..REPLAY + 8]);
        if self.last_replay.is_some_and(|last| replay <= last) {
            return Err(Error::Replayed);
        }

        let pairwise = info & INFO_PAIRWISE != 0;
        let mic = info & INFO_MIC != 0;
        let encrypted = info & INFO_ENCRYPTED != 0;
        if info & INFO_ACK == 0 {
            return Err(Error::Unexpected(info));
        }
        match (pairwise, mic) {
            (true, false) => self.first(frame, replay, reply),
            (true, true) if info & INFO_INSTALL != 0 && encrypted => self.third(frame, replay, reply),
            (false, true) if info & INFO_SECURE != 0 && encrypted => self.group(frame, replay, reply),
            _ => Err(Error::Unexpected(info)),
        }
    }

    /// Message 1 carries no check value, so it is believed only as far as answering it. Only
    /// message 3, which proves the sender holds the key, can change what is in use.
    fn first(&mut self, frame: &[u8], replay: [u8; 8], reply: &mut [u8]) -> Result<Step, Error> {
        self.anonce.copy_from_slice(&frame[NONCE..NONCE + 32]);
        let ptk = self.derive();
        self.pending = Some(ptk);
        let info = INFO_VERSION_SHA1_AES | INFO_PAIRWISE | INFO_MIC;
        let reply_length = build(reply, info, replay, &self.snonce, &OWN_RSN, &ptk.kck);
        Ok(Step {
            message: Message::First,
            reply_length,
            pairwise: None,
            group: None,
        })
    }

    fn third(&mut self, frame: &[u8], replay: [u8; 8], reply: &mut [u8]) -> Result<Step, Error> {
        let ptk = self.pending.ok_or(Error::Unexpected(0))?;
        check_mic(frame, &ptk.kck)?;
        if frame[NONCE..NONCE + 32] != self.anonce {
            return Err(Error::WrongNonce);
        }
        self.last_replay = Some(replay);

        let mut plain = [0; KEY_DATA_MAX];
        let data = decrypt_key_data(frame, &ptk.kek, &mut plain)?;
        let (rsn, group) = parse_key_data(data)?;
        if rsn != Some(&self.network_rsn[..self.network_rsn_length]) {
            return Err(Error::RsnMismatch);
        }
        let group = group_key(group, frame)?;
        self.current = Some(ptk);

        let info = INFO_VERSION_SHA1_AES | INFO_PAIRWISE | INFO_MIC | INFO_SECURE;
        let reply_length = build(reply, info, replay, &[0; 32], &[], &ptk.kck);
        // A repeated message 3 gets its answer, but the keys are not installed again: that would
        // reset their counters, the attack known as KRACK.
        let pairwise = (self.installed_tk != Some(ptk.tk)).then_some(ptk.tk);
        self.installed_tk = Some(ptk.tk);
        Ok(Step {
            message: Message::Third,
            reply_length,
            pairwise,
            group: self.new_group(group),
        })
    }

    fn group(&mut self, frame: &[u8], replay: [u8; 8], reply: &mut [u8]) -> Result<Step, Error> {
        let ptk = self.current.ok_or(Error::Unexpected(0))?;
        check_mic(frame, &ptk.kck)?;
        self.last_replay = Some(replay);
        let mut plain = [0; KEY_DATA_MAX];
        let data = decrypt_key_data(frame, &ptk.kek, &mut plain)?;
        let (_, group) = parse_key_data(data)?;
        let group = group_key(group, frame)?;
        let info = INFO_VERSION_SHA1_AES | INFO_MIC | INFO_SECURE;
        let reply_length = build(reply, info, replay, &[0; 32], &[], &ptk.kck);
        Ok(Step {
            message: Message::Group,
            reply_length,
            pairwise: None,
            group: self.new_group(group),
        })
    }

    fn new_group(&mut self, group: GroupKey) -> Option<GroupKey> {
        let identity = (group.index, group.key);
        if self.installed_group == Some(identity) {
            return None;
        }
        self.installed_group = Some(identity);
        Some(group)
    }

    /// IEEE Std 802.11-2016, 12.7.1.3: the lower of each pair goes first.
    fn derive(&self) -> Ptk {
        let mut data = [0; 76];
        let (low, high) = ordered(&self.own, &self.access_point);
        data[..6].copy_from_slice(low);
        data[6..12].copy_from_slice(high);
        let (low, high) = ordered(&self.snonce, &self.anonce);
        data[12..44].copy_from_slice(low);
        data[44..].copy_from_slice(high);
        let mut keys = [0; 48];
        crypto::prf(&self.pmk, b"Pairwise key expansion", &data, &mut keys);
        let mut ptk = Ptk {
            kck: [0; 16],
            kek: [0; 16],
            tk: [0; TK_LENGTH],
        };
        ptk.kck.copy_from_slice(&keys[..16]);
        ptk.kek.copy_from_slice(&keys[16..32]);
        ptk.tk.copy_from_slice(&keys[32..]);
        ptk
    }
}

fn ordered<'a>(a: &'a [u8], b: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    if a < b { (a, b) } else { (b, a) }
}

/// Cuts padding off a frame, and checks that it is a key frame whose key data fits in it.
fn key_frame(frame: &[u8]) -> Result<&[u8], Error> {
    if frame.len() < KEY_DATA || frame[1] != EAPOL_TYPE_KEY || frame[DESCRIPTOR] != DESCRIPTOR_RSN {
        return Err(Error::NotKeyFrame);
    }
    let body = usize::from(u16::from_be_bytes([frame[2], frame[3]]));
    let frame = frame.get(..4 + body).ok_or(Error::NotKeyFrame)?;
    if frame.len() < KEY_DATA {
        return Err(Error::NotKeyFrame);
    }
    let data = usize::from(u16::from_be_bytes([frame[KEY_DATA_LENGTH], frame[KEY_DATA_LENGTH + 1]]));
    if frame.len() != KEY_DATA + data {
        return Err(Error::NotKeyFrame);
    }
    Ok(frame)
}

/// The check value covers the whole frame with its own field zero, and is the first 16 bytes of
/// the HMAC: IEEE Std 802.11-2016, 12.7.2.
fn check_mic(frame: &[u8], kck: &[u8; 16]) -> Result<(), Error> {
    let hmac = Hmac::new(kck);
    let digest = hmac.sign(&[&frame[..MIC], &[0; MIC_LENGTH], &frame[MIC + MIC_LENGTH..]]);
    if crypto::equal(&digest[..MIC_LENGTH], &frame[MIC..MIC + MIC_LENGTH]) {
        Ok(())
    } else {
        Err(Error::BadMic)
    }
}

fn decrypt_key_data<'a>(frame: &[u8], kek: &[u8; 16], plain: &'a mut [u8]) -> Result<&'a [u8], Error> {
    let wrapped = &frame[KEY_DATA..];
    let length = wrapped.len().checked_sub(8).ok_or(Error::BadKeyData)?;
    let output = plain.get_mut(..length).ok_or(Error::BadKeyData)?;
    if crypto::key_unwrap(kek, wrapped, output) {
        Ok(output)
    } else {
        Err(Error::BadKeyData)
    }
}

/// The security element and the group key entry, of those found.
type KeyData<'a> = (Option<&'a [u8]>, Option<&'a [u8]>);

/// Finds the security element and the group key entry among the elements of key data.
fn parse_key_data(data: &[u8]) -> Result<KeyData<'_>, Error> {
    let mut rsn = None;
    let mut group = None;
    let mut rest = data;
    while let [id, length, tail @ ..] = rest {
        let Some(body) = tail.get(..usize::from(*length)) else {
            // Key wrap pads with one 0xDD then zeros, which says nothing of a length.
            if *id == ELEMENT_VENDOR {
                break;
            }
            return Err(Error::BadKeyData);
        };
        if *id == ELEMENT_RSN && rsn.is_none() {
            rsn = Some(&rest[..2 + body.len()]);
        } else if *id == ELEMENT_VENDOR && body.starts_with(&KDE_GROUP_KEY) && group.is_none() {
            group = Some(&body[KDE_GROUP_KEY.len()..]);
        }
        rest = &tail[usize::from(*length)..];
    }
    Ok((rsn, group))
}

/// The entry holds a key number in its first byte, a reserved byte, then the key.
fn group_key(entry: Option<&[u8]>, frame: &[u8]) -> Result<GroupKey, Error> {
    let [flags, _, bytes @ ..] = entry.ok_or(Error::NoGroupKey)? else {
        return Err(Error::NoGroupKey);
    };
    let key = <[u8; TK_LENGTH]>::try_from(bytes).map_err(|_| Error::GroupKeyLength(bytes.len()))?;
    let mut rsc = [0; 6];
    rsc.copy_from_slice(&frame[RSC..RSC + 6]);
    Ok(GroupKey {
        index: flags & 3,
        key,
        rsc,
    })
}

fn build(output: &mut [u8], info: u16, replay: [u8; 8], nonce: &[u8; 32], data: &[u8], kck: &[u8; 16]) -> usize {
    let length = KEY_DATA + data.len();
    let frame = &mut output[..length];
    frame.fill(0);
    frame[0] = EAPOL_VERSION;
    frame[1] = EAPOL_TYPE_KEY;
    frame[2..4].copy_from_slice(&((length - 4) as u16).to_be_bytes());
    frame[DESCRIPTOR] = DESCRIPTOR_RSN;
    frame[KEY_INFO..KEY_INFO + 2].copy_from_slice(&info.to_be_bytes());
    frame[REPLAY..REPLAY + 8].copy_from_slice(&replay);
    frame[NONCE..NONCE + 32].copy_from_slice(nonce);
    frame[KEY_DATA_LENGTH..KEY_DATA].copy_from_slice(&(data.len() as u16).to_be_bytes());
    frame[KEY_DATA..].copy_from_slice(data);
    let digest = Hmac::new(kck).sign(&[frame]);
    frame[MIC..MIC + MIC_LENGTH].copy_from_slice(&digest[..MIC_LENGTH]);
    length
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const AP: [u8; 6] = [2, 0, 0, 0, 0, 2];
    const SNONCE: [u8; 32] = [0x55; 32];
    const ANONCE: [u8; 32] = [0xAA; 32];
    const GTK: [u8; 16] = [0x77; 16];

    fn pmk() -> [u8; 32] {
        crypto::wpa_key(b"password", b"IEEE")
    }

    fn supplicant() -> Supplicant {
        Supplicant::new(pmk(), OWN, AP, &OWN_RSN, SNONCE).unwrap()
    }

    /// What an access point sends. A `kck` of `None` leaves the check value zero.
    fn ap_frame(info: u16, replay: u8, nonce: &[u8; 32], data: &[u8], kck: Option<&[u8; 16]>) -> ([u8; 512], usize) {
        let mut frame = [0; 512];
        let length = KEY_DATA + data.len();
        frame[0] = 2;
        frame[1] = EAPOL_TYPE_KEY;
        frame[2..4].copy_from_slice(&((length - 4) as u16).to_be_bytes());
        frame[DESCRIPTOR] = DESCRIPTOR_RSN;
        frame[KEY_INFO..KEY_INFO + 2].copy_from_slice(&info.to_be_bytes());
        frame[REPLAY + 7] = replay;
        frame[NONCE..NONCE + 32].copy_from_slice(nonce);
        frame[RSC] = 9;
        frame[KEY_DATA_LENGTH..KEY_DATA].copy_from_slice(&(data.len() as u16).to_be_bytes());
        frame[KEY_DATA..length].copy_from_slice(data);
        if let Some(kck) = kck {
            let digest = Hmac::new(kck).sign(&[&frame[..length]]);
            frame[MIC..MIC + MIC_LENGTH].copy_from_slice(&digest[..MIC_LENGTH]);
        }
        (frame, length)
    }

    /// The security element and the group key, padded and wrapped.
    fn wrapped_data(kek: &[u8; 16], with_rsn: &[u8], index: u8) -> ([u8; 128], usize) {
        let mut plain = [0; 120];
        let mut at = with_rsn.len();
        plain[..at].copy_from_slice(with_rsn);
        plain[at] = ELEMENT_VENDOR;
        plain[at + 1] = 22;
        plain[at + 2..at + 6].copy_from_slice(&KDE_GROUP_KEY);
        plain[at + 6] = index;
        plain[at + 8..at + 24].copy_from_slice(&GTK);
        at += 24;
        if at % 8 != 0 {
            plain[at] = 0xDD;
            at = at.next_multiple_of(8);
        }
        let mut wrapped = [0; 128];
        crypto::key_wrap(kek, &plain[..at], &mut wrapped);
        (wrapped, at + 8)
    }

    const INFO_M1: u16 = INFO_VERSION_SHA1_AES | INFO_PAIRWISE | INFO_ACK;
    const INFO_M3: u16 =
        INFO_VERSION_SHA1_AES | INFO_PAIRWISE | INFO_ACK | INFO_MIC | INFO_INSTALL | INFO_SECURE | INFO_ENCRYPTED;
    const INFO_GROUP: u16 = INFO_VERSION_SHA1_AES | INFO_ACK | INFO_MIC | INFO_SECURE | INFO_ENCRYPTED;

    /// Runs message 1, and returns the keys.
    fn after_first(client: &mut Supplicant, reply: &mut [u8]) -> Ptk {
        let (frame, length) = ap_frame(INFO_M1, 1, &ANONCE, &[], None);
        let step = client.handle(&frame[..length], reply).unwrap();
        assert_eq!(step.message, Message::First);
        client.pending.unwrap()
    }

    #[test]
    fn second_message_matches_python() {
        let mut client = supplicant();
        let mut reply = [0; REPLY_MAX];
        let (frame, length) = ap_frame(INFO_M1, 1, &ANONCE, &[], None);
        let step = client.handle(&frame[..length], &mut reply).unwrap();
        assert_eq!(step.reply_length, 121);
        assert_eq!(reply[..4], [1, 3, 0, 117]);
        assert_eq!(reply[KEY_INFO..KEY_INFO + 2], [0x01, 0x0A]);
        assert_eq!(reply[NONCE..NONCE + 32], SNONCE);
        assert_eq!(reply[KEY_DATA..step.reply_length], OWN_RSN);
        assert_eq!(reply[MIC..MIC + MIC_LENGTH], [
            0x18, 0xAA, 0x34, 0xAF, 0x63, 0x08, 0x59, 0x8E, 0xF1, 0x14, 0x54, 0x14, 0x70, 0xEE, 0x6C, 0xC9,
        ]);
    }

    #[test]
    fn full_handshake_then_group_rekey() {
        let mut client = supplicant();
        let mut reply = [0; REPLY_MAX];
        let ptk = after_first(&mut client, &mut reply);

        let (data, data_length) = wrapped_data(&ptk.kek, &OWN_RSN, 1);
        let (frame, length) = ap_frame(INFO_M3, 2, &ANONCE, &data[..data_length], Some(&ptk.kck));
        let step = client.handle(&frame[..length], &mut reply).unwrap();
        assert_eq!(step.message, Message::Third);
        assert_eq!(step.pairwise, Some(ptk.tk));
        let group = step.group.unwrap();
        assert_eq!((group.index, group.key, group.rsc[0]), (1, GTK, 9));
        assert_eq!(step.reply_length, KEY_DATA);
        check_mic(&reply[..KEY_DATA], &ptk.kck).unwrap();
        assert!(client.is_complete());

        // A repeated message 3 is answered, but changes no key.
        let (frame, length) = ap_frame(INFO_M3, 3, &ANONCE, &data[..data_length], Some(&ptk.kck));
        let step = client.handle(&frame[..length], &mut reply).unwrap();
        assert_eq!((step.pairwise, step.group, step.reply_length), (None, None, KEY_DATA));

        // The same frame again is a replay.
        assert_eq!(client.handle(&frame[..length], &mut reply), Err(Error::Replayed));

        let (data, data_length) = wrapped_data(&ptk.kek, &[], 2);
        let (frame, length) = ap_frame(INFO_GROUP, 4, &[0; 32], &data[..data_length], Some(&ptk.kck));
        let step = client.handle(&frame[..length], &mut reply).unwrap();
        assert_eq!(step.message, Message::Group);
        assert_eq!(step.group.unwrap().index, 2);
        assert_eq!(reply[KEY_INFO..KEY_INFO + 2], [0x03, 0x02]);
    }

    #[test]
    fn third_message_with_wrong_key_is_refused() {
        let mut client = supplicant();
        let mut reply = [0; REPLY_MAX];
        let ptk = after_first(&mut client, &mut reply);
        let (data, data_length) = wrapped_data(&ptk.kek, &OWN_RSN, 1);
        let (frame, length) = ap_frame(INFO_M3, 2, &ANONCE, &data[..data_length], Some(&[1; 16]));
        assert_eq!(client.handle(&frame[..length], &mut reply), Err(Error::BadMic));
        assert!(!client.is_complete());
    }

    #[test]
    fn downgrade_is_refused() {
        let mut client = supplicant();
        let mut reply = [0; REPLY_MAX];
        let ptk = after_first(&mut client, &mut reply);
        let mut other = OWN_RSN;
        other[13] = 2;
        let (data, data_length) = wrapped_data(&ptk.kek, &other, 1);
        let (frame, length) = ap_frame(INFO_M3, 2, &ANONCE, &data[..data_length], Some(&ptk.kck));
        let error = client.handle(&frame[..length], &mut reply).unwrap_err();
        assert_eq!(error, Error::RsnMismatch);
        assert!(error.is_fatal());
    }

    #[test]
    fn group_message_before_the_pairwise_keys_is_refused() {
        let mut client = supplicant();
        let mut reply = [0; REPLY_MAX];
        let (frame, length) = ap_frame(INFO_GROUP, 1, &[0; 32], &[0; 32], Some(&[0; 16]));
        assert_eq!(client.handle(&frame[..length], &mut reply), Err(Error::Unexpected(0)));
    }

    #[test]
    fn bad_input_does_not_panic() {
        let mut client = supplicant();
        let mut reply = [0; REPLY_MAX];
        for length in 0..KEY_DATA + 8 {
            let bytes = [0xFF; 128];
            assert!(client.handle(&bytes[..length], &mut reply).is_err());
        }
        // Declares more key data than there is.
        let (mut frame, length) = ap_frame(INFO_M1, 1, &ANONCE, &[1, 2, 3, 4], None);
        frame[KEY_DATA_LENGTH + 1] = 200;
        assert_eq!(client.handle(&frame[..length], &mut reply), Err(Error::NotKeyFrame));
    }

    #[test]
    fn padding_after_the_frame_is_ignored() {
        let mut client = supplicant();
        let mut reply = [0; REPLY_MAX];
        let (frame, length) = ap_frame(INFO_M1, 1, &ANONCE, &[], None);
        assert!(client.handle(&frame[..length + 30], &mut reply).is_ok());
    }

    #[test]
    fn network_support() {
        assert!(rsn_supported(&OWN_RSN));
        assert!(!rsn_supported(&[]));
        let mut tkip_group = OWN_RSN;
        tkip_group[7] = 2;
        assert!(!rsn_supported(&tkip_group));
        let mut sae_only = OWN_RSN;
        sae_only[19] = 8;
        assert!(!rsn_supported(&sae_only));
        let mut protected = OWN_RSN;
        protected[20] = 1 << 6;
        assert!(!rsn_supported(&protected));
        let mut optional = OWN_RSN;
        optional[20] = 1 << 7;
        assert!(rsn_supported(&optional));
        // Traffic may use either cipher, as long as AES is one.
        let mixed = [
            48, 24, 1, 0, 0, 0x0F, 0xAC, 4, 2, 0, 0, 0x0F, 0xAC, 2, 0, 0x0F, 0xAC, 4, 1, 0, 0, 0x0F, 0xAC, 2, 0, 0,
        ];
        assert!(rsn_supported(&mixed));
        // Declares more suites than it holds.
        let short = [48, 8, 1, 0, 0, 0x0F, 0xAC, 4, 9, 0];
        assert!(!rsn_supported(&short));
    }
}
