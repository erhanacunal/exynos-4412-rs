//! Atmel maXTouch mXT1664S touch controller, on `i2c_5` at [`ADDRESS`]: power-up, its object
//! table, and touch messages.
//!
//! Paths are relative to the Linux 5.15.198 tree; `atmel_mxt_ts.c` is in
//! `drivers/input/touchscreen/`. The vendor driver is `1664s_driver_v13.c` in the same
//! directory of the vendor kernel.
//!
//! The controller keeps its configuration in its own non-volatile memory, where the vendor
//! driver put it (`1664s_driver_v13.c:4995-5014`). This driver reads that configuration and
//! never stores one: a write reaches only the controller's RAM, and a reset or power-off
//! brings back what was stored. The writes are:
//!
//! - the calibrate command of the command processor, [`Mxt::calibrate`], which makes the
//!   controller take its reference of an untouched screen again;
//! - [`Mxt::wake`], the run settings of the power and touch objects, and only where the
//!   stored ones would report nothing.
//!
//! The backup command, which stores the configuration, is never sent. Neither is anything to
//! the bootloader, which answers at [`BOOTLOADER_ADDRESS`] after some resets: its protocol
//! replaces the firmware. Every write goes to [`ADDRESS`], fixed here.
//!
//! The controller pulls [`CHG`] low while it has messages. [`Mxt::read_message`] takes one,
//! `None` once there are none left. A polled reader reads while CHG is low. With the
//! `embassy` feature, `Mxt::next_message` waits on CHG's interrupt instead, through an
//! [`EintInput`](crate::embassy::eint::EintInput), and awaits the I2C bytes.
//!
//! ```ignore
//! mxt::power_up(pmic, mct)?;
//! let touch = Mxt::open(I2c::new(Bus::I2c5))?;
//! while !mxt::CHG.read() {
//!     if let Some(Message::Touch(touch)) = touch.read_message()? { /* ... */ }
//! }
//! ```

use crate::drivers::gpio::{self, Pin, Pull};
use crate::drivers::i2c::{self, Bus, I2c};
use crate::drivers::max77686::{self, Max77686};
use crate::drivers::timer::Mct;
use crate::soc;

/// `exynos4412-mehmet.dts`, `touchscreen@4a` on `i2c_5`.
pub const ADDRESS: u8 = 0x4A;
/// Where the controller's bootloader answers, seen on this board after BUCK6 and BUCK7 were
/// raised with the controller already powered (`atmel_mxt_ts.c:480-484`). Only probed, by
/// address alone.
pub const BOOTLOADER_ADDRESS: u8 = 0x26;
/// `samsung,i2c-max-bus-freq` of `i2c_5` in the DTS.
pub const BUS_HZ: u32 = 100_000;
/// `samsung,i2c-sda-delay` of `i2c_5` in the DTS.
pub const BUS_SDA_DELAY_NS: u32 = 100;

/// Reset, active low: `gpio-mehmet.h:121`, `reset-gpios` in the DTS.
pub const RESET: Pin = gpio::GPB.pin(4);
/// `gpio-mehmet.h:122`, `wake-gpios` in the DTS: high while the controller runs.
pub const XVDD_ENABLE: Pin = gpio::GPM3.pin(3);
/// `atmel,irq-gpio` in the DTS: low while the controller has messages.
pub const CHG: Pin = gpio::GPX0.pin(4);

/// `vdd_tsp_1v8` and `vdd_tsp`, `exynos4412-mehmet.dts:1011-1014` and `1152-1155`.
const LDO_LOGIC: (u8, u16) = (6, 1800);
const LDO_ANALOG: (u8, u16) = (26, 3000);
/// What Linux leaves BUCK7 and BUCK6 at on this board. The controller does not answer with
/// U-Boot's 2000 and 1350 mV: see the README's MAX77686 section.
const BUCK7: (u8, u16) = (7, 3300);
const BUCK6: (u8, u16) = (6, 2000);
/// The power-up times of `atmel_mxt_ts.c:3200-3243`, which work on this board.
const RAILS_SETTLE_MS: u64 = 10;
const RESET_HOLD_MS: u64 = 70;
const BOOT_MS: u64 = 300;

