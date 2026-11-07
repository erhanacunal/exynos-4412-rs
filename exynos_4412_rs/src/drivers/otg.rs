//! The USB OTG controller at [`soc::OTG_BASE`], a Synopsys DWC2 core, as a USB device on the
//! micro-USB port: endpoint 0 and its standard requests here, the other endpoints for a class
//! such as [`usb::msc`](crate::usb::msc). It moves data by its own DMA (buffer DMA mode), from
//! and to buffers in DRAM, and does the cache maintenance for them.
//!
//! [`Otg::poll`] does the work and reports what happened; [`Otg::wait`] sleeps until the
//! controller's interrupt line rises. A class answers its own requests on endpoint 0
//! ([`Event::ClassRequest`]) and starts its transfers on the other endpoints.
//!
//! Registers and fields: Linux 5.15.198 `drivers/usb/dwc2/hw.h`, cited by line. The settings
//! for this SoC's core follow `params.c:74-80` (`dwc2_set_s3c6400_params`: an 8-bit UTMI+
//! interface), `core.c:1144-1148` (the turnaround time for it) and the vendor kernel's
//! `drivers/usb/gadget/s3c_udc_otg.c:560-632` (the FIFO sizes, `regs-otg.h:168-176`).

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::timer::Mct;
use crate::drivers::usb_phy::{self, Mode};
use crate::usb::{DIR_IN, Descriptors, Kind, Recipient, Setup, Speed, descriptor, request};
use crate::{mmu, sleep, soc};

// hw.h:43-65.
const GOTGCTL: usize = 0x000;
const GOTGCTL_BSESVLD: u32 = 1 << 19;
// hw.h:67-73: GOTGINT, SESENDDET is bit 2.
const GOTGINT: usize = 0x004;
const GOTGINT_SES_END_DET: u32 = 1 << 2;
// hw.h:75-89.
const GAHBCFG: usize = 0x008;
const GAHBCFG_DMA_EN: u32 = 1 << 5;
const GAHBCFG_HBSTLEN_INCR4: u32 = 3 << 1;
const GAHBCFG_GLBL_INTR_EN: u32 = 1 << 0;
// hw.h:95-125.
const GUSBCFG: usize = 0x00C;
const GUSBCFG_FORCEDEVMODE: u32 = 1 << 30;
const GUSBCFG_FORCEHOSTMODE: u32 = 1 << 29;
const GUSBCFG_USBTRDTIM_MASK: u32 = 0xF << 10;
const GUSBCFG_USBTRDTIM_SHIFT: u32 = 10;
const GUSBCFG_HNPCAP: u32 = 1 << 9;
const GUSBCFG_SRPCAP: u32 = 1 << 8;
const GUSBCFG_PHYSEL: u32 = 1 << 6;
const GUSBCFG_ULPI_UTMI_SEL: u32 = 1 << 4;
const GUSBCFG_PHYIF16: u32 = 1 << 3;
const GUSBCFG_TOUTCAL_MASK: u32 = 7;
/// core.c:1148: the turnaround time of an 8-bit UTMI+ interface.
const USBTRDTIM_UTMI8: u32 = 9;
// hw.h:127-140.
const GRSTCTL: usize = 0x010;
const GRSTCTL_AHBIDLE: u32 = 1 << 31;
const GRSTCTL_TXFNUM_ALL: u32 = 0x10 << 6;
const GRSTCTL_TXFFLSH: u32 = 1 << 5;
const GRSTCTL_RXFFLSH: u32 = 1 << 4;
const GRSTCTL_CSFTRST: u32 = 1 << 0;
// hw.h:142-176.
const GINTSTS: usize = 0x014;
const GINTMSK: usize = 0x018;
const GINTSTS_WKUPINT: u32 = 1 << 31;
const GINTSTS_SESSREQINT: u32 = 1 << 30;
const GINTSTS_OEPINT: u32 = 1 << 19;
const GINTSTS_IEPINT: u32 = 1 << 18;
const GINTSTS_ENUMDONE: u32 = 1 << 13;
const GINTSTS_USBRST: u32 = 1 << 12;
const GINTSTS_USBSUSP: u32 = 1 << 11;
const GINTSTS_GOUTNAKEFF: u32 = 1 << 7;
const GINTSTS_OTGINT: u32 = 1 << 2;
// hw.h:202-206.
const GRXFSIZ: usize = 0x024;
const GNPTXFSIZ: usize = 0x028;
// hw.h:242-334.
const GSNPSID: usize = 0x040;
const GHWCFG1: usize = 0x044;
const GHWCFG2: usize = 0x048;
const GHWCFG3: usize = 0x04C;
const GHWCFG4: usize = 0x050;
/// The core's ID reads "OT" in its top half: hw.h:244 and `core.c`'s check of it.
const SNPSID_OT: u32 = 0x4F54_0000;
const GHWCFG2_NUM_DEV_EP_SHIFT: u32 = 10;
const GHWCFG4_DED_FIFO_EN: u32 = 1 << 25;
/// hw.h:436: `DPTXFSIZN(n)`, the FIFO of IN endpoint `n` from 1 on.
const DPTXFSIZN: usize = 0x104;
// hw.h:448-469.
const DCFG: usize = 0x800;
const DCFG_DEVADDR_MASK: u32 = 0x7F << 4;
const DCFG_DEVADDR_SHIFT: u32 = 4;
const DCFG_DEVSPD_MASK: u32 = 3;
const DCFG_DEVSPD_HS: u32 = 0;
// hw.h:471-483.
const DCTL: usize = 0x804;
const DCTL_CGOUTNAK: u32 = 1 << 10;
const DCTL_SGOUTNAK: u32 = 1 << 9;
const DCTL_CGNPINNAK: u32 = 1 << 8;
const DCTL_SFTDISCON: u32 = 1 << 1;
const DCTL_RMTWKUPSIG: u32 = 1 << 0;
// hw.h:485-497.
const DSTS: usize = 0x808;
const DSTS_ENUMSPD_SHIFT: u32 = 1;
const DSTS_ENUMSPD_MASK: u32 = 3;
const DSTS_ENUMSPD_HS: u32 = 0;
const DSTS_ENUMSPD_LS: u32 = 2;
// hw.h:499-526.
const DIEPMSK: usize = 0x810;
const DOEPMSK: usize = 0x814;
const DAINT: usize = 0x818;
const DAINTMSK: usize = 0x81C;
const DAINT_OUTEP_SHIFT: u32 = 16;
// hw.h:537-586: endpoint control, IN at 0x900 and OUT at 0xB00, 0x20 apart.
const DIEPCTL: usize = 0x900;
const DOEPCTL: usize = 0xB00;
const EP_STRIDE: usize = 0x20;
const DXEPCTL_EPENA: u32 = 1 << 31;
const DXEPCTL_EPDIS: u32 = 1 << 30;
const DXEPCTL_SETD0PID: u32 = 1 << 28;
const DXEPCTL_SNAK: u32 = 1 << 27;
const DXEPCTL_CNAK: u32 = 1 << 26;
const DXEPCTL_TXFNUM_SHIFT: u32 = 22;
const DXEPCTL_STALL: u32 = 1 << 21;
const DXEPCTL_EPTYPE_BULK: u32 = 2 << 18;
const DXEPCTL_USBACTEP: u32 = 1 << 15;
const DXEPCTL_MPS_MASK: u32 = 0x7FF;
/// hw.h:541-545: endpoint 0's packet size field, 0 for 64 bytes.
const D0EPCTL_MPS_MASK: u32 = 3;
// hw.h:588-614: endpoint interrupts, at 0x08 in each endpoint's registers.
const DXEPINT: usize = 0x08;
const DXEPINT_STSPHSERCVD: u32 = 1 << 5;
const DXEPINT_SETUP: u32 = 1 << 3;
const DXEPINT_TIMEOUT: u32 = 1 << 3;
const DXEPINT_AHBERR: u32 = 1 << 2;
const DXEPINT_EPDISBLD: u32 = 1 << 1;
const DXEPINT_XFERCOMPL: u32 = 1 << 0;
// hw.h:630-646: transfer size, at 0x10.
const DXEPTSIZ: usize = 0x10;
const DOEPTSIZ0_SUPCNT_SHIFT: u32 = 29;
const DXEPTSIZ_PKTCNT_SHIFT: u32 = 19;
// hw.h:647-648: DMA address, at 0x14.
const DXEPDMA: usize = 0x14;
// hw.h:652.
const PCGCTL: usize = 0xE00;

