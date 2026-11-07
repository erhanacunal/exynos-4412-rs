//! USB host test: a flash drive on the micro-USB port, through an OTG adapter, with
//! `exynos_4412_rs::drivers::ehci` and `exynos_4412_rs::usb::host`.
//!
//! Waits for the adapter (the ID pin `gpx1-7` low), then turns VBUS on (`gpx0-6`) and the EHCI,
//! and waits for a device. Enumerates it and prints what it is. A mass storage device is opened:
//! INQUIRY, capacity, the partition table, then a timed read of the first 32 MiB, in 64 KiB and
//! in 1 MiB requests. Then the first partition's FAT file system, through
//! `exynos_4412_rs::fat::usb`: the root directory, with long names, and a timed read of its
//! largest file with its CRC-32 (compare with `crc32 FILE` on a PC). Nothing is written unless asked: `w` writes 1 MiB at the end of the drive,
//! reads it back, and puts back what was there.
//!
//! VBUS goes off as soon as the adapter is out, so the board never drives 5 V into a PC. `v`
//! turns it on without the adapter (for an adapter whose ID pin is not grounded): only with
//! nothing but a device on the port.
//!
//! UART: `w` write test (with a drive open), `v` VBUS on regardless, `r` restart, `f` restart
//! into fastboot.

#![no_std]
#![no_main]

use core::cell::{RefCell, UnsafeCell};
use core::ops::ControlFlow;

use exynos_4412_rs::drivers::ehci::{self, Ehci};
use exynos_4412_rs::drivers::gpio::{GPM0, GPX0, GPX1, Pin, Pull};
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::fat::embedded_sdmmc::{LfnBuffer, Mode, ShortFileName, VolumeIdx, VolumeManager};
use exynos_4412_rs::fat::usb::{self as fat_usb, UsbDevice};
use exynos_4412_rs::fat::FixedTime;
use exynos_4412_rs::partition::{self, BlockRead, Partition};
use exynos_4412_rs::usb::host::{self, Device};
use exynos_4412_rs::usb::host::msc::{self, Disk};
use exynos_4412_rs::{BootArgs, console, mmu, println, sleep, soc};

exynos_4412_rs::entry!(main);

/// `vdd_uotg_3v3` and `vdd_uotg_1v0`, as in `usb_msc_test`.
const LDOS: [(u8, u16); 2] = [(12, 3300), (15, 1000)];
const BUFFER_BYTES: usize = 1024 * 1024;
/// The FAT device's read-ahead: four windows of 64 KiB.
const WINDOW_BYTES: usize = 256 * 1024;
/// Root directory entries listed at most.
const LIST_MAX: usize = 64;
const SPEED_TEST_BYTES: u32 = 32 * 1024 * 1024;
/// Vendor `mach-mehmet.c`: the OTG adapter's ID pin (low with one), the VBUS boost, and
/// `VDD_5V0_EN` (`GPIO_USB_BOOT_EN`), which the vendor kernel sets at start.
const ID: Pin = GPX1.pin(7);
const VBUS: Pin = GPX0.pin(6);
const VDD_5V0: Pin = GPM0.pin(0);
/// After VBUS comes, devices get 100 ms to power up (USB 2.0, 7.1.7.3) and more in practice.
const POWER_UP_US: u64 = 200_000;

#[repr(C, align(32))]
struct Buffer<const N: usize>(UnsafeCell<[u8; N]>);

// SAFETY: `main` takes the one reference to each, once.
unsafe impl<const N: usize> Sync for Buffer<N> {}

static DATA: Buffer<BUFFER_BYTES> = Buffer(UnsafeCell::new([0; BUFFER_BYTES]));
static SAVED: Buffer<BUFFER_BYTES> = Buffer(UnsafeCell::new([0; BUFFER_BYTES]));
static WINDOW: Buffer<WINDOW_BYTES> = Buffer(UnsafeCell::new([0; WINDOW_BYTES]));