/// `atmel_mxt_ts.c:42-44`: the information block, then the object table, then a CRC.
const INFO_BYTES: usize = 7;
const OBJECT_START: u16 = 0x07;
const OBJECT_BYTES: usize = 6;
const CRC_BYTES: usize = 3;
/// More than the 33 objects of this firmware.
pub const MAX_OBJECTS: usize = 64;
/// `atmel_mxt_ts.c:79`: what T5 holds when no message is waiting.
const NO_MESSAGE: u8 = 0xFF;
/// Bytes of a message kept, the report ID first. T5 of this firmware is larger than the ones
/// any object here sends.
pub const MESSAGE_BYTES: usize = 16;

/// Object types, `atmel_mxt_ts.c:46-77`: the message processor, where messages are read.
pub const T5_MESSAGE: u8 = 5;
/// The command processor: reset, backup, calibrate.
pub const T6_COMMAND: u8 = 6;
/// The power settings.
pub const T7_POWER: u8 = 7;
/// The multi-touch screen.
pub const T9_TOUCH: u8 = 9;

/// `atmel_mxt_ts.c:82-84`: offsets in T6 of its commands.
const T6_CALIBRATE: u16 = 2;
/// `atmel_mxt_ts.c:106-110`: offsets in T9.
const T9_CTRL: u16 = 0;
const T9_ORIENT: u16 = 9;
const T9_NUMTOUCH: u16 = 14;
const T9_RANGE: u16 = 18;
/// T9 CTRL: the object is on and sends its messages.
const T9_CTRL_ENABLE: u8 = 1 << 0;
const T9_CTRL_REPORT: u8 = 1 << 1;
/// `atmel_mxt_ts.c:2290-2294`: what Linux runs with when the stored T7 is zero, which is
/// deep sleep. T7 holds the idle interval first, then the active one.
const T7_IDLE_MS: u8 = 100;
const T7_ACTIVE_MS: u8 = 20;

/// T9 status bits, `atmel_mxt_ts.c:113-120`: the touch was suppressed, a palm for one.
pub const STATUS_SUPPRESS: u8 = 1 << 1;
/// It moved.
pub const STATUS_MOVE: u8 = 1 << 4;
/// It ended.
pub const STATUS_RELEASE: u8 = 1 << 5;
/// It began.
pub const STATUS_PRESS: u8 = 1 << 6;
/// The finger is on the screen.
pub const STATUS_DETECT: u8 = 1 << 7;
/// T9 ORIENT bits, `atmel_mxt_ts.c:128-130`: X and Y are swapped.
pub const ORIENT_SWITCH: u8 = 1 << 0;
/// X counts the other way.
pub const ORIENT_INVERT_X: u8 = 1 << 1;
/// Y counts the other way.
pub const ORIENT_INVERT_Y: u8 = 1 << 2;
/// T6 status bits, `atmel_mxt_ts.c:89-94`: the controller has reset.
pub const T6_RESET: u8 = 1 << 7;
/// Messages were lost.
pub const T6_OVERFLOW: u8 = 1 << 6;
/// The measurement went wrong.
pub const T6_SIGNAL_ERROR: u8 = 1 << 5;
/// A calibration is going on.
pub const T6_CALIBRATING: u8 = 1 << 4;
/// The stored configuration is bad.
pub const T6_CONFIG_ERROR: u8 = 1 << 3;
/// An I2C checksum was wrong.
pub const T6_COMMS_ERROR: u8 = 1 << 2;

/// Why a call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The I2C transfer failed.
    Bus(i2c::Error),
    /// A PMIC call failed during the power-up.
    Pmic(max77686::Error),
    /// The controller did not answer at [`ADDRESS`]; whether the bootloader did.
    NoController {
        /// The bootloader answered at [`BOOTLOADER_ADDRESS`].
        bootloader: bool,
    },
    /// The object table is larger than [`MAX_OBJECTS`].
    TooManyObjects(u8),
    /// The CRC after the object table is not the one computed.
    Crc {
        /// What the controller holds.
        read: u32,
        /// What its information block and object table give.
        computed: u32,
    },
    /// The firmware has no object of this type, which this driver needs.
    MissingObject(u8),
}