/// Endpoint 0's packets: 64 bytes at full and high speed.
pub const EP0_PACKET: usize = 64;
/// SETUP packets the core may store in a row: `DOEPTSIZ0`'s `SUPCNT`.
const SETUPS: usize = 3;
/// Endpoints the registers have room for.
const MAX_EPS: usize = 16;
/// SETUP packets kept for [`Otg::take_setups`].
const SETUP_LOG: usize = 16;
/// The longest answer endpoint 0 builds: a configuration descriptor at each speed has to fit.
const EP0_DATA: usize = 256;

/// FIFO sizes in 32-bit words: receive, endpoint 0's transmit, each other IN endpoint's.
/// The vendor's receive FIFO (`regs-otg.h:173`) when the RAM holds it, else less.
const FIFOS_LARGE: (u32, u32, u32) = (1024, 64, 512);
const FIFOS_SMALL: (u32, u32, u32) = (512, 32, 256);
/// IN endpoints that get a transmit FIFO: 1 to this. A class needs one or two.
pub const IN_ENDPOINTS: u8 = 3;
/// Words left at the top of the FIFO RAM: the core keeps its endpoints' DMA state there
/// (`GDFIFOCFG`'s `EPInfoBase`, hw.h:392-394).
const FIFO_RESERVED: u32 = 64;

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const RESET_TIMEOUT_US: u64 = 100_000;
/// After forcing device mode, the core takes up to 25 ms to switch: `core.c`,
/// `dwc2_force_mode`.
const FORCE_MODE_US: u64 = 25_000;
/// How long a disable or a flush may take before it is given up.
const STOP_TIMEOUT_US: u64 = 2_000;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller is already taken: there is one.
    InUse,
    /// The core did not identify itself: `GSNPSID`.
    NotFound(u32),
    /// The core did not come out of its soft reset, or its AHB master did not go idle:
    /// `GRSTCTL`.
    ResetTimeout(u32),
    /// The FIFO RAM is smaller than the smallest layout: its depth in words.
    FifoTooSmall(u32),
    /// Not an endpoint the controller has, or not enabled.
    BadEndpoint,
    /// A transfer is still running on the endpoint.
    Busy,
    /// A buffer the DMA cannot use: not 4-byte aligned, or for OUT not whole cache lines and
    /// whole packets.
    BadBuffer,
}

/// What [`Otg::poll`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The host reset the bus: every transfer is stopped, the device is unconfigured.
    Reset,
    /// The reset is over, at this speed.
    Enumerated(Speed),
    /// `SET_CONFIGURATION`: the class enables its endpoints for this value, or for 0 learns
    /// that they are gone.
    Configured(u8),
    /// A class or vendor request with no data stage, or with one to the host. Answer before
    /// the next [`Otg::poll`]: [`Otg::control_in`], [`Otg::control_ack`] or
    /// [`Otg::control_stall`]. Requests with data from the host are stalled before this.
    ClassRequest(Setup),
    /// The host cleared the halt of this endpoint (an address, bit 7 for IN).
    HaltCleared(u8),
    /// The transfer [`Otg::start_in`] started on this endpoint number is done.
    InDone(u8),
    /// The transfer [`Otg::start_out`] started on this endpoint number is done, with this many
    /// bytes; fewer than asked if the host ended it with a short packet.
    OutDone(u8, usize),
    /// The bus went idle for 3 ms.
    Suspend,
    /// The host resumed the bus.
    Resume,
    /// VBUS went away: the cable is out.
    Disconnected,
    /// VBUS came: the cable is in.
    SessionRequest,
}

/// What the core says it is, and the FIFOs it got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// `GSNPSID`: "OT" and the core's version, `0x4F54_xyzA` for x.yz a.
    pub snpsid: u32,
    /// `GHWCFG1` to `GHWCFG4`.
    pub hwcfg: [u32; 4],
    /// Endpoints besides 0.
    pub endpoints: u8,
    /// Whether each IN endpoint has a transmit FIFO of its own.
    pub dedicated_fifos: bool,
    /// The FIFO RAM, in 32-bit words.
    pub fifo_words: u32,
    /// The receive FIFO, endpoint 0's transmit FIFO, each other's: in words.
    pub fifos: (u32, u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Waiting for a SETUP.
    Idle,
    /// Sending the data stage.
    DataIn,
    /// Sending the zero-length status packet.
    StatusIn,
    /// A class request is out with the class.
    Class,
}

#[derive(Clone, Copy, Debug, Default)]
struct Transfer {
    active: bool,
    address: usize,
    len: usize,
    done: usize,
    /// What the hardware runs now.
    chunk: usize,
}