fn ms(mct: Mct, since: u64) -> u64 {
    (mct.count() - since) / u64::from(soc::MCT_CLOCK_HZ / 1000)
}

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?").trim_end()
}

/// `partition::scan` reads through `&self`.
struct Reader<'a, 'b>(&'a RefCell<&'a mut Disk<'b>>);

impl BlockRead for Reader<'_, '_> {
    type Error = msc::Error;

    fn read_block(&self, lba: u32, buffer: &mut [u8; 512]) -> Result<(), msc::Error> {
        self.0.borrow_mut().read(lba, buffer)
    }
}

fn set_vbus(on: bool) {
    VBUS.set_output(on);
}

fn print_device(ehci: &mut Ehci, device: &Device) {
    println!(
        "Device {:04x}:{:04x}, USB {:x}.{:02x}, class {:02x}, config {} of {} interfaces, first {:02x?}",
        device.vendor(),
        device.product(),
        device.usb_version() >> 8,
        device.usb_version() & 0xFF,
        device.class(),
        device.configuration,
        device.interfaces,
        device.first_class
    );
    let names = ["Manufacturer", "Product", "Serial"];
    for (name, index) in names.iter().zip(device.strings()) {
        let mut out = [0u8; 64];
        match host::string(ehci, index, &mut out) {
            Ok(0) => {}
            Ok(len) => println!("  {}: {}", name, text(&out[..len])),
            Err(error) => println!("  {}: ERROR {:?}", name, error),
        }
    }
}

fn speed_test(disk: &mut Disk<'_>, data: &mut [u8], mct: Mct) {
    let total = SPEED_TEST_BYTES.min(disk.blocks().saturating_mul(512) / BUFFER_BYTES as u32 * BUFFER_BYTES as u32);
    for chunk in [64 * 1024, BUFFER_BYTES] {
        let begin = mct.count();
        let mut lba = 0;
        let mut done = 0u32;
        while done < total {
            if let Err(error) = disk.read(lba, &mut data[..chunk]) {
                println!("Read at {} ERROR {:?}", lba, error);
                return;
            }
            lba += (chunk / 512) as u32;
            done += chunk as u32;
        }
        let took = ms(mct, begin).max(1);
        println!(
            "Read {} MiB in {} KiB requests: {} ms, {} KiB/s",
            total >> 20,
            chunk / 1024,
            took,
            u64::from(total) / 1024 * 1000 / took
        );
    }
}

/// CRC-32 (IEEE 802.3, reflected, as `crc32` and zip use), a table made at compile time.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// Continues a CRC-32: start with `!0`, end with `!crc`.
fn crc32(mut crc: u32, data: &[u8]) -> u32 {
    for byte in data {
        crc = CRC_TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

type FatError = exynos_4412_rs::fat::embedded_sdmmc::Error<fat_usb::Error>;

/// The first partition's FAT: the root directory, then the largest file in it read whole.
fn fat_test(disk: &mut Disk<'_>, window: &mut [u8], data: &mut [u8], mct: Mct) -> Result<(), FatError> {
    let device = UsbDevice::new(disk, window).map_err(FatError::DeviceError)?;
    let volumes: VolumeManager<_, _, 4, 4, 1> = VolumeManager::new(device, FixedTime::default());
    let begin = mct.count();
    let volume = volumes.open_volume(VolumeIdx(0))?;
    println!("FAT: volume open in {} ms", ms(mct, begin));
    let root = volume.open_root_dir()?;
    let mut name = [0u8; 256];
    let mut lfn = LfnBuffer::new(&mut name);
    let (mut count, mut bytes) = (0usize, 0u64);
    let mut largest: Option<(ShortFileName, u32)> = None;
    root.iterate_dir_lfn(&mut lfn, |entry, long| {
        if entry.attributes.is_volume() {
            return ControlFlow::Continue(());
        }
        count += 1;
        if entry.attributes.is_directory() {
            if count <= LIST_MAX {
                match long {
                    Some(long) => println!("  {:>10} {} {}/", "<DIR>", entry.mtime, long),
                    None => println!("  {:>10} {} {}/", "<DIR>", entry.mtime, entry.name),
                }
            }
            return ControlFlow::Continue(());
        }
        bytes += u64::from(entry.size);
        if count <= LIST_MAX {
            match long {
                Some(long) => println!("  {:>10} {} {}", entry.size, entry.mtime, long),
                None => println!("  {:>10} {} {}", entry.size, entry.mtime, entry.name),
            }
        }
        if largest.as_ref().is_none_or(|(_, size)| entry.size > *size) {
            largest = Some((entry.name, entry.size));
        }
        ControlFlow::Continue(())
    })?;
    let more = count.saturating_sub(LIST_MAX);
    println!("Root: {} entries ({} not shown), {} KiB in files", count, more, bytes / 1024);
    let listed = volumes.device(|device| device.stats());
    println!("  {:?}", listed);

    let Some((name, size)) = largest else {
        println!("No file to read");
        return Ok(());
    };
    let file = root.open_file_in_dir(name, Mode::ReadOnly)?;
    let begin = mct.count();
    let mut crc = !0u32;
    let mut done = 0u64;
    loop {
        let read = file.read(&mut data[..])?;
        if read == 0 {
            break;
        }
        crc = crc32(crc, &data[..read]);
        done += read as u64;
    }
    let took = ms(mct, begin).max(1);
    let stats = volumes.device(|device| device.stats());
    println!(
        "{}: {} of {} bytes in {} ms, {} KiB/s, CRC-32 {:08x}",
        name,
        done,
        size,
        took,
        done / 1024 * 1000 / took,
        !crc
    );
    println!(
        "  {} reads, {} blocks, {} from windows",
        stats.disk_reads - listed.disk_reads,
        stats.blocks_read - listed.blocks_read,
        stats.window_reads - listed.window_reads
    );
    file.close()?;
    root.close()?;
    volume.close()?;
    Ok(())
}

/// MBR type of a GPT's protective entry: the backup GPT is in the drive's last blocks.
const TYPE_GPT: u8 = 0xEE;

/// 1 MiB at the end of the drive: saved, written with a pattern, read back, put back. Only
/// where no partition or partition table is: refused on a GPT drive, or if a partition reaches
/// that far.
fn write_test(disk: &mut Disk<'_>, table: &[Partition], data: &mut [u8], saved: &mut [u8], mct: Mct) {
    let blocks = (BUFFER_BYTES / 512) as u32;
    let Some(lba) = disk.blocks().checked_sub(blocks) else {
        println!("Drive too small");
        return;
    };
    if table.iter().any(|p| p.kind == TYPE_GPT) {
        println!("GPT drive: its backup table is in the last blocks; write test refused");
        return;
    }
    if let Some(p) = table.iter().find(|p| u64::from(p.start) + u64::from(p.count) > u64::from(lba)) {
        println!("p{} reaches block {}: write test refused", p.number, lba);
        return;
    }
    let pattern = |i: usize| (i.wrapping_mul(0x9E37_79B1) >> 13) as u8 ^ (i >> 9) as u8;
    let step = |what: &str, result: Result<(), msc::Error>| match result {
        Ok(()) => true,
        Err(error) => {
            println!("{} ERROR {:?}", what, error);
            false
        }
    };
    println!("Write test at blocks {}..{}", lba, lba + blocks);
    if !step("Save", disk.read(lba, saved)) {
        return;
    }
    data.iter_mut().enumerate().for_each(|(i, byte)| *byte = pattern(i));
    let begin = mct.count();
    let wrote = step("Write", disk.write(lba, data)) && step("Flush", disk.flush());
    let took = ms(mct, begin).max(1);
    if wrote {
        data.fill(0);
        if step("Read back", disk.read(lba, data)) {
            let bad = data.iter().enumerate().filter(|(i, byte)| **byte != pattern(*i)).count();
            println!(
                "Wrote 1 MiB in {} ms ({} KiB/s); read back: {} bytes differ{}",
                took,
                1024 * 1000 / took,
                bad,
                if disk.no_cache() { " (no SYNCHRONIZE CACHE)" } else { "" }
            );
        }
    }
    if step("Restore", disk.write(lba, saved)) && step("Flush", disk.flush()) && step("Check", disk.read(lba, data)) {
        println!("Restored: {}", if data == saved { "same as before" } else { "MISMATCH" });
    }
}

/// Serves an opened disk until the device goes away or the adapter is out.
fn use_disk(disk: &mut Disk<'_>, window: &mut [u8], data: &mut [u8], mct: Mct) -> ([Partition; 16], usize) {
    let (vendor, product, revision) = disk.identity();
    println!(
        "Disk: '{}' '{}' '{}', removable {}, {} LUNs, {} blocks of {} ({} MiB)",
        text(vendor),
        text(product),
        text(revision),
        disk.removable(),
        disk.luns(),
        disk.blocks(),
        disk.block_size(),
        (u64::from(disk.blocks()) * u64::from(disk.block_size())) >> 20
    );
    let mut table = [Partition::default(); 16];
    let found = {
        let cell = RefCell::new(&mut *disk);
        match partition::scan(&Reader(&cell), &mut table) {
            Ok(0) => {
                println!("No partition table");
                0
            }
            Ok(found) => {
                for p in &table[..found] {
                    let gpt = if p.kind == TYPE_GPT { " (GPT, not read)" } else { "" };
                    println!("  p{}: type {:#04x}{}, start {}, {} blocks", p.number, p.kind, gpt, p.start, p.count);
                }
                found
            }
            Err(error) => {
                // Unknown layout: claim the whole drive, so nothing is written.
                println!("Partition table ERROR {:?}", error);
                table[0] = Partition { number: 0, kind: 0, start: 0, count: u32::MAX, record: 0 };
                1
            }
        }
    };
    speed_test(disk, data, mct);
    if let Err(error) = fat_test(disk, window, data, mct) {
        println!("FAT ERROR {:?}", error);
    }
    println!("Ready. UART: w write test, r restart, f fastboot. Unplug to end.");
    (table, found)
}

fn wait_adapter(mct: Mct, keys: &mut PowerKeys) {
    let uart = console::uart();
    println!("Waiting for the OTG adapter (ID low), or 'v' to force VBUS on");
    loop {
        if !ID.read() {
            return;
        }
        keys.service(mct);
        match uart.try_read_byte() {
            Some(b'v') => {
                println!("VBUS forced on");
                return;
            }
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
        sleep::for_us(mct, 20_000);
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nUSB host test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let i2c = I2c::new(Bus::I2c0);
    match i2c.init(400_000, 100) {
        Ok(_) => {
            let pmic = Max77686::new(i2c);
            for (number, millivolts) in LDOS {
                match pmic.ldo(number) {
                    Ok(ldo) if ldo.mode() == 3 => println!("LDO{}: on, {} mV", number, ldo.millivolts()),
                    Ok(_) => println!("LDO{}: turned on {:?}", number, pmic.enable_ldo(number, millivolts)),
                    Err(error) => println!("LDO{} ERROR {:?}", number, error),
                }
            }
        }
        Err(error) => println!("I2C ERROR {:?}: USB supplies not checked", error),
    }

    println!(
        "Pins: VDD_5V0_EN gpm0-0 {:?} {}, VBUS gpx0-6 {:?} {}, ID gpx1-7 {:?} {}",
        VDD_5V0.function(),
        VDD_5V0.read(),
        VBUS.function(),
        VBUS.read(),
        ID.function(),
        ID.read()
    );
    VDD_5V0.set_output(true);
    set_vbus(false);
    ID.set_input(Pull::None);

    // SAFETY: the one reference to each buffer, taken once, here.
    let data = unsafe { &mut *DATA.0.get() };
    // SAFETY: as above.
    let saved = unsafe { &mut *SAVED.0.get() };
    // SAFETY: as above.
    let window = unsafe { &mut *WINDOW.0.get() };
    let mut keys = PowerKeys::new();
    let uart = console::uart();
    let mut controller: Option<Ehci> = None;

    loop {
        wait_adapter(mct, &mut keys);
        set_vbus(true);
        println!("VBUS on; charger pins (DC, USB) {:?}", power::charger_connected());
        sleep::for_us(mct, POWER_UP_US);
        if controller.is_none() {
            match Ehci::new(mct) {
                Ok(ehci) => {
                    let info = ehci.info();
                    println!(
                        "EHCI {:#010x}, hcsparams {:#010x}, hccparams {:#010x}, {} ports",
                        info.version,
                        info.hcsparams,
                        info.hccparams,
                        ehci.ports()
                    );
                    controller = Some(ehci);
                }
                Err(error) => {
                    println!("EHCI ERROR {:?}", error);
                    set_vbus(false);
                    loop {
                        keys.service(mct);
                        sleep::for_us(mct, 20_000);
                    }
                }
            }
        }
        let Some(ehci) = controller.as_mut() else { continue };
        println!("Port 0: {:#010x}; waiting for a device", ehci.port_status(ehci::PORT).raw);

        // Until the adapter goes: a device comes, is used, goes.
        let forced = ID.read();
        while forced || !ID.read() {
            keys.service(mct);
            if !ehci.port_status(ehci::PORT).connected() {
                ehci.wait_port(ehci::PORT, Some(mct.count() + u64::from(soc::MCT_CLOCK_HZ / 10)));
                if uart.try_read_byte() == Some(b'r') {
                    power::restart(BootMode::Normal);
                }
                continue;
            }
            let begin = mct.count();
            println!("Connected: port {:#010x}", ehci.port_status(ehci::PORT).raw);
            match host::enumerate(ehci) {
                Ok(device) => {
                    println!("Enumerated in {} ms, speed {:?}", ms(mct, begin), device.speed);
                    print_device(ehci, &device);
                    match device.mass_storage {
                        Some(storage) => {
                            println!(
                                "Mass storage: interface {}, subclass {}, IN {:#04x}/{} OUT {:#04x}/{}",
                                storage.interface,
                                storage.subclass,
                                storage.input.address,
                                storage.input.packet,
                                storage.output.address,
                                storage.output.packet
                            );
                            match Disk::open(ehci, storage, mct) {
                                Ok(mut disk) => {
                                    let (table, found) = use_disk(&mut disk, window, data, mct);
                                    loop {
                                        keys.service(mct);
                                        match uart.try_read_byte() {
                                            Some(b'w') => write_test(&mut disk, &table[..found], data, saved, mct),
                                            Some(b'r') => power::restart(BootMode::Normal),
                                            Some(b'f') => power::restart(BootMode::Fastboot),
                                            _ => {}
                                        }
                                        if !disk.connected() || (!forced && ID.read()) {
                                            break;
                                        }
                                        sleep::for_us(mct, 20_000);
                                    }
                                }
                                Err(error) => println!("Disk ERROR {:?}", error),
                            }
                        }
                        None => println!("Not a Bulk-Only mass storage device"),
                    }
                }
                Err(error) => println!("Enumeration ERROR {:?}", error),
            }
            // Wait for it to go before looking again.
            while ehci.port_status(ehci::PORT).connected() && (forced || !ID.read()) {
                keys.service(mct);
                ehci.wait_port(ehci::PORT, Some(mct.count() + u64::from(soc::MCT_CLOCK_HZ / 10)));
            }
            ehci.clear_port_changes(ehci::PORT);
            println!("Device gone");
        }
        set_vbus(false);
        println!("Adapter out: VBUS off");
    }
}