impl From<i2c::Error> for Error {
    fn from(error: i2c::Error) -> Self {
        Self::Bus(error)
    }
}

impl From<max77686::Error> for Error {
    fn from(error: max77686::Error) -> Self {
        Self::Pmic(error)
    }
}

fn wait_ms(mct: Mct, ms: u64) {
    let end = mct.count() + ms * u64::from(soc::MCT_CLOCK_HZ / 1000);
    while mct.count() < end {
        core::hint::spin_loop();
    }
}

/// Starts the controller from off, or restarts it: raises BUCK7 and BUCK6 to Linux's values,
/// holds reset while its rails come on, and releases it. Then checks that it answers at
/// [`ADDRESS`]; if the bootloader answers instead, resets it once more, which has always
/// brought it back. `pmic` is on `i2c_0`, initialized.
///
/// Returns how many resets it took.
pub fn power_up(pmic: Max77686, mct: Mct) -> Result<u32, Error> {
    CHG.set_input(Pull::None);
    for (number, millivolts) in [BUCK7, BUCK6] {
        if pmic.buck_millivolts(number)? != millivolts {
            pmic.set_buck_millivolts(number, millivolts)?;
        }
    }
    wait_ms(mct, RAILS_SETTLE_MS);
    RESET.set_output(false);
    XVDD_ENABLE.set_output(true);
    for (number, millivolts) in [LDO_ANALOG, LDO_LOGIC] {
        pmic.enable_ldo(number, millivolts)?;
        wait_ms(mct, RAILS_SETTLE_MS);
    }

    let i2c = I2c::new(Bus::I2c5);
    i2c.init(BUS_HZ, BUS_SDA_DELAY_NS)?;
    for resets in 1..=2 {
        wait_ms(mct, RESET_HOLD_MS);
        RESET.write(true);
        wait_ms(mct, BOOT_MS);
        if i2c.probe(ADDRESS).is_ok() {
            return Ok(resets);
        }
        RESET.write(false);
    }
    RESET.write(true);
    Err(Error::NoController { bootloader: i2c.probe(BOOTLOADER_ADDRESS).is_ok() })
}

/// The information block: `struct mxt_info`, `atmel_mxt_ts.c:225-233`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// `0xa2` for the mXT1664S.
    pub family: u8,
    /// The variant within the family.
    pub variant: u8,
    /// The firmware version, `0x20` for 2.0.
    pub version: u8,
    /// The firmware build.
    pub build: u8,
    /// Lines of the sensor matrix, X then Y.
    pub matrix_x: u8,
    /// See `matrix_x`.
    pub matrix_y: u8,
    /// Entries of the object table.
    pub objects: u8,
}

/// One entry of the object table: `struct mxt_object`, `atmel_mxt_ts.c:235-241`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Object {
    /// Its type, `T5_MESSAGE` and the like.
    pub kind: u8,
    /// Where its first instance starts.
    pub address: u16,
    /// Bytes of one instance.
    pub size: u16,
    /// How many there are, from 1.
    pub instances: u8,
    /// Report IDs of one instance.
    pub report_ids: u8,
    /// The first report ID of the first instance, 0 for an object that sends none.
    pub first_report_id: u8,
}

impl Object {
    /// Whether `report_id` is one of this object's, of any instance.
    pub fn owns(&self, report_id: u8) -> bool {
        let count = u16::from(self.report_ids) * u16::from(self.instances);
        self.first_report_id != 0
            && report_id >= self.first_report_id
            && u16::from(report_id) < u16::from(self.first_report_id) + count
    }
}

/// What T9 is configured for, as stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchConfig {
    /// CTRL: bit 0 on, bit 1 reports.
    pub ctrl: u8,
    /// [`ORIENT_SWITCH`], [`ORIENT_INVERT_X`], [`ORIENT_INVERT_Y`].
    pub orient: u8,
    /// Touches it tracks at most.
    pub touches: u8,
    /// The largest X and Y it reports.
    pub x_range: u16,
    /// See `x_range`.
    pub y_range: u16,
}

