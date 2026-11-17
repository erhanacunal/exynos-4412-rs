//! The saved application: `FERRITE/FERRITE.BIN`; the application's
//! data file next to it, `FERRITE/FLASH.BIN`, written as sent ([`save_flash`]); and the board's
//! own settings in `SYSTEM/` ([`SYSTEM_DIR`]): `WIFI.TXT`, the WiFi chip's firmware and NVRAM
//! (`net`), read by this loader and by the application alike, written by [`save_system`].
//!
//! The file is the UART's wire header followed by the image, so the CRC it was checked against is
//! kept with it:
//!
//! | Bytes | Content |
//! |---|---|
//! | 8 | `EXYNBOOT` |
//! | 4 | length, little-endian |
//! | 4 | CRC-32 (IEEE, as zlib's) of the image, little-endian |
//! | length | the image |
//!
//! Where: USERDATA (p10) when ferrite-ui-xl has formatted it as ext4 ([`crate::userdata`]),
//! otherwise CACHE (p8, FAT32), as before the move: every function here tries USERDATA first and
//! uses CACHE only when USERDATA holds no ext4. [`medium`] says which one it was.
//!
//! Runs with the MMU and caches off before WiFi, where memory is Strongly-ordered, and with them
//! on after (`net`), where `fat::PartitionDevice` keeps the IDMAC's buffer coherent itself. CACHE
//! is never formatted here: without a FAT32 on it, there is nothing to boot and nothing is saved.

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::mmc::{self, Mmc};
use exynos_4412_rs::fat::embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};
use exynos_4412_rs::fat::{self, FixedTime, PartitionDevice};

use crate::crc::crc32;
use crate::userdata;

/// CACHE.
const PARTITION: u32 = 8;
pub const DIR: &str = "FERRITE";
pub const FILE: &str = "FERRITE.BIN";
/// The application's data, which it reads itself: saved as it came, with no header.
pub const FLASH_FILE: &str = "FLASH.BIN";
/// Present: test mode, the saved application is not booted (see [`crate::stay`]). Empty.
pub const STAY_FILE: &str = "STAY";
pub const HEADER_MAGIC: &[u8; 8] = b"EXYNBOOT";
/// The board's settings, shared with the application: see `net`.
pub const SYSTEM_DIR: &str = "SYSTEM";
const HEADER_BYTES: usize = 16;
/// Read-ahead and single blocks, plus a 32 KiB run for writes: this loader writes one file.
const DEVICE_BYTES: usize = fat::MIN_BUFFER - 512 + 32 * 1024;
/// Guesses at the address U-Boot gave the card, which it does not report (as `fat_test`).
const RCA_GUESSES: [u16; 2] = [1, 2];

type FsError = fat::embedded_sdmmc::Error<fat::Error>;
type Volumes<'a> = VolumeManager<PartitionDevice<'a>, FixedTime, 4, 4, 1>;

#[repr(C, align(32))]
struct Buffer(UnsafeCell<[u8; DEVICE_BYTES]>);

// SAFETY: one user at a time: `with_volumes` takes it for the length of one call, which never
// awaits, and this program runs on one core; no interrupt handler touches it.
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
    /// USERDATA's ext4 failed or refused.
    Ext4(exynos_4412_ext4::ext4_rs::Error),
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

/// Whether the last call found USERDATA's ext4 (else it used CACHE).
static mut ON_USERDATA: bool = false;

/// Where the files are, for the log: `"USERDATA"` or `"CACHE"`, as the last call found.
pub fn medium() -> &'static str {
    if unsafe { ON_USERDATA } { "USERDATA" } else { "CACHE" }
}

