//! The host side: enumerating the one device on the EHCI's port ([`Ehci`]), and what it
//! offers. [`msc`] then drives a mass storage device.
//!
//! Requests and descriptors: USB 2.0, chapter 9.

pub mod msc;

use crate::drivers::ehci::{self, Ehci};
use crate::usb::{Setup, Speed, descriptor, request};

/// The address the device gets.
pub const ADDRESS: u8 = 1;
/// USB 2.0, 9.2.6.3: the device may take 2 ms to answer at its new address.
const SET_ADDRESS_US: u64 = 2_000;
/// The longest configuration descriptor read.
const CONFIGURATION_BYTES: usize = 512;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller's error.
    Ehci(ehci::Error),
    /// A descriptor that is too short or not what it says.
    BadDescriptor(u8),
}

impl From<ehci::Error> for Error {
    fn from(error: ehci::Error) -> Self {
        Self::Ehci(error)
    }
}

/// A bulk endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    /// Its address, bit 7 for IN.
    pub address: u8,
    /// Its packet size.
    pub packet: u16,
}

/// A mass storage interface with the Bulk-Only Transport: class 8, protocol 0x50.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MassStorage {
    /// The interface number.
    pub interface: u8,
    /// The command set: 6 is SCSI.
    pub subclass: u8,
    /// Bulk IN.
    pub input: Endpoint,
    /// Bulk OUT.
    pub output: Endpoint,
}

/// The device, as enumeration found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    /// The link's speed.
    pub speed: Speed,
    /// The device descriptor.
    pub descriptor: [u8; 18],
    /// The configuration set.
    pub configuration: u8,
    /// Interfaces in it.
    pub interfaces: u8,
    /// The class of the first interface.
    pub first_class: (u8, u8, u8),
    /// A Bulk-Only mass storage interface, if there is one.
    pub mass_storage: Option<MassStorage>,
}

impl Device {
    /// `idVendor`.
    pub const fn vendor(&self) -> u16 {
        u16::from_le_bytes([self.descriptor[8], self.descriptor[9]])
    }
    /// `idProduct`.
    pub const fn product(&self) -> u16 {
        u16::from_le_bytes([self.descriptor[10], self.descriptor[11]])
    }
    /// `bcdUSB`.
    pub const fn usb_version(&self) -> u16 {
        u16::from_le_bytes([self.descriptor[2], self.descriptor[3]])
    }
    /// `bDeviceClass`: 9 is a hub.
    pub const fn class(&self) -> u8 {
        self.descriptor[4]
    }
    /// The indexes of the manufacturer, product and serial number strings, 0 for none.
    pub const fn strings(&self) -> [u8; 3] {
        [self.descriptor[14], self.descriptor[15], self.descriptor[16]]
    }
}

const fn get_descriptor(kind: u8, index: u8, language: u16, length: u16) -> Setup {
    Setup {
        request_type: 0x80,
        request: request::GET_DESCRIPTOR,
        value: (kind as u16) << 8 | index as u16,
        index: language,
        length,
    }
}