/// The power settings of T7: 0 for both is deep sleep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerConfig {
    /// Between measurements with nothing touching, in ms.
    pub idle_ms: u8,
    /// Between measurements while touched, in ms.
    pub active_ms: u8,
}

/// One touch of T9: `mxt_proc_t9_message`, `atmel_mxt_ts.c:825-846`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Touch {
    /// Which of the tracked touches, from 0.
    pub id: u8,
    /// The `STATUS_*` bits.
    pub status: u8,
    /// In T9's ranges, before any orientation of this driver: T9 applies its own ORIENT.
    pub x: u16,
    /// See `x`.
    pub y: u16,
    /// Sensor nodes covered, 0 for a narrow tip such as a stylus.
    pub area: u8,
    /// Strength of the signal.
    pub amplitude: u8,
}

impl Touch {
    /// Whether the finger is on the screen.
    pub const fn down(&self) -> bool {
        self.status & STATUS_DETECT != 0
    }
}

/// One message from T5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// From T9.
    Touch(Touch),
    /// From T6.
    Status {
        /// The `T6_*` bits.
        status: u8,
        /// The CRC of the stored configuration.
        config_crc: u32,
    },
    /// From any other object.
    Other {
        /// Its type, 0 if no object owns the report ID.
        kind: u8,
        /// The message, from the report ID on.
        bytes: [u8; MESSAGE_BYTES],
    },
}

/// The controller after its object table was read. Holds a copy of the table.
#[derive(Clone, Debug)]
pub struct Mxt {
    i2c: I2c,
    info: Info,
    objects: [Object; MAX_OBJECTS],
    t5: Object,
    t6: Object,
    t7: Object,
    t9: Object,
    /// Whether T9 reports 10-bit positions: `atmel_mxt_ts.c:841-845`.
    t9_x_10bit: bool,
    t9_y_10bit: bool,
}

/// `mxt_calc_crc24`, `atmel_mxt_ts.c:1295-1308`.
fn crc24(crc: u32, first: u8, second: u8) -> u32 {
    let result = (crc << 1) ^ (u32::from(second) << 8 | u32::from(first));
    if result & 0x100_0000 != 0 { result ^ 0x80_001B } else { result }
}

/// `mxt_calculate_crc`, `atmel_mxt_ts.c:1310-1332`: two bytes at a time, an odd last one with 0.
fn crc_of(parts: &[&[u8]]) -> u32 {
    let mut crc = 0;
    let mut pending: Option<u8> = None;
    for byte in parts.iter().flat_map(|part| part.iter().copied()) {
        match pending.take() {
            Some(first) => crc = crc24(crc, first, byte),
            None => pending = Some(byte),
        }
    }
    if let Some(first) = pending {
        crc = crc24(crc, first, 0);
    }
    crc & 0xFF_FFFF
}

impl Mxt {
    /// Reads the information block and the object table, and checks the CRC after them.
    /// `i2c` is `i2c_5`, initialized, with the controller answering.
    pub fn open(i2c: I2c) -> Result<Self, Error> {
        let mut raw = [0; INFO_BYTES];
        read(i2c, 0, &mut raw)?;
        let info = Info {
            family: raw[0],
            variant: raw[1],
            version: raw[2],
            build: raw[3],
            matrix_x: raw[4],
            matrix_y: raw[5],
            objects: raw[6],
        };
        let count = usize::from(info.objects);
        if count > MAX_OBJECTS {
            return Err(Error::TooManyObjects(info.objects));
        }

        let mut table = [0; MAX_OBJECTS * OBJECT_BYTES];
        let table = &mut table[..count * OBJECT_BYTES];
        read(i2c, OBJECT_START, table)?;
        let mut crc = [0; CRC_BYTES];
        read(i2c, OBJECT_START + table.len() as u16, &mut crc)?;
        let read_crc = u32::from(crc[0]) | u32::from(crc[1]) << 8 | u32::from(crc[2]) << 16;
        let computed = crc_of(&[&raw, table]);
        if read_crc != computed {
            return Err(Error::Crc { read: read_crc, computed });
        }

        let mut objects = [Object::default(); MAX_OBJECTS];
        // `atmel_mxt_ts.c:1712-1724`: report IDs are numbered from 1, in table order.
        let mut next_report_id: u16 = 1;
        for (object, entry) in objects.iter_mut().zip(table.as_chunks::<OBJECT_BYTES>().0) {
            *object = Object {
                kind: entry[0],
                address: u16::from_le_bytes([entry[1], entry[2]]),
                size: u16::from(entry[3]) + 1,
                instances: entry[4] + 1,
                report_ids: entry[5],
                first_report_id: 0,
            };
            if object.report_ids != 0 {
                object.first_report_id = next_report_id.min(0xFE) as u8;
                next_report_id += u16::from(object.report_ids) * u16::from(object.instances);
            }
        }
        let find = |kind| objects[..count].iter().find(|object| object.kind == kind).copied().ok_or(Error::MissingObject(kind));
        let mut mxt = Self {
            i2c,
            info,
            objects,
            t5: find(T5_MESSAGE)?,
            t6: find(T6_COMMAND)?,
            t7: find(T7_POWER)?,
            t9: find(T9_TOUCH)?,
            t9_x_10bit: false,
            t9_y_10bit: false,
        };
        let config = mxt.touch_config()?;
        mxt.t9_x_10bit = config.x_range < 1024;
        mxt.t9_y_10bit = config.y_range < 1024;
        Ok(mxt)
    }

