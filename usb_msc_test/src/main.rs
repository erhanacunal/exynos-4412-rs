//! USB mass storage test: the CACHE partition (p8) as a removable disk on the micro-USB port,
//! through `exynos_4412_rs::drivers::otg` and `exynos_4412_rs::usb::msc`.
//!
//! Reports the USB supplies, the PHY as U-Boot left it and what the OTG core says it is, then
//! pulls D+ up and serves the host. Prints the bus events and the first SETUP packets of each
//! enumeration, then every 2 s while the host reads or writes: commands, MiB moved and the
//! rate. Every write goes through an `MmcWriter` for CACHE, which refuses blocks outside it.
//!
//! UART: `s` statistics, `d` disconnect and connect again, `r` restart, `f` restart into
//! fastboot (both refused while a command runs). Keys: as in `power_test`.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::cmu::Cmu;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::mmc::{self, Mmc, MmcWriter};
use exynos_4412_rs::drivers::otg::{Event, Otg};
use exynos_4412_rs::drivers::power::{self, BootMode, PowerKeys};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::drivers::usb_phy;
use exynos_4412_rs::usb::msc::{self, MassStorage, Stats};
use exynos_4412_rs::{BootArgs, console, mmu, println, sleep, soc};

exynos_4412_rs::entry!(main);

/// The partition served: CACHE.
const TARGET: u32 = 8;
/// Guesses at the address U-Boot gave the card, which it does not report.
const RCA_GUESSES: [u16; 2] = [1, 2];
/// `vdd_uotg_3v3` and `vdd_uotg_1v0`: vendor `mach-mehmet.c:1296-1306`.
const LDO_UOTG_3V3: (u8, u16) = (12, 3300);
const LDO_UOTG_1V0: (u8, u16) = (15, 1000);
const BUFFER_BYTES: usize = 64 * 1024;
const REPORT_MS: u64 = 2000;
/// SETUP packets printed after each bus reset; the rest are counted.
const SETUPS_SHOWN: u32 = 24;

static STRINGS: msc::Strings = ["exynos_4412_rs", "Exynos 4412 CACHE", "441200000008"];

#[repr(C, align(32))]
struct Buffer(UnsafeCell<[u8; BUFFER_BYTES]>);

// SAFETY: `main` takes the one reference, once.
unsafe impl Sync for Buffer {}

static BUFFER: Buffer = Buffer(UnsafeCell::new([0; BUFFER_BYTES]));

fn ms(counts: u64) -> u64 {
    counts / u64::from(soc::MCT_CLOCK_HZ / 1000)
}

fn find_rca(card: Mmc) -> Option<u16> {
    RCA_GUESSES
        .into_iter()
        .find(|rca| card.read_status(*rca).is_ok_and(|status| mmc::card_state(status) == 4))
}

/// Turns a USB supply on if it is off, and prints it.
fn supply(pmic: &Max77686, (number, millivolts): (u8, u16)) {
    match pmic.ldo(number) {
        Ok(ldo) if ldo.mode() == 3 => println!("LDO{}: on, {} mV", number, ldo.millivolts()),
        Ok(ldo) => {
            print_result("LDO", number, pmic.enable_ldo(number, millivolts).is_ok());
            println!("  (was mode {}, {} mV)", ldo.mode(), ldo.millivolts());
        }
        Err(error) => println!("LDO{} ERROR {:?}", number, error),
    }
}

fn print_result(what: &str, number: u8, ok: bool) {
    println!("{}{}: turned on {}", what, number, if ok { "OK" } else { "FAILED" });
}

fn report(stats: &Stats, since: &Stats, counts: u64) {
    let read = stats.blocks_read - since.blocks_read;
    let written = stats.blocks_written - since.blocks_written;
    let kib_s = |blocks: u64| blocks * 512 * u64::from(soc::MCT_CLOCK_HZ) / 1024 / counts.max(1);
    println!(
        "cmd {} fail {} | read {} KiB {} KiB/s | write {} KiB {} KiB/s | total R {} W {} MiB",
        stats.commands,
        stats.failed,
        read / 2,
        kib_s(read),
        written / 2,
        kib_s(written),
        stats.blocks_read / 2048,
        stats.blocks_written / 2048,
    );
}

