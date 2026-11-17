//! USERDATA (p10) as ferrite-ui-xl formats it: ext4 with a journal (`exynos_4412_rs::ext4`), the
//! same tree as on CACHE but in lower case (`ferrite/ferrite.bin`, `ferrite/flash.bin`,
//! `ferrite/stay`, `system/…`; ferrite-ui-xl `storage/ext4.rs`, keep in sync): callers give the
//! CACHE names, [`path`] lowers them. [`cache`](crate::cache) comes here first and falls back to CACHE's FAT32 only
//! when USERDATA holds no ext4 at all.
//!
//! Every call mounts (replaying the journal if a power loss left a transaction), works, and
//! unmounts. Files are replaced whole: written under a temporary name, then renamed over the old
//! one, so a power loss leaves the old file or the new one, never a torn one.

use alloc::format;
use alloc::string::String;
use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::mmc::Mmc;
use exynos_4412_rs::ext4::PartitionBlocks;
use exynos_4412_rs::ext4::ext4_rs::{self, BlockDevice, Ext4, MountOptions};

use crate::cache::Error;

/// USERDATA.
const PARTITION: u32 = 10;
const BOUNCE_BYTES: usize = 64 * 1024;
/// Where the superblock's magic is in block 0, and what it holds for ext2/3/4.
const MAGIC_AT: usize = 1024 + 0x38;
const MAGIC: u16 = 0xEF53;
/// Small: this loader reads a few files per call, and the arena is reset after each.
const OPTIONS: MountOptions = MountOptions { cache_blocks: 64, commit_blocks: 256 };

pub type Fs = Ext4<PartitionBlocks<'static>>;

#[repr(C, align(32))]
struct Bounce(UnsafeCell<[u8; BOUNCE_BYTES]>);

// SAFETY: one user at a time, as `cache::DEVICE`.
unsafe impl Sync for Bounce {}

static BOUNCE: Bounce = Bounce(UnsafeCell::new([0; BOUNCE_BYTES]));

impl From<ext4_rs::Error> for Error {
    fn from(error: ext4_rs::Error) -> Self {
        match error {
            ext4_rs::Error::NotFound => Self::NotFound,
            other => Self::Ext4(other),
        }
    }
}

/// Mounts USERDATA. `None`: USERDATA holds no ext4 of ours (none, one with features `ext4_rs`
/// does not handle such as a Linux rootfs, or there is no partition 10), so CACHE is the place;
/// ours failing to mount is an error, not a reason to fall back. [`finish`] unmounts.
pub fn mount(card: Mmc, rca: u16) -> Option<Result<Fs, Error>> {
    // SAFETY: see `Bounce`: this is the only reference while `f` runs.
    let bounce: &'static mut [u8] = unsafe { &mut *BOUNCE.0.get() };
    let mut device = PartitionBlocks::new(card, rca, PARTITION, bounce, || 0).ok()?;
    let mut block = [0u8; ext4_rs::BLOCK_SIZE];
    if device.read(0, &mut block).is_err() {
        return Some(Err(Error::Card("USERDATA read")));
    }
    if u16::from_le_bytes([block[MAGIC_AT], block[MAGIC_AT + 1]]) != MAGIC {
        return None;
    }
    match Ext4::mount(device, OPTIONS) {
        Ok(fs) => Some(Ok(fs)),
        Err(ext4_rs::Error::Unsupported(_)) => None,
        Err(error) => Some(Err(Error::Ext4(error))),
    }
}

/// Unmounts after a call whose result was `result`; the first failure wins.
pub fn finish<T>(fs: Fs, result: Result<T, Error>) -> Result<T, Error> {
    let unmounted = fs.unmount();
    match (result, unmounted) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(Error::Ext4(error)),
        (Ok(value), Ok(_)) => Ok(value),
    }
}

/// The on-disk path of `dir`/`name`: lower case.
fn path(dir: &str, name: &str) -> String {
    format!("/{}/{}", dir, name).to_ascii_lowercase()
}

/// Reads `<dir>/<name>` whole into the start of `buf`; its length. [`Error::BadFile`] when it is
/// longer than `buf`.
pub fn read_file(fs: &mut Fs, dir: &str, name: &str, buf: &mut [u8]) -> Result<usize, Error> {
    let ino = fs.lookup(&path(dir, name))?.ok_or(Error::NotFound)?;
    let len = fs.metadata(ino)?.size;
    if len > buf.len() as u64 {
        return Err(Error::BadFile);
    }
    let len = len as usize;
    if fs.read_at(ino, 0, &mut buf[..len])? != len {
        return Err(Error::BadFile);
    }
    Ok(len)
}

/// Reads `<dir>/<name>` from `offset`, into all of `buf`.
pub fn read_exact_at(fs: &mut Fs, dir: &str, name: &str, offset: u64, buf: &mut [u8]) -> Result<u64, Error> {
    let ino = fs.lookup(&path(dir, name))?.ok_or(Error::NotFound)?;
    let size = fs.metadata(ino)?.size;
    if fs.read_at(ino, offset, buf)? != buf.len() {
        return Err(Error::BadFile);
    }
    Ok(size)
}

pub fn exists(fs: &mut Fs, dir: &str, name: &str) -> Result<bool, Error> {
    match fs.lookup(&path(dir, name)) {
        Ok(found) => Ok(found.is_some()),
        Err(ext4_rs::Error::NotFound) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn ensure_dir(fs: &mut Fs, dir: &str) -> Result<(), Error> {
    match fs.mkdir(&format!("/{}", dir).to_ascii_lowercase()) {
        Ok(_) | Err(ext4_rs::Error::Exists) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Writes `parts`, one after the other, as `<dir>/<name>` (making `dir` if needed): under a
/// temporary name first, then renamed over the old file.
pub fn write_file(fs: &mut Fs, dir: &str, name: &str, parts: &[&[u8]]) -> Result<(), Error> {
    ensure_dir(fs, dir)?;
    let target = path(dir, name);
    let temporary = format!("{}.new", target);
    match fs.remove(&temporary) {
        Ok(()) | Err(ext4_rs::Error::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    let ino = fs.create(&temporary)?;
    let mut at = 0u64;
    for part in parts {
        if fs.write_at(ino, at, part)? != part.len() {
            return Err(Error::Ext4(ext4_rs::Error::NoSpace));
        }
        at += part.len() as u64;
    }
    fs.rename(&temporary, &target)?;
    fs.sync()?;
    Ok(())
}

/// Creates `<dir>/<name>` empty if it is not there (making `dir`), or deletes it.
pub fn set_present(fs: &mut Fs, dir: &str, name: &str, on: bool) -> Result<(), Error> {
    let present = exists(fs, dir, name)?;
    if on && !present {
        ensure_dir(fs, dir)?;
        fs.create(&path(dir, name))?;
    } else if !on && present {
        fs.remove(&path(dir, name))?;
    }
    fs.sync()?;
    Ok(())
}