    /// The information block.
    pub const fn info(&self) -> Info {
        self.info
    }

    /// The object table, in the controller's order.
    pub fn objects(&self) -> &[Object] {
        &self.objects[..usize::from(self.info.objects)]
    }

    /// Reads `buffer.len()` bytes of instance 0 of `object`, from `offset`.
    pub fn read_object(&self, object: &Object, offset: u16, buffer: &mut [u8]) -> Result<(), Error> {
        read(self.i2c, object.address + offset, buffer)
    }

    /// T9's configuration, read from the controller.
    pub fn touch_config(&self) -> Result<TouchConfig, Error> {
        let mut head = [0; T9_RANGE as usize + 4];
        self.read_object(&self.t9, 0, &mut head)?;
        let range = T9_RANGE as usize;
        Ok(TouchConfig {
            ctrl: head[T9_CTRL as usize],
            orient: head[T9_ORIENT as usize],
            touches: head[T9_NUMTOUCH as usize],
            x_range: u16::from_le_bytes([head[range], head[range + 1]]),
            y_range: u16::from_le_bytes([head[range + 2], head[range + 3]]),
        })
    }

    /// T7's settings, read from the controller.
    pub fn power_config(&self) -> Result<PowerConfig, Error> {
        let mut raw = [0; 2];
        self.read_object(&self.t7, 0, &mut raw)?;
        Ok(PowerConfig { idle_ms: raw[0], active_ms: raw[1] })
    }

    /// Makes the controller measure and report, if what it stored would not: T7 out of deep
    /// sleep with Linux's intervals (`atmel_mxt_ts.c:2272-2293`), T9 on with its messages.
    /// Only in RAM: see the [module documentation](self). Returns whether it wrote T7 and T9.
    pub fn wake(&self) -> Result<(bool, bool), Error> {
        let power = self.power_config()?;
        let wake_power = power.idle_ms == 0 || power.active_ms == 0;
        if wake_power {
            self.write(self.t7.address, &[T7_IDLE_MS, T7_ACTIVE_MS])?;
        }
        let ctrl = self.touch_config()?.ctrl;
        let wanted = T9_CTRL_ENABLE | T9_CTRL_REPORT;
        let wake_touch = ctrl & wanted != wanted;
        if wake_touch {
            self.write(self.t9.address + T9_CTRL, &[ctrl | wanted])?;
        }
        Ok((wake_power, wake_touch))
    }

    /// Asks the controller to take its reference of an untouched screen again. Nothing must
    /// touch the screen meanwhile. It answers with a T6 message with [`T6_CALIBRATING`].
    pub fn calibrate(&self) -> Result<(), Error> {
        self.write(self.t6.address + T6_CALIBRATE, &[1])
    }

    /// Takes one message from T5, `None` if there is none.
    pub fn read_message(&self) -> Result<Option<Message>, Error> {
        let mut bytes = [0; MESSAGE_BYTES];
        read(self.i2c, self.t5.address, &mut bytes[..self.message_length()])?;
        Ok(self.parse(bytes))
    }