#[repr(C, align(32))]
struct Ep0Dma {
    /// Room for the SETUPs, whole cache lines.
    setup: [u8; 32],
    /// One packet of a data stage.
    data: [u8; EP0_PACKET],
}

struct Ep0Buffers(UnsafeCell<Ep0Dma>);

// SAFETY: only the one `Otg` (see `TAKEN`) touches it, and the DMA, which the `Otg` starts.
unsafe impl Sync for Ep0Buffers {}

static EP0: Ep0Buffers = Ep0Buffers(UnsafeCell::new(Ep0Dma { setup: [0; 32], data: [0; EP0_PACKET] }));
static TAKEN: AtomicBool = AtomicBool::new(false);

const _: () = assert!(SETUPS * 8 <= 32);

fn read(offset: usize) -> u32 {
    // SAFETY: every offset is a register of the controller's 128 KiB, device memory that stays
    // mapped for the whole program; its gate is open from `Otg::new` on.
    unsafe { ptr::read_volatile((soc::OTG_BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as in `read`.
    unsafe { ptr::write_volatile((soc::OTG_BASE + offset) as *mut u32, value) }
}

fn modify(offset: usize, clear: u32, set: u32) {
    write(offset, (read(offset) & !clear) | set);
}

const fn in_reg(ep: usize, offset: usize) -> usize {
    DIEPCTL + ep * EP_STRIDE + offset
}

const fn out_reg(ep: usize, offset: usize) -> usize {
    DOEPCTL + ep * EP_STRIDE + offset
}

/// Waits until `done` with a timeout; whether it came.
fn wait_for(mct: Mct, timeout_us: u64, done: impl Fn() -> bool) -> bool {
    let begin = mct.count();
    while !done() {
        if mct.count() - begin > timeout_us * COUNTS_PER_US {
            return false;
        }
    }
    true
}

/// The device side of the OTG controller. There is one: [`Otg::new`] fails the second time.
pub struct Otg {
    mct: Mct,
    descriptors: Descriptors<'static>,
    info: Info,
    speed: Speed,
    configuration: u8,
    stage: Stage,
    data: [u8; EP0_DATA],
    data_len: usize,
    data_sent: usize,
    chunk: usize,
    zlp: bool,
    /// Packet size of each IN and OUT endpoint, 0 while disabled.
    in_mps: [u16; MAX_EPS],
    out_mps: [u16; MAX_EPS],
    in_xfer: [Transfer; MAX_EPS],
    out_xfer: [Transfer; MAX_EPS],
    /// Halted endpoints, bit `n` IN `n`, bit `16 + n` OUT `n`; wedged ones stay halted when
    /// the host clears them.
    halted: u32,
    wedged: u32,
    /// Largest transfer the size registers take, in bytes and in packets.
    max_bytes: usize,
    max_packets: usize,
    /// The SETUPs since the last [`take_setups`](Self::take_setups), and how many did not fit.
    setup_log: [Setup; SETUP_LOG],
    setups: usize,
    setups_lost: u32,
}

impl Otg {
    /// Takes the controller: turns PHY0 on for it, resets the core and sets it up as a
    /// high-speed device presenting `descriptors`, but not connected yet ([`connect`](Self::connect)).
    /// The MCT must run. `vdd_uotg_3v3` (LDO12) and `vdd_uotg_1v0` (LDO15) must be on.
    pub fn new(mct: Mct, descriptors: Descriptors<'static>) -> Result<Self, Error> {
        if TAKEN.load(Ordering::Relaxed) {
            return Err(Error::InUse);
        }
        usb_phy::power_on(Mode::Device, mct);

        let snpsid = read(GSNPSID);
        if snpsid & 0xFFFF_0000 != SNPSID_OT {
            return Err(Error::NotFound(snpsid));
        }
        // dwc2_core_reset: the AHB master idle, the soft reset, idle again.
        if !wait_for(mct, RESET_TIMEOUT_US, || read(GRSTCTL) & GRSTCTL_AHBIDLE != 0) {
            return Err(Error::ResetTimeout(read(GRSTCTL)));
        }
        modify(GRSTCTL, 0, GRSTCTL_CSFTRST);
        if !wait_for(mct, RESET_TIMEOUT_US, || read(GRSTCTL) & GRSTCTL_CSFTRST == 0) {
            return Err(Error::ResetTimeout(read(GRSTCTL)));
        }
        if !wait_for(mct, RESET_TIMEOUT_US, || read(GRSTCTL) & GRSTCTL_AHBIDLE != 0) {
            return Err(Error::ResetTimeout(read(GRSTCTL)));
        }

        // An 8-bit UTMI+ PHY, high speed, no HNP or SRP, the device role forced.
        modify(
            GUSBCFG,
            GUSBCFG_FORCEHOSTMODE
                | GUSBCFG_USBTRDTIM_MASK
                | GUSBCFG_HNPCAP
                | GUSBCFG_SRPCAP
                | GUSBCFG_PHYSEL
                | GUSBCFG_ULPI_UTMI_SEL
                | GUSBCFG_PHYIF16
                | GUSBCFG_TOUTCAL_MASK,
            GUSBCFG_FORCEDEVMODE | USBTRDTIM_UTMI8 << GUSBCFG_USBTRDTIM_SHIFT | 7,
        );
        sleep::for_us(mct, FORCE_MODE_US);
        write(PCGCTL, 0);
        modify(DCTL, 0, DCTL_SFTDISCON);
        modify(DCFG, DCFG_DEVSPD_MASK | DCFG_DEVADDR_MASK, DCFG_DEVSPD_HS);

        let hwcfg = [read(GHWCFG1), read(GHWCFG2), read(GHWCFG3), read(GHWCFG4)];
        let endpoints = ((hwcfg[1] >> GHWCFG2_NUM_DEV_EP_SHIFT) & 0xF) as u8;
        let fifo_words = hwcfg[2] >> 16;
        let in_fifos = u32::from(endpoints.min(IN_ENDPOINTS));
        let fifos = [FIFOS_LARGE, FIFOS_SMALL]
            .into_iter()
            .find(|(rx, tx0, tx)| rx + tx0 + tx * in_fifos + FIFO_RESERVED <= fifo_words)
            .ok_or(Error::FifoTooSmall(fifo_words))?;
        let (rx, tx0, tx) = fifos;
        write(GRXFSIZ, rx);
        write(GNPTXFSIZ, tx0 << 16 | rx);
        for ep in 1..=in_fifos {
            write(DPTXFSIZN + (ep as usize - 1) * 4, tx << 16 | (rx + tx0 + tx * (ep - 1)));
        }
        let xfer_bits = (hwcfg[2] & 0xF) + 11;
        let packet_bits = ((hwcfg[2] >> 4) & 7) + 4;

        let info = Info {
            snpsid,
            hwcfg,
            endpoints,
            dedicated_fifos: hwcfg[3] & GHWCFG4_DED_FIFO_EN != 0,
            fifo_words,
            fifos,
        };
        let otg = Self {
            mct,
            descriptors,
            info,
            speed: Speed::High,
            configuration: 0,
            stage: Stage::Idle,
            data: [0; EP0_DATA],
            data_len: 0,
            data_sent: 0,
            chunk: 0,
            zlp: false,
            in_mps: [0; MAX_EPS],
            out_mps: [0; MAX_EPS],
            in_xfer: [Transfer::default(); MAX_EPS],
            out_xfer: [Transfer::default(); MAX_EPS],
            halted: 0,
            wedged: 0,
            max_bytes: (1 << xfer_bits) - 1,
            max_packets: (1 << packet_bits) - 1,
            setup_log: [Setup::from_bytes(&[0; 8]); SETUP_LOG],
            setups: 0,
            setups_lost: 0,
        };
        otg.flush_fifos();

        write(GAHBCFG, GAHBCFG_DMA_EN | GAHBCFG_HBSTLEN_INCR4 | GAHBCFG_GLBL_INTR_EN);
        write(GOTGINT, u32::MAX);
        for ep in 0..=usize::from(endpoints) {
            write(in_reg(ep, DXEPINT), u32::MAX);
            write(out_reg(ep, DXEPINT), u32::MAX);
        }
        write(DIEPMSK, DXEPINT_XFERCOMPL | DXEPINT_AHBERR | DXEPINT_TIMEOUT);
        write(DOEPMSK, DXEPINT_XFERCOMPL | DXEPINT_SETUP | DXEPINT_AHBERR | DXEPINT_STSPHSERCVD);
        write(DAINTMSK, 1 | 1 << DAINT_OUTEP_SHIFT);
        write(GINTSTS, u32::MAX);
        write(
            GINTMSK,
            GINTSTS_USBRST
                | GINTSTS_ENUMDONE
                | GINTSTS_IEPINT
                | GINTSTS_OEPINT
                | GINTSTS_USBSUSP
                | GINTSTS_WKUPINT
                | GINTSTS_OTGINT
                | GINTSTS_SESSREQINT,
        );
        TAKEN.store(true, Ordering::Relaxed);
        Ok(otg)
    }

    /// What the core is.
    pub const fn info(&self) -> Info {
        self.info
    }

    /// The speed of the last enumeration.
    pub const fn speed(&self) -> Speed {
        self.speed
    }

    /// The configuration the host set, 0 for none.
    pub const fn configuration(&self) -> u8 {
        self.configuration
    }

    /// Hands over the SETUP packets received since the last call, oldest first, for a trace;
    /// returns how many more came than were kept.
    pub fn take_setups(&mut self, mut each: impl FnMut(Setup)) -> u32 {
        self.setup_log[..self.setups].iter().for_each(|setup| each(*setup));
        self.setups = 0;
        core::mem::take(&mut self.setups_lost)
    }

    /// Whether VBUS is there, from the PHY's B-session valid.
    pub fn vbus(&self) -> bool {
        read(GOTGCTL) & GOTGCTL_BSESVLD != 0
    }

    /// Pulls D+ up: the host sees a device arrive.
    pub fn connect(&mut self) {
        modify(DCTL, DCTL_SFTDISCON, 0);
    }

    /// Lets D+ go: the host sees the device leave.
    pub fn disconnect(&mut self) {
        modify(DCTL, 0, DCTL_SFTDISCON);
    }

    /// Whether the interrupt line is up: [`poll`](Self::poll) has something.
    pub fn pending(&self) -> bool {
        read(GINTSTS) & read(GINTMSK) != 0
    }

    /// Sleeps until [`pending`](Self::pending), or until the MCT counter reaches `deadline`.
    pub fn wait(&self, deadline: Option<u64>) -> bool {
        sleep::until(self.mct, &[soc::OTG_IRQ], deadline, || self.pending())
    }

    fn flush_fifos(&self) {
        write(GRSTCTL, GRSTCTL_TXFNUM_ALL | GRSTCTL_TXFFLSH);
        wait_for(self.mct, STOP_TIMEOUT_US, || read(GRSTCTL) & GRSTCTL_TXFFLSH == 0);
        write(GRSTCTL, GRSTCTL_RXFFLSH);
        wait_for(self.mct, STOP_TIMEOUT_US, || read(GRSTCTL) & GRSTCTL_RXFFLSH == 0);
    }

    /// Handles what the controller flagged, one thing at a time: call until it returns `None`,
    /// then [`wait`](Self::wait).
    pub fn poll(&mut self) -> Option<Event> {
        loop {
            let status = read(GINTSTS) & read(GINTMSK);
            if status == 0 {
                return None;
            }
            if status & GINTSTS_USBRST != 0 {
                write(GINTSTS, GINTSTS_USBRST);
                self.bus_reset();
                return Some(Event::Reset);
            }
            if status & GINTSTS_ENUMDONE != 0 {
                write(GINTSTS, GINTSTS_ENUMDONE);
                return Some(Event::Enumerated(self.enumerated()));
            }
            if status & GINTSTS_OTGINT != 0 {
                let otg = read(GOTGINT);
                write(GOTGINT, otg);
                if otg & GOTGINT_SES_END_DET != 0 {
                    self.stop_all();
                    self.configuration = 0;
                    return Some(Event::Disconnected);
                }
                continue;
            }
            if status & GINTSTS_SESSREQINT != 0 {
                write(GINTSTS, GINTSTS_SESSREQINT);
                return Some(Event::SessionRequest);
            }
            if status & GINTSTS_USBSUSP != 0 {
                write(GINTSTS, GINTSTS_USBSUSP);
                return Some(Event::Suspend);
            }
            if status & GINTSTS_WKUPINT != 0 {
                write(GINTSTS, GINTSTS_WKUPINT);
                return Some(Event::Resume);
            }
            let daint = read(DAINT) & read(DAINTMSK);
            if status & GINTSTS_OEPINT != 0
                && let Some(ep) = (0..MAX_EPS).find(|ep| daint & (1 << (DAINT_OUTEP_SHIFT as usize + ep)) != 0)
            {
                if let Some(event) = self.out_interrupt(ep) {
                    return Some(event);
                }
                continue;
            }
            if status & GINTSTS_IEPINT != 0
                && let Some(ep) = (0..MAX_EPS).find(|ep| daint & (1 << ep) != 0)
            {
                if let Some(event) = self.in_interrupt(ep) {
                    return Some(event);
                }
                continue;
            }
            // The endpoint summaries clear themselves with the endpoints' bits.
            if status & !(GINTSTS_OEPINT | GINTSTS_IEPINT) == 0 {
                return None;
            }
            write(GINTSTS, status & !(GINTSTS_OEPINT | GINTSTS_IEPINT));
        }
    }

    fn bus_reset(&mut self) {
        modify(DCTL, DCTL_RMTWKUPSIG, 0);
        self.stop_all();
        self.flush_fifos();
        modify(DCFG, DCFG_DEVADDR_MASK, 0);
        self.configuration = 0;
        self.stage = Stage::Idle;
        self.arm_setup();
    }

    fn enumerated(&mut self) -> Speed {
        self.speed = match (read(DSTS) >> DSTS_ENUMSPD_SHIFT) & DSTS_ENUMSPD_MASK {
            DSTS_ENUMSPD_HS => Speed::High,
            DSTS_ENUMSPD_LS => Speed::Low,
            _ => Speed::Full,
        };
        modify(in_reg(0, 0), D0EPCTL_MPS_MASK, 0);
        modify(DCTL, 0, DCTL_CGNPINNAK);
        self.arm_setup();
        self.speed
    }

    /// Stops every transfer and turns every endpoint but 0 off.
    fn stop_all(&mut self) {
        self.stop_ep0_in();
        for ep in 1..MAX_EPS {
            self.disable_in(ep);
            self.disable_out(ep);
        }
        write(DAINTMSK, 1 | 1 << DAINT_OUTEP_SHIFT);
        self.halted = 0;
        self.wedged = 0;
    }

    fn stop_in(&self, ep: usize) {
        if read(in_reg(ep, 0)) & DXEPCTL_EPENA != 0 {
            modify(in_reg(ep, 0), 0, DXEPCTL_SNAK | DXEPCTL_EPDIS);
            wait_for(self.mct, STOP_TIMEOUT_US, || read(in_reg(ep, DXEPINT)) & DXEPINT_EPDISBLD != 0);
            write(in_reg(ep, DXEPINT), DXEPINT_EPDISBLD);
        }
    }

    fn stop_out(&self, ep: usize) {
        if read(out_reg(ep, 0)) & DXEPCTL_EPENA != 0 {
            // An OUT endpoint is disabled under the global OUT NAK.
            modify(DCTL, 0, DCTL_SGOUTNAK);
            wait_for(self.mct, STOP_TIMEOUT_US, || read(GINTSTS) & GINTSTS_GOUTNAKEFF != 0);
            modify(out_reg(ep, 0), 0, DXEPCTL_SNAK | DXEPCTL_EPDIS);
            wait_for(self.mct, STOP_TIMEOUT_US, || read(out_reg(ep, DXEPINT)) & DXEPINT_EPDISBLD != 0);
            write(out_reg(ep, DXEPINT), DXEPINT_EPDISBLD);
            modify(DCTL, 0, DCTL_CGOUTNAK);
        }
    }

    fn stop_ep0_in(&mut self) {
        self.stop_in(0);
        write(GRSTCTL, GRSTCTL_TXFFLSH);
        wait_for(self.mct, STOP_TIMEOUT_US, || read(GRSTCTL) & GRSTCTL_TXFFLSH == 0);
    }

    fn disable_in(&mut self, ep: usize) {
        if ep > usize::from(self.info.endpoints) {
            return;
        }
        self.stop_in(ep);
        modify(in_reg(ep, 0), DXEPCTL_USBACTEP | DXEPCTL_STALL, DXEPCTL_SNAK);
        self.in_mps[ep] = 0;
        self.in_xfer[ep].active = false;
    }

    fn disable_out(&mut self, ep: usize) {
        if ep > usize::from(self.info.endpoints) {
            return;
        }
        self.stop_out(ep);
        modify(out_reg(ep, 0), DXEPCTL_USBACTEP | DXEPCTL_STALL, DXEPCTL_SNAK);
        self.out_mps[ep] = 0;
        self.out_xfer[ep].active = false;
    }

    /// Enables a bulk endpoint, `address` with bit 7 for IN, packets of `packet` bytes (512 at
    /// high speed, 64 at full speed), from DATA0, not halted.
    pub fn enable_bulk(&mut self, address: u8, packet: u16) -> Result<(), Error> {
        let ep = usize::from(address & 0x0F);
        if ep == 0 || ep > usize::from(self.info.endpoints) || packet == 0 || u32::from(packet) > DXEPCTL_MPS_MASK {
            return Err(Error::BadEndpoint);
        }
        let control = DXEPCTL_USBACTEP | DXEPCTL_EPTYPE_BULK | DXEPCTL_SETD0PID | DXEPCTL_SNAK | u32::from(packet);
        if address & DIR_IN != 0 {
            if ep > usize::from(IN_ENDPOINTS) {
                return Err(Error::BadEndpoint);
            }
            self.disable_in(ep);
            let fifo = if self.info.dedicated_fifos { (ep as u32) << DXEPCTL_TXFNUM_SHIFT } else { 0 };
            write(in_reg(ep, 0), control | fifo);
            self.in_mps[ep] = packet;
            modify(DAINTMSK, 0, 1 << ep);
        } else {
            self.disable_out(ep);
            write(out_reg(ep, 0), control);
            self.out_mps[ep] = packet;
            modify(DAINTMSK, 0, 1 << (DAINT_OUTEP_SHIFT as usize + ep));
        }
        self.set_halt_bits(address, false, false);
        Ok(())
    }

    fn halt_bit(address: u8) -> u32 {
        let ep = u32::from(address & 0x0F);
        if address & DIR_IN != 0 { 1 << ep } else { 1 << (16 + ep) }
    }

    fn set_halt_bits(&mut self, address: u8, halted: bool, wedged: bool) {
        let bit = Self::halt_bit(address);
        self.halted = if halted { self.halted | bit } else { self.halted & !bit };
        self.wedged = if wedged { self.wedged | bit } else { self.wedged & !bit };
    }

    /// Whether the endpoint at `address` is halted.
    pub fn is_halted(&self, address: u8) -> bool {
        self.halted & Self::halt_bit(address) != 0
    }

    /// Halts the endpoint at `address` (bit 7 for IN), or ends its halt and starts it again
    /// from DATA0. With `wedge`, a halt stays when the host clears it, until the class ends it
    /// here: the Bulk-Only Transport's way after an invalid command.
    pub fn set_halt(&mut self, address: u8, halt: bool, wedge: bool) -> Result<(), Error> {
        let ep = usize::from(address & 0x0F);
        if ep == 0 || ep > usize::from(self.info.endpoints) {
            return Err(Error::BadEndpoint);
        }
        let control = if address & DIR_IN != 0 { in_reg(ep, 0) } else { out_reg(ep, 0) };
        if halt {
            modify(control, 0, DXEPCTL_STALL);
        } else {
            modify(control, DXEPCTL_STALL, DXEPCTL_SETD0PID);
        }
        self.set_halt_bits(address, halt, halt && wedge);
        Ok(())
    }

    /// Stops the transfer on the endpoint at `address` (bit 7 for IN), if one runs: no event
    /// comes for it, and its buffer is free again. What it moved is lost.
    pub fn abort(&mut self, address: u8) {
        let ep = usize::from(address & 0x0F);
        if ep == 0 || ep > usize::from(self.info.endpoints) {
            return;
        }
        if address & DIR_IN != 0 {
            self.stop_in(ep);
            write(GRSTCTL, (ep as u32) << 6 | GRSTCTL_TXFFLSH);
            wait_for(self.mct, STOP_TIMEOUT_US, || read(GRSTCTL) & GRSTCTL_TXFFLSH == 0);
            self.in_xfer[ep].active = false;
        } else {
            self.stop_out(ep);
            if self.out_xfer[ep].active {
                let xfer = self.out_xfer[ep];
                mmu::invalidate_dcache_range(xfer.address, xfer.len);
            }
            self.out_xfer[ep].active = false;
        }
    }

    fn check_endpoint(&self, ep: u8, mps: &[u16; MAX_EPS], xfer: &[Transfer; MAX_EPS]) -> Result<usize, Error> {
        let ep = usize::from(ep);
        if ep == 0 || ep >= MAX_EPS || mps[ep] == 0 {
            return Err(Error::BadEndpoint);
        }
        if xfer[ep].active {
            return Err(Error::Busy);
        }
        Ok(ep)
    }

    /// The most one run of the hardware moves on an endpoint with these packets.
    fn max_chunk(&self, mps: usize) -> usize {
        (self.max_packets * mps).min(self.max_bytes / mps * mps)
    }

    /// Starts sending `data` on IN endpoint `ep` (a number), in as many packets as it takes:
    /// [`Event::InDone`] when the host has it all. A length of a whole number of packets ends
    /// without a short packet; zero sends one zero-length packet.
    ///
    /// # Safety
    ///
    /// `data` must stay valid and unchanged until [`Event::InDone`] for `ep`, or until
    /// [`Event::Reset`], [`Event::Disconnected`] or [`Event::Configured`] stop the transfer:
    /// the controller reads it by DMA meanwhile.
    pub unsafe fn start_in(&mut self, ep: u8, data: &[u8]) -> Result<(), Error> {
        let ep = self.check_endpoint(ep, &self.in_mps, &self.in_xfer)?;
        let address = data.as_ptr() as usize;
        if !address.is_multiple_of(4) {
            return Err(Error::BadBuffer);
        }
        mmu::clean_dcache_range(address, data.len());
        self.in_xfer[ep] = Transfer { active: true, address, len: data.len(), done: 0, chunk: 0 };
        self.kick_in(ep);
        Ok(())
    }

    fn kick_in(&mut self, ep: usize) {
        let mps = usize::from(self.in_mps[ep]);
        let max = self.max_chunk(mps);
        let xfer = &mut self.in_xfer[ep];
        let chunk = (xfer.len - xfer.done).min(max);
        let packets = chunk.div_ceil(mps).max(1);
        xfer.chunk = chunk;
        write(in_reg(ep, DXEPTSIZ), (packets as u32) << DXEPTSIZ_PKTCNT_SHIFT | chunk as u32);
        write(in_reg(ep, DXEPDMA), (xfer.address + xfer.done) as u32);
        modify(in_reg(ep, 0), DXEPCTL_STALL, DXEPCTL_EPENA | DXEPCTL_CNAK);
    }

    /// Starts receiving into `buffer` on OUT endpoint `ep` (a number), up to its length:
    /// [`Event::OutDone`] when it is full or the host ends early with a short packet.
    ///
    /// # Safety
    ///
    /// `buffer` must stay valid and untouched until [`Event::OutDone`] for `ep`, or until
    /// [`Event::Reset`], [`Event::Disconnected`] or [`Event::Configured`] stop the transfer:
    /// the controller writes it by DMA meanwhile. It must start on a cache line
    /// ([`mmu::DCACHE_LINE`]) and its length be whole cache lines and whole packets.
    pub unsafe fn start_out(&mut self, ep: u8, buffer: &mut [u8]) -> Result<(), Error> {
        let ep = self.check_endpoint(ep, &self.out_mps, &self.out_xfer)?;
        let address = buffer.as_mut_ptr() as usize;
        let mps = usize::from(self.out_mps[ep]);
        if buffer.is_empty()
            || !address.is_multiple_of(mmu::DCACHE_LINE)
            || !buffer.len().is_multiple_of(mmu::DCACHE_LINE)
            || !buffer.len().is_multiple_of(mps)
        {
            return Err(Error::BadBuffer);
        }
        mmu::discard_dcache_range(address, buffer.len());
        self.out_xfer[ep] = Transfer { active: true, address, len: buffer.len(), done: 0, chunk: 0 };
        self.kick_out(ep);
        Ok(())
    }

    fn kick_out(&mut self, ep: usize) {
        let mps = usize::from(self.out_mps[ep]);
        let max = self.max_chunk(mps);
        let xfer = &mut self.out_xfer[ep];
        let chunk = (xfer.len - xfer.done).min(max);
        xfer.chunk = chunk;
        write(out_reg(ep, DXEPTSIZ), ((chunk / mps) as u32) << DXEPTSIZ_PKTCNT_SHIFT | chunk as u32);
        write(out_reg(ep, DXEPDMA), (xfer.address + xfer.done) as u32);
        modify(out_reg(ep, 0), DXEPCTL_STALL, DXEPCTL_EPENA | DXEPCTL_CNAK);
    }

    fn in_interrupt(&mut self, ep: usize) -> Option<Event> {
        // All of them cleared, masked or not, so that the endpoint's summary bit falls.
        let raw = read(in_reg(ep, DXEPINT));
        write(in_reg(ep, DXEPINT), raw);
        let flags = raw & read(DIEPMSK);
        if flags & DXEPINT_XFERCOMPL == 0 {
            return None;
        }
        if ep == 0 {
            self.ep0_in_done();
            return None;
        }
        let xfer = &mut self.in_xfer[ep];
        if !xfer.active {
            return None;
        }
        xfer.done += xfer.chunk;
        if xfer.done < xfer.len {
            self.kick_in(ep);
            return None;
        }
        xfer.active = false;
        Some(Event::InDone(ep as u8))
    }

    fn out_interrupt(&mut self, ep: usize) -> Option<Event> {
        let raw = read(out_reg(ep, DXEPINT));
        write(out_reg(ep, DXEPINT), raw);
        let flags = raw & read(DOEPMSK);
        if ep == 0 {
            let mut event = None;
            if flags & DXEPINT_SETUP != 0 {
                event = self.setup_received();
            }
            if flags & (DXEPINT_SETUP | DXEPINT_XFERCOMPL) != 0 {
                self.arm_setup();
            }
            return event;
        }
        if flags & DXEPINT_XFERCOMPL == 0 {
            return None;
        }
        let left = (read(out_reg(ep, DXEPTSIZ)) as usize) & self.max_bytes;
        let xfer = &mut self.out_xfer[ep];
        if !xfer.active {
            return None;
        }
        let got = xfer.chunk.saturating_sub(left);
        xfer.done += got;
        if got == xfer.chunk && xfer.done < xfer.len {
            self.kick_out(ep);
            return None;
        }
        xfer.active = false;
        mmu::invalidate_dcache_range(xfer.address, xfer.len);
        Some(Event::OutDone(ep as u8, xfer.done))
    }

    // Endpoint 0.

    fn ep0() -> *mut Ep0Dma {
        EP0.0.get()
    }

    /// Lets endpoint 0 take SETUPs, and the status stage of a read, into the SETUP buffer.
    fn arm_setup(&self) {
        if read(out_reg(0, 0)) & DXEPCTL_EPENA != 0 {
            return;
        }
        // SAFETY: the one `Otg` owns `EP0`; only its address is taken here.
        let setup = unsafe { ptr::addr_of_mut!((*Self::ep0()).setup) } as usize;
        mmu::discard_dcache_range(setup, 32);
        write(out_reg(0, DXEPTSIZ), (SETUPS as u32) << DOEPTSIZ0_SUPCNT_SHIFT | 1 << DXEPTSIZ_PKTCNT_SHIFT | (SETUPS * 8) as u32);
        write(out_reg(0, DXEPDMA), setup as u32);
        modify(out_reg(0, 0), DXEPCTL_STALL, DXEPCTL_EPENA | DXEPCTL_CNAK);
    }

    fn setup_received(&mut self) -> Option<Event> {
        // SAFETY: as in `arm_setup`; the DMA is done with the buffer, the endpoint off.
        let setup = unsafe { ptr::addr_of_mut!((*Self::ep0()).setup) } as usize;
        mmu::invalidate_dcache_range(setup, 32);
        // The address moves on 8 bytes for each SETUP stored: the last one is just before it.
        let end = read(out_reg(0, DXEPDMA)) as usize;
        let last = if end >= setup + 8 && end <= setup + SETUPS * 8 { end - 8 } else { setup };
        let mut bytes = [0u8; 8];
        for (i, byte) in bytes.iter_mut().enumerate() {
            // SAFETY: `last + i` lies in the SETUP buffer, written by the DMA and invalidated.
            *byte = unsafe { ptr::read_volatile((last + i) as *const u8) };
        }
        let setup = Setup::from_bytes(&bytes);
        if self.setups < SETUP_LOG {
            self.setup_log[self.setups] = setup;
            self.setups += 1;
        } else {
            self.setups_lost += 1;
        }
        if self.stage != Stage::Idle {
            // The host gave up on the last request.
            self.stop_ep0_in();
            self.stage = Stage::Idle;
        }
        self.request(setup)
    }

    fn request(&mut self, setup: Setup) -> Option<Event> {
        if setup.kind() != Kind::Standard {
            if !setup.is_in() && setup.length != 0 {
                self.control_stall();
                return None;
            }
            self.stage = Stage::Class;
            self.data_len = usize::from(setup.length);
            return Some(Event::ClassRequest(setup));
        }
        let length = usize::from(setup.length);
        match (setup.request, setup.recipient()) {
            (request::GET_DESCRIPTOR, Recipient::Device) if setup.is_in() => match self.descriptor(setup.value) {
                Some(len) => self.reply(len, length),
                None => self.control_stall(),
            },
            (request::SET_ADDRESS, Recipient::Device) => {
                // The core answers the status stage from the old address: set it now.
                modify(DCFG, DCFG_DEVADDR_MASK, (u32::from(setup.value) & 0x7F) << DCFG_DEVADDR_SHIFT);
                self.status_in();
            }
            (request::GET_CONFIGURATION, Recipient::Device) => {
                self.data[0] = self.configuration;
                self.reply(1, length);
            }
            (request::SET_CONFIGURATION, Recipient::Device) => {
                let value = setup.value as u8;
                if value > 1 {
                    self.control_stall();
                    return None;
                }
                self.stop_all();
                self.configuration = value;
                self.status_in();
                return Some(Event::Configured(value));
            }
            (request::GET_INTERFACE, Recipient::Interface) if self.configuration != 0 => {
                self.data[0] = 0;
                self.reply(1, length);
            }
            (request::SET_INTERFACE, Recipient::Interface) if self.configuration != 0 && setup.value == 0 => {
                self.status_in();
            }
            (request::GET_STATUS, Recipient::Device | Recipient::Interface) => {
                self.data[..2].fill(0);
                self.reply(2, length);
            }
            (request::GET_STATUS, Recipient::Endpoint) => match self.endpoint(setup.index) {
                Some(address) => {
                    self.data[0] = u8::from(address & 0x0F != 0 && self.is_halted(address));
                    self.data[1] = 0;
                    self.reply(2, length);
                }
                None => self.control_stall(),
            },
            (request::CLEAR_FEATURE | request::SET_FEATURE, Recipient::Endpoint)
                if setup.value == request::FEATURE_ENDPOINT_HALT =>
            {
                let Some(address) = self.endpoint(setup.index) else {
                    self.control_stall();
                    return None;
                };
                if address & 0x0F == 0 {
                    self.status_in();
                    return None;
                }
                if setup.request == request::SET_FEATURE {
                    let _ = self.set_halt(address, true, false);
                    self.status_in();
                    return None;
                }
                if self.wedged & Self::halt_bit(address) == 0 {
                    let _ = self.set_halt(address, false, false);
                }
                self.status_in();
                return Some(Event::HaltCleared(address));
            }
            // Remote wake-up and test modes: accepted, not acted on.
            (request::CLEAR_FEATURE | request::SET_FEATURE, Recipient::Device) => self.status_in(),
            _ => self.control_stall(),
        }
        None
    }

    /// The address in a request's `wIndex`, if it is an endpoint that is there.
    fn endpoint(&self, index: u16) -> Option<u8> {
        let address = index as u8 & (DIR_IN | 0x0F);
        let ep = usize::from(address & 0x0F);
        let on = ep == 0 || if address & DIR_IN != 0 { self.in_mps[ep] != 0 } else { self.out_mps[ep] != 0 };
        on.then_some(address)
    }

    /// Puts descriptor `value` (type in the high byte, index in the low) in `data`: its length.
    fn descriptor(&mut self, value: u16) -> Option<usize> {
        let (kind, index) = ((value >> 8) as u8, value as u8);
        let descriptors = self.descriptors;
        let (this, other) = match self.speed {
            Speed::High => (descriptors.high_speed, descriptors.full_speed),
            _ => (descriptors.full_speed, descriptors.high_speed),
        };
        let copy = |data: &mut [u8; EP0_DATA], bytes: &[u8]| {
            let len = bytes.len().min(EP0_DATA);
            data[..len].copy_from_slice(&bytes[..len]);
            len
        };
        match kind {
            descriptor::DEVICE => Some(copy(&mut self.data, descriptors.device)),
            descriptor::CONFIGURATION if index == 0 => Some(copy(&mut self.data, this)),
            descriptor::OTHER_SPEED_CONFIGURATION if index == 0 => {
                let len = copy(&mut self.data, other);
                self.data[1] = descriptor::OTHER_SPEED_CONFIGURATION;
                Some(len)
            }
            descriptor::DEVICE_QUALIFIER => {
                // USB 2.0, table 9-9: the device descriptor's fields that do not change.
                let device = descriptors.device;
                self.data[..10].copy_from_slice(&[
                    10,
                    descriptor::DEVICE_QUALIFIER,
                    device[2],
                    device[3],
                    device[4],
                    device[5],
                    device[6],
                    device[7],
                    device[17],
                    0,
                ]);
                Some(10)
            }
            descriptor::STRING if index == 0 => {
                // US English only.
                self.data[..4].copy_from_slice(&[4, descriptor::STRING, 0x09, 0x04]);
                Some(4)
            }
            descriptor::STRING => {
                let text = descriptors.strings.get(usize::from(index) - 1)?;
                let mut len = 2;
                for unit in text.encode_utf16() {
                    if len + 2 > EP0_DATA.min(255) {
                        break;
                    }
                    self.data[len..len + 2].copy_from_slice(&unit.to_le_bytes());
                    len += 2;
                }
                self.data[0] = len as u8;
                self.data[1] = descriptor::STRING;
                Some(len)
            }
            _ => None,
        }
    }

    /// Sends the first `len` bytes of `data` as a data stage the host asked `asked` bytes of.
    fn reply(&mut self, len: usize, asked: usize) {
        let len = len.min(asked);
        self.data_len = len;
        self.data_sent = 0;
        // A shorter answer than asked ends in a short packet, zero-length if need be.
        self.zlp = len > 0 && len < asked && len.is_multiple_of(EP0_PACKET);
        self.stage = Stage::DataIn;
        self.send_ep0_packet();
    }

    fn send_ep0_packet(&mut self) {
        let chunk = (self.data_len - self.data_sent).min(EP0_PACKET);
        let buffer = Self::ep0();
        // SAFETY: the one `Otg` owns `EP0`, and endpoint 0's IN DMA is idle between packets.
        let data = unsafe { &mut (*buffer).data };
        data[..chunk].copy_from_slice(&self.data[self.data_sent..self.data_sent + chunk]);
        let address = data.as_ptr() as usize;
        mmu::clean_dcache_range(address, EP0_PACKET);
        self.chunk = chunk;
        write(in_reg(0, DXEPTSIZ), 1 << DXEPTSIZ_PKTCNT_SHIFT | chunk as u32);
        write(in_reg(0, DXEPDMA), address as u32);
        modify(in_reg(0, 0), DXEPCTL_STALL, DXEPCTL_EPENA | DXEPCTL_CNAK);
    }

    fn status_in(&mut self) {
        self.stage = Stage::StatusIn;
        self.data_len = 0;
        self.data_sent = 0;
        self.zlp = false;
        self.send_ep0_packet();
    }

    fn ep0_in_done(&mut self) {
        if self.stage != Stage::DataIn {
            self.stage = Stage::Idle;
            return;
        }
        self.data_sent += self.chunk;
        if self.data_sent < self.data_len {
            self.send_ep0_packet();
        } else if self.zlp {
            self.zlp = false;
            self.send_ep0_packet();
        } else {
            // The host's zero-length status packet lands in the armed SETUP buffer.
            self.stage = Stage::Idle;
        }
    }

    /// Answers the pending [`Event::ClassRequest`] with `data`, cut to what the host asked.
    pub fn control_in(&mut self, data: &[u8]) {
        if self.stage != Stage::Class {
            return;
        }
        let asked = self.data_len;
        let len = data.len().min(EP0_DATA);
        self.data[..len].copy_from_slice(&data[..len]);
        self.reply(len, asked);
    }

    /// Accepts the pending [`Event::ClassRequest`], which has no data stage.
    pub fn control_ack(&mut self) {
        if self.stage == Stage::Class {
            self.status_in();
        }
    }

    /// Refuses the pending request: endpoint 0 stalls until the next SETUP.
    pub fn control_stall(&mut self) {
        modify(in_reg(0, 0), 0, DXEPCTL_STALL);
        modify(out_reg(0, 0), 0, DXEPCTL_STALL);
        self.stage = Stage::Idle;
    }
}

impl Drop for Otg {
    /// Lets D+ go, stops the core's interrupts and transfers, and turns PHY0 off.
    fn drop(&mut self) {
        self.disconnect();
        write(GINTMSK, 0);
        write(GAHBCFG, 0);
        self.stop_all();
        usb_phy::power_off();
        TAKEN.store(false, Ordering::Relaxed);
    }
}