fn stop(mct: Mct) -> ! {
    println!("Stopped. Keys: as in power_test.");
    let mut keys = PowerKeys::new();
    loop {
        keys.service(mct);
        sleep::for_us(mct, 10_000);
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nUSB mass storage test");
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();

    let i2c = I2c::new(Bus::I2c0);
    match i2c.init(400_000, 100) {
        Ok(_) => {
            let pmic = Max77686::new(i2c);
            supply(&pmic, LDO_UOTG_3V3);
            supply(&pmic, LDO_UOTG_1V0);
        }
        Err(error) => println!("I2C ERROR {:?}: USB supplies not checked", error),
    }
    let (device_gate, host_gate) = Cmu::new().usb_gates_open();
    println!("Gates: usb_device {}, usb_host {}", device_gate, host_gate);
    let phy = usb_phy::state();
    println!(
        "PHY as left: pwr {:#06x} clk {:#06x} rst {:#06x} isol {:#x} mode {}",
        phy.power, phy.clock, phy.reset, phy.isolation, phy.mode
    );
    let (dc, usb) = power::charger_connected();
    println!("Charger pins: DC {}, USB (VBUS) {}", dc, usb);

    let card = match Mmc::new() {
        Ok(card) => card,
        Err(error) => {
            println!("eMMC ERROR {:x?}", error);
            stop(mct);
        }
    };
    let Some(rca) = find_rca(card) else {
        println!("No card in transfer state at {:?}", RCA_GUESSES);
        stop(mct);
    };
    let writer = match MmcWriter::new(card, rca, TARGET) {
        Ok(writer) => writer,
        Err(error) => {
            println!("CACHE ERROR {:x?}", error);
            stop(mct);
        }
    };
    let partition = writer.partition();
    println!(
        "CACHE: p{}, start {}, {} blocks ({} MiB)",
        partition.number,
        partition.start,
        partition.count,
        partition.count / 2048
    );

    let mut otg = match Otg::new(mct, msc::descriptors(&STRINGS)) {
        Ok(otg) => otg,
        Err(error) => {
            println!("OTG ERROR {:x?}", error);
            stop(mct);
        }
    };
    let info = otg.info();
    let phy = usb_phy::state();
    println!(
        "PHY now: pwr {:#06x} clk {:#06x} rst {:#06x} isol {:#x} mode {}",
        phy.power, phy.clock, phy.reset, phy.isolation, phy.mode
    );
    println!(
        "OTG core {:#010x}, hwcfg {:08x} {:08x} {:08x} {:08x}",
        info.snpsid, info.hwcfg[0], info.hwcfg[1], info.hwcfg[2], info.hwcfg[3]
    );
    println!(
        "  {} endpoints, dedicated FIFOs {}, FIFO RAM {} words, rx {} tx0 {} tx {}",
        info.endpoints, info.dedicated_fifos, info.fifo_words, info.fifos.0, info.fifos.1, info.fifos.2
    );

    // SAFETY: the one reference to BUFFER, taken once, here.
    let buffer = unsafe { &mut *BUFFER.0.get() };
    let mut storage = MassStorage::new(writer, buffer, "Exynos", "4412 CACHE");
    otg.connect();
    println!("Connected: D+ pulled up, VBUS {}", otg.vbus());

    let uart = console::uart();
    let uart_irq = uart.id().irq();
    uart.enable_rx_interrupt();
    let mut keys = PowerKeys::new();
    let mut shown = 0u32;
    let mut since = storage.stats();
    let mut last = mct.count();
    let period = REPORT_MS * u64::from(soc::MCT_CLOCK_HZ / 1000);
    let begin = mct.count();
    loop {
        while let Some(event) = otg.poll() {
            match event {
                Event::Reset => {
                    shown = 0;
                    println!("[{} ms] bus reset", ms(mct.count() - begin));
                }
                Event::Enumerated(speed) => println!("[{} ms] speed {:?}", ms(mct.count() - begin), speed),
                Event::Configured(value) => println!("[{} ms] configuration {}", ms(mct.count() - begin), value),
                Event::Suspend => println!("[{} ms] suspend", ms(mct.count() - begin)),
                Event::Resume => println!("[{} ms] resume", ms(mct.count() - begin)),
                Event::Disconnected => println!("[{} ms] VBUS gone", ms(mct.count() - begin)),
                Event::SessionRequest => println!("[{} ms] VBUS came", ms(mct.count() - begin)),
                Event::ClassRequest(setup) => println!("  class request {:02x} {:02x}", setup.request_type, setup.request),
                Event::HaltCleared(address) => println!("  halt cleared on {:#04x}", address),
                Event::InDone(_) | Event::OutDone(..) => {}
            }
            let lost = otg.take_setups(|setup| {
                if shown < SETUPS_SHOWN {
                    let b = setup.to_bytes();
                    println!(
                        "  SETUP {:02x} {:02x} {:02x}{:02x} {:02x}{:02x} {:02x}{:02x}",
                        b[0], b[1], b[3], b[2], b[5], b[4], b[7], b[6]
                    );
                }
                shown += 1;
            });
            if lost != 0 {
                println!("  ({} SETUPs not kept)", lost);
            }
            storage.handle(&mut otg, event);
        }

        let now = mct.count();
        if now - last >= period {
            let stats = storage.stats();
            if stats.commands != since.commands {
                report(&stats, &since, now - last);
                if stats.last_failure != since.last_failure
                    && let Some((opcode, sense)) = stats.last_failure
                {
                    println!("  last failure: opcode {:#04x}, sense {:x?}", opcode, sense);
                }
            }
            since = stats;
            last = now;
        }
        keys.service(mct);

        match uart.try_read_byte() {
            Some(b's') => {
                let stats = storage.stats();
                println!("{:?}, ejected {}, VBUS {}", stats, storage.ejected(), otg.vbus());
            }
            Some(b'd') => {
                otg.disconnect();
                println!("Disconnected for 1 s");
                sleep::for_us(mct, 1_000_000);
                otg.connect();
                println!("Connected again");
            }
            Some(key @ (b'r' | b'f')) if storage.busy() => println!("Busy with a command: '{}' refused", key as char),
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }

        // Asleep until the controller, the UART or the next report wakes the core.
        let deadline = Some(mct.count() + period.min(u64::from(soc::MCT_CLOCK_HZ / 20)));
        let mut irqs = [soc::OTG_IRQ; 2];
        if let Some(irq) = uart_irq {
            irqs[1] = irq;
        }
        sleep::until(mct, &irqs, deadline, || {
            uart.clear_rx_interrupt();
            otg.pending() || uart.rx_ready()
        });
    }
}
