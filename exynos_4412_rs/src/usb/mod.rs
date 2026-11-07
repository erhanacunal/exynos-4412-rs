//! USB protocol pieces shared by the controllers' drivers and the classes: the SETUP packet,
//! the standard requests and descriptors (USB 2.0 specification, chapter 9), and the
//! descriptors a device presents.
//!
//! - [`msc`]: a mass storage device (Bulk-Only Transport, SCSI) on the OTG controller
//!   ([`otg`](crate::drivers::otg)), over any [`msc::BlockDevice`].

pub mod msc;

/// Bit 7 of an endpoint address: the IN direction, device to host.
pub const DIR_IN: u8 = 0x80;

/// Descriptor types: USB 2.0, table 9-5.
pub mod descriptor {
    /// The device descriptor.
    pub const DEVICE: u8 = 1;
    /// A configuration descriptor, with its interfaces and endpoints after it.
    pub const CONFIGURATION: u8 = 2;
    /// A string descriptor.
    pub const STRING: u8 = 3;
    /// An interface descriptor.
    pub const INTERFACE: u8 = 4;
    /// An endpoint descriptor.
    pub const ENDPOINT: u8 = 5;
    /// What the device would be at the other speed.
    pub const DEVICE_QUALIFIER: u8 = 6;
    /// The configuration at the other speed.
    pub const OTHER_SPEED_CONFIGURATION: u8 = 7;
}

/// Standard request codes: USB 2.0, table 9-4.
pub mod request {
    /// `GET_STATUS`.
    pub const GET_STATUS: u8 = 0;
    /// `CLEAR_FEATURE`.
    pub const CLEAR_FEATURE: u8 = 1;
    /// `SET_FEATURE`.
    pub const SET_FEATURE: u8 = 3;
    /// `SET_ADDRESS`.
    pub const SET_ADDRESS: u8 = 5;
    /// `GET_DESCRIPTOR`.
    pub const GET_DESCRIPTOR: u8 = 6;
    /// `GET_CONFIGURATION`.
    pub const GET_CONFIGURATION: u8 = 8;
    /// `SET_CONFIGURATION`.
    pub const SET_CONFIGURATION: u8 = 9;
    /// `GET_INTERFACE`.
    pub const GET_INTERFACE: u8 = 10;
    /// `SET_INTERFACE`.
    pub const SET_INTERFACE: u8 = 11;
    /// Feature selector `ENDPOINT_HALT`: table 9-6.
    pub const FEATURE_ENDPOINT_HALT: u16 = 0;
}

/// Who a request is for, from `bmRequestType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    /// The device.
    Device,
    /// An interface: [`Setup::index`] is its number.
    Interface,
    /// An endpoint: [`Setup::index`] is its address.
    Endpoint,
    /// Anything else.
    Other,
}

/// What a request is, from `bmRequestType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Chapter 9's.
    Standard,
    /// The class's.
    Class,
    /// The vendor's.
    Vendor,
    /// Reserved.
    Reserved,
}

/// A SETUP packet: USB 2.0, table 9-2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    /// `bmRequestType`.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// `wLength`: the most the data stage carries.
    pub length: u16,
}

impl Setup {
    /// The packet's eight bytes, little-endian fields.
    pub const fn from_bytes(bytes: &[u8; 8]) -> Self {
        Self {
            request_type: bytes[0],
            request: bytes[1],
            value: u16::from_le_bytes([bytes[2], bytes[3]]),
            index: u16::from_le_bytes([bytes[4], bytes[5]]),
            length: u16::from_le_bytes([bytes[6], bytes[7]]),
        }
    }

    /// The eight bytes, as they go on the wire.
    pub const fn to_bytes(&self) -> [u8; 8] {
        let value = self.value.to_le_bytes();
        let index = self.index.to_le_bytes();
        let length = self.length.to_le_bytes();
        [self.request_type, self.request, value[0], value[1], index[0], index[1], length[0], length[1]]
    }

    /// Whether the data stage, if any, goes to the host.
    pub const fn is_in(&self) -> bool {
        self.request_type & DIR_IN != 0
    }

    /// Standard, class or vendor.
    pub const fn kind(&self) -> Kind {
        match (self.request_type >> 5) & 3 {
            0 => Kind::Standard,
            1 => Kind::Class,
            2 => Kind::Vendor,
            _ => Kind::Reserved,
        }
    }

    /// Device, interface or endpoint.
    pub const fn recipient(&self) -> Recipient {
        match self.request_type & 0x1F {
            0 => Recipient::Device,
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            _ => Recipient::Other,
        }
    }
}

/// The speed a link runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    /// 480 Mbit/s.
    High,
    /// 12 Mbit/s.
    Full,
    /// 1.5 Mbit/s.
    Low,
}

/// What a device presents: its descriptors, written by the class, and its strings. The driver
/// answers `GET_DESCRIPTOR` from these, and makes the device qualifier and the other speed's
/// configuration from them.
#[derive(Clone, Copy, Debug)]
pub struct Descriptors<'a> {
    /// The device descriptor, 18 bytes, `bMaxPacketSize0` 64.
    pub device: &'a [u8; 18],
    /// The one configuration at high speed, all its descriptors after one another.
    pub high_speed: &'a [u8],
    /// The same at full speed.
    pub full_speed: &'a [u8],
    /// String descriptors 1, 2 and on, sent in UTF-16; 0, the language list, is US English.
    pub strings: &'a [&'a str],
}
