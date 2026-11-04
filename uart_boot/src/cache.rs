//! The saved application on CACHE (p8, FAT32): `FERRITE/FERRITE.BIN`.
//!
//! The file is the wire header followed by the image, so the CRC that came over the UART is
//! kept with it:
//!
//! | Bytes | Content |
//! |---|---|
//! | 8 | `EXYNBOOT` |
//! | 4 | length, little-endian |
//! | 4 | CRC-32 (IEEE, as zlib's) of the image, little-endian |
//! | length | the image |
//!
//! Runs with the MMU and caches off, as the rest of `uart_boot`: memory is Strongly-ordered,
//! so the IDMAC and the CPU see the same bytes without cache maintenance, only slower. CACHE is
//! never formatted here: without a FAT32 on it, there is nothing to boot and nothing is saved.

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::mmc::{self, Mmc};
use exynos_4412_rs::fat::embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};
use exynos_4412_rs::fat::{self, FixedTime, PartitionDevice};

/// CACHE.
const PARTITION: u32 = 8;
pub const DIR: &str = "FERRITE";
pub const FILE: &str = "FERRITE.BIN";
pub const HEADER_MAGIC: &[u8; 8] = b"EXYNBOOT";
const HEADER_BYTES: usize = 16;
/// Read-ahead and single blocks, plus a 32 KiB run for writes: this loader writes one file.
const DEVICE_BYTES: usize = fat::MIN_BUFFER - 512 + 32 * 1024;
/// Guesses at the address U-Boot gave the card, which it does not report (as `fat_test`).
const RCA_GUESSES: [u16; 2] = [1, 2];

type FsError = fat::embedded_sdmmc::Error<fat::Error>;
type Volumes<'a> = VolumeManager<PartitionDevice<'a>, FixedTime, 4, 4, 1>;

#[repr(C, align(32))]
struct Buffer(UnsafeCell<[u8; DEVICE_BYTES]>);

// SAFETY: one user at a time: `with_volumes` takes it for the length of one call, and this
// program has one thread and no interrupts.
unsafe impl Sync for Buffer {}

static DEVICE: Buffer = Buffer(UnsafeCell::new([0; DEVICE_BYTES]));

/// Why a load or a save failed, for the log line (fields are read through `Debug`).
#[derive(Debug)]
#[allow(dead_code)]
pub enum Error {
    /// The eMMC controller, `EXT_CSD`, or no card in transfer state.
    Card(&'static str),
    /// CACHE has no FAT32.
    NoFat,
    /// No saved application.
    NotFound,
    /// The file is not a header and an image of the length it gives.
    BadFile,
    /// The image does not match the CRC saved with it.
    Crc { want: u32, got: u32 },
    Fs(FsError),
}

impl From<FsError> for Error {
    fn from(error: FsError) -> Self {
        match error {
            FsError::NotFound => Self::NotFound,
            other => Self::Fs(other),
        }
    }
}

impl From<fat::Error> for Error {
    fn from(error: fat::Error) -> Self {
        Self::Fs(FsError::DeviceError(error))
    }
}

/// Opens CACHE and runs `f` on its volume manager, then writes the device's cache to the card.
fn with_volumes<T>(f: impl FnOnce(&Volumes<'_>) -> Result<T, Error>) -> Result<T, Error> {
    let card = Mmc::new().map_err(|_| Error::Card("controller"))?;
    match card.read_ext_csd() {
        Ok(ext) if ext.reads_user_area() => {}
        Ok(_) => return Err(Error::Card("not on the user area")),
        Err(_) => return Err(Error::Card("EXT_CSD")),
    }
    let rca = RCA_GUESSES
        .into_iter()
        .find(|rca| card.read_status(*rca).is_ok_and(|s| mmc::card_state(s) == 4))
        .ok_or(Error::Card("no card in transfer state"))?;
    // SAFETY: see `Buffer`: this is the only reference while `f` runs.
    let buffer: &mut [u8] = unsafe { &mut *DEVICE.0.get() };
    let device = PartitionDevice::new(card, rca, PARTITION, buffer)?;
    if !device.probe()? {
        return Err(Error::NoFat);
    }
    let volumes = VolumeManager::new(device, FixedTime::default());
    let result = f(&volumes);
    volumes.device(|device| device.flush())?;
    result
}

fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |mut crc, &byte| {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
        crc
    })
}

/// Reads the saved application into `stage` (at most `stage.len()` bytes) and checks its CRC.
/// Returns its length.
pub fn load(stage: &mut [u8]) -> Result<usize, Error> {
    with_volumes(|volumes| {
        let volume = volumes.open_volume(VolumeIdx(0))?;
        let root = volume.open_root_dir()?;
        let dir = root.open_dir(DIR)?;
        let file = dir.open_file_in_dir(FILE, Mode::ReadOnly)?;
        let mut header = [0u8; HEADER_BYTES];
        if file.read(&mut header)? != HEADER_BYTES || &header[..8] != HEADER_MAGIC {
            return Err(Error::BadFile);
        }
        let len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        let want = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        if len > stage.len() || file.length() as usize != HEADER_BYTES + len {
            return Err(Error::BadFile);
        }
        let mut done = 0;
        while done < len {
            let n = file.read(&mut stage[done..len])?;
            if n == 0 {
                return Err(Error::BadFile);
            }
            done += n;
        }
        file.close()?;
        let got = crc32(&stage[..len]);
        if got != want {
            return Err(Error::Crc { want, got });
        }
        Ok(len)
    })
}

/// Saves `image`, whose CRC is `crc`, as the application to boot, making `FERRITE` if needed.
pub fn save(image: &[u8], crc: u32) -> Result<(), Error> {
    with_volumes(|volumes| {
        let volume = volumes.open_volume(VolumeIdx(0))?;
        let root = volume.open_root_dir()?;
        match root.make_dir_in_dir(DIR) {
            Ok(()) | Err(FsError::DirAlreadyExists) => {}
            Err(error) => return Err(error.into()),
        }
        let dir = root.open_dir(DIR)?;
        let file = dir.open_file_in_dir(FILE, Mode::ReadWriteCreateOrTruncate)?;
        let mut header = [0u8; HEADER_BYTES];
        header[..8].copy_from_slice(HEADER_MAGIC);
        header[8..12].copy_from_slice(&(image.len() as u32).to_le_bytes());
        header[12..16].copy_from_slice(&crc.to_le_bytes());
        file.write(&header)?;
        file.write(image)?;
        file.close()?;
        Ok(())
    })
}