    /// Bytes read from T5 per message. `atmel_mxt_ts.c:1745`: the last byte of T5 is a CRC
    /// this firmware does not send.
    fn message_length(&self) -> usize {
        usize::from(self.t5.size - 1).min(MESSAGE_BYTES)
    }

    /// What a message read from T5 says, `None` if there was none.
    fn parse(&self, bytes: [u8; MESSAGE_BYTES]) -> Option<Message> {
        let report_id = bytes[0];
        if report_id == NO_MESSAGE || report_id == 0 {
            return None;
        }
        if self.t9.owns(report_id) {
            let mut x = u16::from(bytes[2]) << 4 | u16::from(bytes[4] >> 4);
            let mut y = u16::from(bytes[3]) << 4 | u16::from(bytes[4] & 0xF);
            if self.t9_x_10bit {
                x >>= 2;
            }
            if self.t9_y_10bit {
                y >>= 2;
            }
            return Some(Message::Touch(Touch {
                id: report_id - self.t9.first_report_id,
                status: bytes[1],
                x,
                y,
                area: bytes[5],
                amplitude: bytes[6],
            }));
        }
        if self.t6.owns(report_id) {
            let config_crc = u32::from(bytes[2]) | u32::from(bytes[3]) << 8 | u32::from(bytes[4]) << 16;
            return Some(Message::Status { status: bytes[1], config_crc });
        }
        let kind = self.objects().iter().find(|object| object.owns(report_id)).map_or(0, |object| object.kind);
        Some(Message::Other { kind, bytes })
    }

    fn write(&self, register: u16, data: &[u8]) -> Result<(), Error> {
        let (frame, length) = frame(register, data);
        self.i2c.write(ADDRESS, &frame[..length])?;
        Ok(())
    }
}

#[cfg(feature = "embassy")]
impl Mxt {
    /// [`read_message`](Self::read_message), each I2C byte awaited: see
    /// [`drivers::i2c`](crate::drivers::i2c).
    pub async fn read_message_async(&self) -> Result<Option<Message>, Error> {
        let mut bytes = [0; MESSAGE_BYTES];
        let length = self.message_length();
        self.i2c.write_read_async(ADDRESS, &self.t5.address.to_le_bytes(), &mut bytes[..length]).await?;
        Ok(self.parse(bytes))
    }

    /// The next message, waiting on `chg`, made from [`CHG`], while there is none. The I2C
    /// bytes are awaited too. A message already waiting still costs its transfer, about
    /// 1.3 ms at [`BUS_HZ`], during which other tasks run.
    pub async fn next_message(&self, chg: &crate::embassy::eint::EintInput) -> Result<Message, Error> {
        loop {
            if let Some(message) = self.read_message_async().await? {
                return Ok(message);
            }
            chg.wait_low().await;
        }
    }

    /// [`calibrate`](Self::calibrate), awaited. Use this one while async reads may run on
    /// the same bus: see [`drivers::i2c`](crate::drivers::i2c).
    pub async fn calibrate_async(&self) -> Result<(), Error> {
        let (frame, length) = frame(self.t6.address + T6_CALIBRATE, &[1]);
        self.i2c.write_async(ADDRESS, &frame[..length]).await?;
        Ok(())
    }
}

/// The register as two bytes, low first, then `data`, and the length of the frame.
fn frame(register: u16, data: &[u8]) -> ([u8; 10], usize) {
    const MAX_DATA: usize = 8;
    assert!(data.len() <= MAX_DATA);
    let mut frame = [0; 2 + MAX_DATA];
    frame[..2].copy_from_slice(&register.to_le_bytes());
    frame[2..2 + data.len()].copy_from_slice(data);
    (frame, 2 + data.len())
}

/// `__mxt_read_reg`, `atmel_mxt_ts.c:660-698`: the register as two bytes, low first.
fn read(i2c: I2c, register: u16, buffer: &mut [u8]) -> Result<(), Error> {
    i2c.write_read(ADDRESS, &register.to_le_bytes(), buffer)?;
    Ok(())
}