/// Resets the device on the port and enumerates it: its device descriptor, address
/// [`ADDRESS`], its first configuration read and set. Leaves the control pipe at that address.
pub fn enumerate(ehci: &mut Ehci) -> Result<Device, Error> {
    let speed = ehci.reset_port(ehci::PORT)?;
    ehci.set_control(0, 64);
    // The first 8 bytes give endpoint 0's packet size; at high speed it is 64 anyway.
    let mut buffer = [0u8; 18];
    ehci.control(get_descriptor(descriptor::DEVICE, 0, 0, 8), &mut buffer)?;
    let packet = u16::from(buffer[7]).max(8);
    let set_address = Setup { request_type: 0, request: request::SET_ADDRESS, value: u16::from(ADDRESS), index: 0, length: 0 };
    ehci.control(set_address, &mut [])?;
    crate::sleep::for_us(ehci.mct(), SET_ADDRESS_US);
    ehci.set_control(ADDRESS, packet);

    let read = ehci.control(get_descriptor(descriptor::DEVICE, 0, 0, 18), &mut buffer)?;
    if read != 18 || buffer[1] != descriptor::DEVICE {
        return Err(Error::BadDescriptor(descriptor::DEVICE));
    }
    let device_descriptor = buffer;

    let mut config = [0u8; CONFIGURATION_BYTES];
    let read = ehci.control(get_descriptor(descriptor::CONFIGURATION, 0, 0, 9), &mut config)?;
    if read < 9 || config[1] != descriptor::CONFIGURATION {
        return Err(Error::BadDescriptor(descriptor::CONFIGURATION));
    }
    let total = usize::from(u16::from_le_bytes([config[2], config[3]])).min(CONFIGURATION_BYTES);
    let read = ehci.control(get_descriptor(descriptor::CONFIGURATION, 0, 0, total as u16), &mut config)?;
    let config = &config[..read];
    let (value, interfaces) = (config[5], config[4]);
    let (first_class, mass_storage) = parse(config);

    let set = Setup { request_type: 0, request: request::SET_CONFIGURATION, value: u16::from(value), index: 0, length: 0 };
    ehci.control(set, &mut [])?;
    Ok(Device {
        speed,
        descriptor: device_descriptor,
        configuration: value,
        interfaces,
        first_class,
        mass_storage,
    })
}

/// Walks a configuration's descriptors: the first interface's class, and the first Bulk-Only
/// mass storage interface with its two bulk endpoints.
fn parse(config: &[u8]) -> ((u8, u8, u8), Option<MassStorage>) {
    let mut first = None;
    let mut found: Option<MassStorage> = None;
    let mut current: Option<MassStorage> = None;
    let mut at = 0;
    while at + 2 <= config.len() {
        let len = usize::from(config[at]);
        if len < 2 || at + len > config.len() {
            break;
        }
        let d = &config[at..at + len];
        match d[1] {
            descriptor::INTERFACE if len >= 9 => {
                if let Some(done) = current.take().filter(|m| m.input.address != 0 && m.output.address != 0)
                    && found.is_none()
                {
                    found = Some(done);
                }
                first.get_or_insert((d[5], d[6], d[7]));
                if d[5] == 0x08 && d[7] == 0x50 && d[3] == 0 {
                    current = Some(MassStorage {
                        interface: d[2],
                        subclass: d[6],
                        input: Endpoint::default(),
                        output: Endpoint::default(),
                    });
                }
            }
            descriptor::ENDPOINT if len >= 7 => {
                if let Some(ms) = current.as_mut()
                    && d[3] & 3 == 2
                {
                    let endpoint = Endpoint { address: d[2], packet: u16::from_le_bytes([d[4], d[5]]) & 0x7FF };
                    if d[2] & 0x80 != 0 {
                        ms.input = endpoint;
                    } else {
                        ms.output = endpoint;
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    if let Some(done) = current.filter(|m| m.input.address != 0 && m.output.address != 0)
        && found.is_none()
    {
        found = Some(done);
    }
    (first.unwrap_or_default(), found)
}

/// Reads string descriptor `index` in US English, as ASCII into `out` (other characters as
/// `?`): its length. Index 0 gives nothing.
pub fn string(ehci: &mut Ehci, index: u8, out: &mut [u8]) -> Result<usize, Error> {
    if index == 0 {
        return Ok(0);
    }
    let mut buffer = [0u8; 255];
    let read = ehci.control(get_descriptor(descriptor::STRING, index, 0x0409, 255), &mut buffer)?;
    if read < 2 || buffer[1] != descriptor::STRING {
        return Err(Error::BadDescriptor(descriptor::STRING));
    }
    let len = usize::from(buffer[0]).min(read);
    let mut count = 0;
    for unit in buffer[2..len].as_chunks::<2>().0 {
        if count == out.len() {
            break;
        }
        let unit = u16::from_le_bytes([unit[0], unit[1]]);
        out[count] = if (0x20..0x7F).contains(&unit) { unit as u8 } else { b'?' };
        count += 1;
    }
    Ok(count)
}