/// Runs `on_ext4` on USERDATA's ext4 if there is one, else `on_fat` on CACHE; `with` goes to
/// whichever runs.
fn with_storage<C, T>(
    with: C,
    on_ext4: impl FnOnce(&mut userdata::Fs, C) -> Result<T, Error>,
    on_fat: impl FnOnce(&Volumes<'_>, C) -> Result<T, Error>,
) -> Result<T, Error> {
    let (card, rca) = open_card()?;
    match userdata::mount(card, rca) {
        Some(mounted) => {
            unsafe { ON_USERDATA = true };
            let mut fs = mounted?;
            let result = on_ext4(&mut fs, with);
            userdata::finish(fs, result)
        }
        None => {
            unsafe { ON_USERDATA = false };
            with_volumes(|volumes| on_fat(volumes, with))
        }
    }
}

/// The eMMC, on its user area, and the card's address.
fn open_card() -> Result<(Mmc, u16), Error> {
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
    Ok((card, rca))
}

/// Opens CACHE and runs `f` on its volume manager, then writes the device's cache to the card.
fn with_volumes<T>(f: impl FnOnce(&Volumes<'_>) -> Result<T, Error>) -> Result<T, Error> {
    let (card, rca) = open_card()?;
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

/// Reads the saved application into `stage` (at most `stage.len()` bytes) and checks its CRC.
/// Returns its length.
pub fn load(stage: &mut [u8]) -> Result<usize, Error> {
    let len = with_storage(&mut *stage, |fs, stage| load_ext4(fs, stage), |volumes, stage| load_fat(volumes, stage))?;
    let header = unsafe { HEADER };
    let want = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    let got = crc32(&stage[..len]);
    if got != want {
        return Err(Error::Crc { want, got });
    }
    Ok(len)
}

/// The header of the file `load` read last.
static mut HEADER: [u8; HEADER_BYTES] = [0; HEADER_BYTES];

/// The image's length from a header, if it is one.
fn image_len(header: &[u8; HEADER_BYTES]) -> Result<usize, Error> {
    if &header[..8] != HEADER_MAGIC {
        return Err(Error::BadFile);
    }
    unsafe { HEADER = *header };
    Ok(u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize)
}

fn load_ext4(fs: &mut userdata::Fs, stage: &mut [u8]) -> Result<usize, Error> {
    let mut header = [0u8; HEADER_BYTES];
    let size = userdata::read_exact_at(fs, DIR, FILE, 0, &mut header)?;
    let len = image_len(&header)?;
    if len > stage.len() || size != (HEADER_BYTES + len) as u64 {
        return Err(Error::BadFile);
    }
    userdata::read_exact_at(fs, DIR, FILE, HEADER_BYTES as u64, &mut stage[..len])?;
    Ok(len)
}

fn load_fat(volumes: &Volumes<'_>, stage: &mut [u8]) -> Result<usize, Error> {
    {
        let volume = volumes.open_volume(VolumeIdx(0))?;
        let root = volume.open_root_dir()?;
        let dir = root.open_dir(DIR)?;
        let file = dir.open_file_in_dir(FILE, Mode::ReadOnly)?;
        let mut header = [0u8; HEADER_BYTES];
        if file.read(&mut header)? != HEADER_BYTES {
            return Err(Error::BadFile);
        }
        let len = image_len(&header)?;
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
        Ok(len)
    }
}

/// Whether [`STAY_FILE`] is there. A CACHE without `FERRITE`, or without FAT32, says no.
pub fn stay() -> Result<bool, Error> {
    let found = with_storage((), |fs, ()| if userdata::exists(fs, DIR, STAY_FILE)? { Ok(()) } else { Err(Error::NotFound) }, |volumes, ()| {
        let volume = volumes.open_volume(VolumeIdx(0))?;
        let root = volume.open_root_dir()?;
        let dir = root.open_dir(DIR)?;
        dir.find_directory_entry(STAY_FILE)?;
        Ok(())
    });
    match found {
        Ok(()) => Ok(true),
        Err(Error::NotFound | Error::NoFat) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Creates [`STAY_FILE`] (`on`), making `FERRITE` if needed, or deletes it.
pub fn set_stay(on: bool) -> Result<(), Error> {
    with_storage((), |fs, ()| userdata::set_present(fs, DIR, STAY_FILE, on), |volumes, ()| {
        let volume = volumes.open_volume(VolumeIdx(0))?;
        let root = volume.open_root_dir()?;
        if on {
            match root.make_dir_in_dir(DIR) {
                Ok(()) | Err(FsError::DirAlreadyExists) => {}
                Err(error) => return Err(error.into()),
            }
            let dir = root.open_dir(DIR)?;
            dir.open_file_in_dir(STAY_FILE, Mode::ReadWriteCreateOrTruncate)?.close()?;
            return Ok(());
        }
        let dir = match root.open_dir(DIR) {
            Ok(dir) => dir,
            Err(FsError::NotFound) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        match dir.delete_entry_in_dir(STAY_FILE) {
            Ok(()) | Err(FsError::NotFound) => Ok(()),
            Err(error) => Err(error.into()),
        }
    })
}

/// Reads `<dir>/<name>` whole into the start of `buf`; its length. [`Error::BadFile`] when it is
/// longer than `buf`.
pub fn read_file(dir: &str, name: &str, buf: &mut [u8]) -> Result<usize, Error> {
    with_storage(buf, |fs, buf| userdata::read_file(fs, dir, name, buf), |volumes, buf| {
        let volume = volumes.open_volume(VolumeIdx(0))?;
        let root = volume.open_root_dir()?;
        let dir = root.open_dir(dir)?;
        let file = dir.open_file_in_dir(name, Mode::ReadOnly)?;
        let len = file.length() as usize;
        if len > buf.len() {
            return Err(Error::BadFile);
        }
        let mut done = 0;
        while done < len {
            let n = file.read(&mut buf[done..len])?;
            if n == 0 {
                return Err(Error::BadFile);
            }
            done += n;
        }
        file.close()?;
        Ok(len)
    })
}

/// Writes `parts`, one after the other, as `<dir>/<name>`, making `dir` if needed.
fn write_file(dir: &str, name: &str, parts: &[&[u8]]) -> Result<(), Error> {
    with_storage((), |fs, ()| userdata::write_file(fs, dir, name, parts), |volumes, ()| {
        let volume = volumes.open_volume(VolumeIdx(0))?;
        let root = volume.open_root_dir()?;
        match root.make_dir_in_dir(dir) {
            Ok(()) | Err(FsError::DirAlreadyExists) => {}
            Err(error) => return Err(error.into()),
        }
        let dir = root.open_dir(dir)?;
        let file = dir.open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)?;
        for part in parts {
            file.write(part)?;
        }
        file.close()?;
        Ok(())
    })
}

/// Saves `image`, whose CRC is `crc`, as the application to boot, making `FERRITE` if needed.
pub fn save(image: &[u8], crc: u32) -> Result<(), Error> {
    let mut header = [0u8; HEADER_BYTES];
    header[..8].copy_from_slice(HEADER_MAGIC);
    header[8..12].copy_from_slice(&(image.len() as u32).to_le_bytes());
    header[12..16].copy_from_slice(&crc.to_le_bytes());
    write_file(DIR, FILE, &[&header, image])
}

/// Saves `data` as [`FLASH_FILE`], as it is: the caller has checked its CRC.
pub fn save_flash(data: &[u8]) -> Result<(), Error> {
    write_file(DIR, FLASH_FILE, &[data])
}

/// Saves `data` as `SYSTEM/<name>` (an 8.3 name, checked by the caller), making `SYSTEM` if needed.
pub fn save_system(name: &str, data: &[u8]) -> Result<(), Error> {
    write_file(SYSTEM_DIR, name, &[data])
}
