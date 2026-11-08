//! The EHCI host controller at [`soc::EHCI_BASE`], on port 0: the micro-USB port, PHY0 in host
//! mode. One high-speed device at a time, plugged in directly (through an OTG adapter): control
//! transfers on its endpoint 0, bulk transfers on one IN and one OUT endpoint. Full- and
//! low-speed devices belong to the companion OHCI, which is not driven here.
//!
//! The controller follows the async schedule, a ring of three queue heads (control, bulk IN,
//! bulk OUT) that stays in place. A transfer is a chain of transfer descriptors hung on its
//! queue head while that is idle (EHCI 1.0, 4.10.2); the controller keeps the data toggles of
//! the bulk endpoints in the queue heads. Queue heads and descriptors live in DRAM seen through
//! the uncached window ([`mmu::DRAM_UNCACHED`]) once the MMU is on, so the CPU and the
//! controller agree on them without cache maintenance; data buffers get it.
//!
//! Registers, descriptors and the order of operations: the Enhanced Host Controller Interface
//! Specification 1.0, cited by section. The one Exynos setting, the DMA bursts of `INSNREG00`,
//! comes from Linux 5.15.198 `drivers/usb/host/ehci-exynos.c:26-33,228-229`.

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::timer::Mct;
use crate::drivers::usb_phy::{self, Mode};
use crate::usb::{Setup, Speed};
use crate::{mmu, sleep, soc};

// 2.2: capability registers.
const CAPLENGTH: usize = 0x00;
const HCSPARAMS: usize = 0x04;
const HCCPARAMS: usize = 0x08;
/// 2.2.3: the port power control bit: ports have power switches.
const HCSPARAMS_PPC: u32 = 1 << 4;
// 2.3: operational registers, after CAPLENGTH.
const USBCMD: usize = 0x00;
const USBCMD_RUN: u32 = 1 << 0;
const USBCMD_HCRESET: u32 = 1 << 1;
const USBCMD_ASE: u32 = 1 << 5;
/// Interrupt threshold, one micro-frame.
const USBCMD_ITC_1: u32 = 1 << 16;
const USBSTS: usize = 0x04;
const USBSTS_USBINT: u32 = 1 << 0;
const USBSTS_USBERRINT: u32 = 1 << 1;
const USBSTS_PCD: u32 = 1 << 2;
const USBSTS_HSE: u32 = 1 << 4;
const USBSTS_HALTED: u32 = 1 << 12;
const USBSTS_ASS: u32 = 1 << 15;
const USBINTR: usize = 0x08;
const CTRLDSSEGMENT: usize = 0x10;
const PERIODICLISTBASE: usize = 0x14;
const ASYNCLISTADDR: usize = 0x18;
const CONFIGFLAG: usize = 0x40;
const PORTSC: usize = 0x44;
// 2.3.9: port status and control.
const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_CSC: u32 = 1 << 1;
const PORTSC_PE: u32 = 1 << 2;
const PORTSC_PEC: u32 = 1 << 3;
const PORTSC_OCC: u32 = 1 << 5;
const PORTSC_PR: u32 = 1 << 8;
const PORTSC_LS_SHIFT: u32 = 10;
const PORTSC_PP: u32 = 1 << 12;
const PORTSC_OWNER: u32 = 1 << 13;
/// The bits a write of 1 clears: written as 0 unless meant.
const PORTSC_CHANGES: u32 = PORTSC_CSC | PORTSC_PEC | PORTSC_OCC;
/// Line state K: a low-speed device (2.3.9, table 2-16).
const LINE_K: u32 = 1;
/// ehci-exynos.c:26-33: INCR16, INCR8, INCR4 bursts and their alignment.
const INSNREG00: usize = 0x90;
const INSNREG00_BURSTS: u32 = 0xF << 22;

// 3.5: transfer descriptor token.
const TOKEN_ACTIVE: u32 = 1 << 7;
const TOKEN_HALTED: u32 = 1 << 6;
const TOKEN_BUFFER_ERROR: u32 = 1 << 5;
const TOKEN_BABBLE: u32 = 1 << 4;
const TOKEN_XACT_ERROR: u32 = 1 << 3;
const TOKEN_PID_SHIFT: u32 = 8;
const PID_OUT: u32 = 0;
const PID_IN: u32 = 1;
const PID_SETUP: u32 = 2;
const TOKEN_CERR_3: u32 = 3 << 10;
const TOKEN_IOC: u32 = 1 << 15;
const TOKEN_BYTES_SHIFT: u32 = 16;
const TOKEN_BYTES_MASK: u32 = 0x7FFF;
const TOKEN_TOGGLE: u32 = 1 << 31;
/// 3.1: the terminate bit of a link pointer.
const TERMINATE: u32 = 1;
/// 3.1, table 3-1: the type of a link to a queue head.
const TYPE_QH: u32 = 1 << 1;
// 3.6.2: queue head endpoint characteristics and capabilities.
const CHARS_ENDPOINT_SHIFT: u32 = 8;
const CHARS_EPS_HIGH: u32 = 2 << 12;
const CHARS_DTC: u32 = 1 << 14;
const CHARS_HEAD: u32 = 1 << 15;
const CHARS_MPS_SHIFT: u32 = 16;
const CAPS_MULT_1: u32 = 1 << 30;

/// 3.5: a descriptor's five buffer pages.
const PAGE: usize = 4096;
/// What one descriptor carries here: four pages, from any offset in the first, and a whole
/// number of packets of any size up to 512.
const QTD_BYTES: usize = 16 * 1024;
/// Descriptors in the pool: one transfer moves up to this many [`QTD_BYTES`].
const QTDS: usize = 32;
/// The control data stage's own buffer, and the bounce buffer of small bulk transfers.
pub const SMALL_BYTES: usize = 1024;

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const REGISTER_TIMEOUT_US: u64 = 20_000;
/// USB 2.0, 7.1.7.5: a root port's reset lasts at least 50 ms.
const PORT_RESET_US: u64 = 50_000;
/// USB 2.0, 7.1.7.3: a new connection settles for 100 ms before the reset.
const DEBOUNCE_US: u64 = 100_000;
/// USB 2.0, 9.2.6.2: after the reset, 10 ms before the first request.
const RESET_RECOVERY_US: u64 = 10_000;
/// The port this board uses: 0, PHY0. Ports 1 and 2 are HSIC.
pub const PORT: usize = 0;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller is already taken: there is one.
    InUse,
    /// A register did not reach the state waited for: which, and its value.
    Register(&'static str, u32),
    /// Nothing is plugged in.
    NoDevice,
    /// The device is not high-speed: it belongs to the companion OHCI.
    NotHighSpeed(Speed),
    /// The endpoint answered STALL.
    Stall,
    /// The device sent more than asked.
    Babble,
    /// Three transaction errors in a row: CRC, timeout or bad PID.
    Transaction,
    /// The controller could not keep up with memory.
    DataBuffer,
    /// The controller stopped on a system error: `USBSTS`.
    HostError(u32),
    /// No answer in time.
    Timeout,
    /// A transfer longer than one call takes, or a buffer the DMA cannot use.
    BadBuffer,
    /// The endpoint's queue head was busy.
    Busy,
}

/// What the controller is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// `HCIVERSION` and `CAPLENGTH`, the first word.
    pub version: u32,
    /// `HCSPARAMS`: the number of ports in bits 0-3.
    pub hcsparams: u32,
    /// `HCCPARAMS`.
    pub hccparams: u32,
}

/// A port, as `PORTSC` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    /// `PORTSC`.
    pub raw: u32,
}

impl PortStatus {
    /// Something is plugged in.
    pub const fn connected(&self) -> bool {
        self.raw & PORTSC_CCS != 0
    }
    /// The port is enabled: a high-speed device after its reset.
    pub const fn enabled(&self) -> bool {
        self.raw & PORTSC_PE != 0
    }
    /// The connection changed since this was last cleared.
    pub const fn connect_changed(&self) -> bool {
        self.raw & PORTSC_CSC != 0
    }
    /// D+ and D-: 0 SE0, 1 K, 2 J.
    pub const fn line_state(&self) -> u32 {
        (self.raw >> PORTSC_LS_SHIFT) & 3
    }
}

/// The three endpoints there are queue heads for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipe {
    /// Endpoint 0.
    Control = 0,
    /// The bulk IN endpoint.
    BulkIn = 1,
    /// The bulk OUT endpoint.
    BulkOut = 2,
}

/// 3.5: a transfer descriptor; 32 bytes, on 32.
#[repr(C, align(32))]
struct Qtd {
    next: u32,
    alternate: u32,
    token: u32,
    buffer: [u32; 5],
}

/// 3.6: a queue head, its overlay included; on 32, given 64.
#[repr(C, align(64))]
struct Qh {
    link: u32,
    characteristics: u32,
    capabilities: u32,
    current: u32,
    next: u32,
    alternate: u32,
    token: u32,
    buffer: [u32; 5],
    extended: [u32; 5],
}

#[repr(C, align(4096))]
struct Pool {
    qh: [Qh; 3],
    qtd: [Qtd; QTDS],
    /// Where a short packet sends the controller: inactive, so the queue stops.
    stop: Qtd,
    setup: [u8; 32],
    small: [u8; SMALL_BYTES],
}

struct Shared(UnsafeCell<Pool>);

// SAFETY: only the one `Ehci` (see `TAKEN`) touches it, and the controller, which it runs.
unsafe impl Sync for Shared {}

const QTD_ZERO: Qtd = Qtd { next: TERMINATE, alternate: TERMINATE, token: 0, buffer: [0; 5] };
const QH_ZERO: Qh = Qh {
    link: TERMINATE,
    characteristics: 0,
    capabilities: 0,
    current: 0,
    next: TERMINATE,
    alternate: TERMINATE,
    token: 0,
    buffer: [0; 5],
    extended: [0; 5],
};
static POOL: Shared = Shared(UnsafeCell::new(Pool {
    qh: [QH_ZERO, QH_ZERO, QH_ZERO],
    qtd: [QTD_ZERO; QTDS],
    stop: QTD_ZERO,
    setup: [0; 32],
    small: [0; SMALL_BYTES],
}));
static TAKEN: AtomicBool = AtomicBool::new(false);

const _: () = assert!(core::mem::size_of::<Pool>() <= mmu::DRAM_UNCACHED_SIZE);

/// Orders the CPU's writes to the descriptors before what follows, and the other way.
fn barrier() {
    // SAFETY: a barrier only orders memory accesses.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
}

fn wait_for(mct: Mct, timeout_us: u64, done: impl Fn() -> bool) -> bool {
    let begin = mct.count();
    while !done() {
        if mct.count() - begin > timeout_us * COUNTS_PER_US {
            return false;
        }
    }
    true
}

/// The host side. There is one: [`Ehci::new`] fails the second time.
pub struct Ehci {
    mct: Mct,
    op: usize,
    info: Info,
    /// The pool as the controller addresses it, and as the CPU does.
    phys: usize,
    cpu: usize,
    /// The bulk pipes' endpoint numbers, IN and OUT.
    bulk: (u8, u8),
}

impl Ehci {
    /// Takes the controller: PHY0 on in host mode, the controller reset and running with an
    /// empty async schedule, every port routed to it and powered. The MCT must run, and VBUS
    /// must be on, which is the board's job (the boost on `gpx0-6`).
    pub fn new(mct: Mct) -> Result<Self, Error> {
        if TAKEN.load(Ordering::Relaxed) {
            return Err(Error::InUse);
        }
        usb_phy::power_on(Mode::Host, mct);
        let phys = POOL.0.get() as usize;
        let cpu = if mmu::is_enabled() { mmu::uncached(phys).ok_or(Error::BadBuffer)? } else { phys };
        let version = read(CAPLENGTH);
        let ehci = Self {
            mct,
            op: soc::EHCI_BASE + (version & 0xFF) as usize,
            info: Info { version, hcsparams: read(HCSPARAMS), hccparams: read(HCCPARAMS) },
            phys,
            cpu,
            bulk: (0, 0),
        };
        write(INSNREG00, INSNREG00_BURSTS);

        // 4.1: stop, reset.
        ehci.op_modify(USBCMD, USBCMD_RUN, 0);
        if !wait_for(mct, REGISTER_TIMEOUT_US, || ehci.op_read(USBSTS) & USBSTS_HALTED != 0) {
            return Err(Error::Register("USBSTS halted", ehci.op_read(USBSTS)));
        }
        ehci.op_write(USBCMD, USBCMD_HCRESET);
        if !wait_for(mct, REGISTER_TIMEOUT_US, || ehci.op_read(USBCMD) & USBCMD_HCRESET == 0) {
            return Err(Error::Register("USBCMD reset", ehci.op_read(USBCMD)));
        }

        // The ring of queue heads, the control one the head of the reclamation list.
        for pipe in [Pipe::Control, Pipe::BulkIn, Pipe::BulkOut] {
            let next = ehci.qh_phys((pipe as usize + 1) % 3);
            let qh = ehci.qh(pipe);
            // SAFETY: the controller is halted; the pool is ours.
            unsafe {
                ptr::write_volatile(ptr::addr_of_mut!((*qh).link), next as u32 | TYPE_QH);
                ptr::write_volatile(ptr::addr_of_mut!((*qh).characteristics), CHARS_EPS_HIGH | 64 << CHARS_MPS_SHIFT);
                ptr::write_volatile(ptr::addr_of_mut!((*qh).capabilities), CAPS_MULT_1);
            }
            ehci.reset_overlay(pipe, false);
        }
        ehci.configure(Pipe::Control, 0, 0, 64);
        barrier();

        ehci.op_write(CTRLDSSEGMENT, 0);
        ehci.op_write(PERIODICLISTBASE, 0);
        ehci.op_write(ASYNCLISTADDR, ehci.qh_phys(0) as u32);
        ehci.op_write(USBSTS, u32::MAX);
        ehci.op_write(USBINTR, USBSTS_USBINT | USBSTS_USBERRINT | USBSTS_PCD | USBSTS_HSE);
        ehci.op_write(USBCMD, USBCMD_ITC_1 | USBCMD_ASE | USBCMD_RUN);
        if !wait_for(mct, REGISTER_TIMEOUT_US, || ehci.op_read(USBSTS) & (USBSTS_HALTED | USBSTS_ASS) == USBSTS_ASS) {
            return Err(Error::Register("USBSTS running", ehci.op_read(USBSTS)));
        }
        // 4.1: routes the ports here, from the companion.
        ehci.op_write(CONFIGFLAG, 1);
        if ehci.info.hcsparams & HCSPARAMS_PPC != 0 {
            for port in 0..ehci.ports() {
                ehci.port_modify(port, 0, PORTSC_PP);
            }
        }
        TAKEN.store(true, Ordering::Relaxed);
        Ok(ehci)
    }

    /// What the controller is.
    pub const fn info(&self) -> Info {
        self.info
    }

    /// The MCT it waits with.
    pub const fn mct(&self) -> Mct {
        self.mct
    }

    /// The endpoint numbers [`set_bulk`](Self::set_bulk) gave, IN and OUT.
    pub const fn bulk_endpoints(&self) -> (u8, u8) {
        self.bulk
    }

    /// How many root ports it has.
    pub const fn ports(&self) -> usize {
        (self.info.hcsparams & 0xF) as usize
    }

    fn op_read(&self, offset: usize) -> u32 {
        // SAFETY: the operational registers, device memory mapped for the whole program.
        unsafe { ptr::read_volatile((self.op + offset) as *const u32) }
    }

    fn op_write(&self, offset: usize, value: u32) {
        // SAFETY: as in `op_read`.
        unsafe { ptr::write_volatile((self.op + offset) as *mut u32, value) }
    }

    fn op_modify(&self, offset: usize, clear: u32, set: u32) {
        self.op_write(offset, (self.op_read(offset) & !clear) | set);
    }

    fn port_modify(&self, port: usize, clear: u32, set: u32) {
        let offset = PORTSC + 4 * port;
        self.op_write(offset, (self.op_read(offset) & !(clear | PORTSC_CHANGES)) | set);
    }

    /// The port's state.
    pub fn port_status(&self, port: usize) -> PortStatus {
        PortStatus { raw: self.op_read(PORTSC + 4 * port) }
    }

    /// Clears the port's change bits.
    pub fn clear_port_changes(&self, port: usize) {
        let offset = PORTSC + 4 * port;
        self.op_write(offset, self.op_read(offset));
    }

    /// `USBSTS`.
    pub fn status(&self) -> u32 {
        self.op_read(USBSTS)
    }

    /// Sleeps until the port's connection changes, or until the MCT counter reaches
    /// `deadline`; whether something is plugged in then.
    pub fn wait_port(&self, port: usize, deadline: Option<u64>) -> bool {
        let was = self.port_status(port).connected();
        sleep::until(self.mct, &[soc::EHCI_IRQ], deadline, || {
            self.op_write(USBSTS, USBSTS_PCD);
            self.port_status(port).connected() != was
        });
        self.port_status(port).connected()
    }

    /// Resets the device on `port` after the connection settles, and enables the port if it
    /// is high-speed (7.1.7.5 of USB 2.0, 4.2.2 here). Otherwise hands the port to the
    /// companion and fails with its speed. The device answers at address 0 afterwards.
    pub fn reset_port(&mut self, port: usize) -> Result<Speed, Error> {
        sleep::for_us(self.mct, DEBOUNCE_US);
        let status = self.port_status(port);
        if !status.connected() {
            return Err(Error::NoDevice);
        }
        self.clear_port_changes(port);
        if status.line_state() == LINE_K {
            self.port_modify(port, 0, PORTSC_OWNER);
            return Err(Error::NotHighSpeed(Speed::Low));
        }
        self.port_modify(port, PORTSC_PE, PORTSC_PR);
        sleep::for_us(self.mct, PORT_RESET_US);
        self.port_modify(port, PORTSC_PR, 0);
        let offset = PORTSC + 4 * port;
        if !wait_for(self.mct, REGISTER_TIMEOUT_US, || self.op_read(offset) & PORTSC_PR == 0) {
            return Err(Error::Register("PORTSC reset", self.op_read(offset)));
        }
        // The high-speed chirp enables the port within 2 ms.
        sleep::for_us(self.mct, 2_000);
        let status = self.port_status(port);
        if !status.connected() {
            return Err(Error::NoDevice);
        }
        if !status.enabled() {
            self.port_modify(port, 0, PORTSC_OWNER);
            return Err(Error::NotHighSpeed(Speed::Full));
        }
        sleep::for_us(self.mct, RESET_RECOVERY_US);
        Ok(Speed::High)
    }

    // The pool.

    fn qh(&self, pipe: Pipe) -> *mut Qh {
        // SAFETY: an address inside the pool, `cpu` its view.
        unsafe { ptr::addr_of_mut!((*(self.cpu as *mut Pool)).qh[pipe as usize]) }
    }

    fn qh_phys(&self, index: usize) -> usize {
        self.phys + index * core::mem::size_of::<Qh>()
    }

    fn qtd(&self, index: usize) -> *mut Qtd {
        // SAFETY: as in `qh`.
        unsafe { ptr::addr_of_mut!((*(self.cpu as *mut Pool)).qtd[index]) }
    }

    fn to_phys(&self, cpu: usize) -> usize {
        cpu - self.cpu + self.phys
    }

    fn stop_phys(&self) -> u32 {
        // SAFETY: as in `qh`.
        self.to_phys(unsafe { ptr::addr_of_mut!((*(self.cpu as *mut Pool)).stop) } as usize) as u32
    }

    fn setup_buffer(&self) -> *mut u8 {
        // SAFETY: as in `qh`.
        unsafe { ptr::addr_of_mut!((*(self.cpu as *mut Pool)).setup) as *mut u8 }
    }

    fn small_buffer(&self) -> *mut u8 {
        // SAFETY: as in `qh`.
        unsafe { ptr::addr_of_mut!((*(self.cpu as *mut Pool)).small) as *mut u8 }
    }

    /// Turns the async schedule off for `edit`, then on again: the controller may hold a
    /// queue head's fields while it runs (4.8.2).
    fn with_schedule_off(&mut self, edit: impl FnOnce(&mut Self)) {
        self.op_modify(USBCMD, USBCMD_ASE, 0);
        wait_for(self.mct, REGISTER_TIMEOUT_US, || self.op_read(USBSTS) & USBSTS_ASS == 0);
        edit(self);
        barrier();
        self.op_modify(USBCMD, 0, USBCMD_ASE);
        wait_for(self.mct, REGISTER_TIMEOUT_US, || self.op_read(USBSTS) & USBSTS_ASS != 0);
    }

    /// Points a queue head at `address`, `endpoint`, packets of `packet` bytes. The control
    /// one takes its toggles from the descriptors and heads the list.
    fn configure(&self, pipe: Pipe, address: u8, endpoint: u8, packet: u16) {
        let mut chars = u32::from(address & 0x7F)
            | u32::from(endpoint & 0x0F) << CHARS_ENDPOINT_SHIFT
            | CHARS_EPS_HIGH
            | u32::from(packet & 0x7FF) << CHARS_MPS_SHIFT;
        if pipe == Pipe::Control {
            chars |= CHARS_DTC | CHARS_HEAD;
        }
        // SAFETY: as in `qh`; the schedule is off or not yet running.
        unsafe { ptr::write_volatile(ptr::addr_of_mut!((*self.qh(pipe)).characteristics), chars) };
    }

    /// Empties the queue head's overlay: idle, nothing next, toggle DATA0 unless `keep_toggle`.
    fn reset_overlay(&self, pipe: Pipe, keep_toggle: bool) {
        let qh = self.qh(pipe);
        // SAFETY: as in `qh`; the controller does not work on this queue head now.
        unsafe {
            let toggle = if keep_toggle { ptr::read_volatile(ptr::addr_of!((*qh).token)) & TOKEN_TOGGLE } else { 0 };
            ptr::write_volatile(ptr::addr_of_mut!((*qh).current), 0);
            ptr::write_volatile(ptr::addr_of_mut!((*qh).next), TERMINATE);
            ptr::write_volatile(ptr::addr_of_mut!((*qh).alternate), TERMINATE);
            ptr::write_volatile(ptr::addr_of_mut!((*qh).token), toggle);
        }
    }

    /// Sets the device's address and endpoint 0's packet size, for the control pipe.
    pub fn set_control(&mut self, address: u8, packet: u16) {
        self.with_schedule_off(|ehci| ehci.configure(Pipe::Control, address, 0, packet));
    }

    /// Sets the bulk pipes: the device's address, the IN and OUT endpoints' numbers and packet
    /// sizes. Both start from DATA0.
    pub fn set_bulk(&mut self, address: u8, input: (u8, u16), output: (u8, u16)) {
        self.bulk = (input.0, output.0);
        self.with_schedule_off(|ehci| {
            ehci.configure(Pipe::BulkIn, address, input.0, input.1);
            ehci.configure(Pipe::BulkOut, address, output.0, output.1);
            ehci.reset_overlay(Pipe::BulkIn, false);
            ehci.reset_overlay(Pipe::BulkOut, false);
        });
    }

    /// Starts a bulk pipe from DATA0 again, as the device does after `CLEAR_FEATURE`
    /// (`ENDPOINT_HALT`).
    pub fn reset_toggle(&mut self, pipe: Pipe) {
        self.with_schedule_off(|ehci| ehci.reset_overlay(pipe, false));
    }

    // Transfers.

    /// Fills descriptor `index` for `len` bytes at controller address `address`.
    fn fill(&self, index: usize, pid: u32, toggle: bool, address: usize, len: usize, alternate: u32) {
        let qtd = self.qtd(index);
        let mut buffer = [0u32; 5];
        buffer[0] = address as u32;
        for (page, slot) in buffer.iter_mut().enumerate().skip(1) {
            *slot = ((address & !(PAGE - 1)) + page * PAGE) as u32;
        }
        let token = TOKEN_ACTIVE
            | TOKEN_CERR_3
            | pid << TOKEN_PID_SHIFT
            | (len as u32) << TOKEN_BYTES_SHIFT
            | if toggle { TOKEN_TOGGLE } else { 0 };
        // SAFETY: as in `qh`; descriptors are not on any queue head between transfers.
        unsafe {
            ptr::write_volatile(ptr::addr_of_mut!((*qtd).next), TERMINATE);
            ptr::write_volatile(ptr::addr_of_mut!((*qtd).alternate), alternate);
            ptr::write_volatile(ptr::addr_of_mut!((*qtd).buffer), buffer);
            ptr::write_volatile(ptr::addr_of_mut!((*qtd).token), token);
        }
    }

    fn link(&self, from: usize, to: usize) {
        let next = self.to_phys(self.qtd(to) as usize) as u32;
        // SAFETY: as in `fill`.
        unsafe { ptr::write_volatile(ptr::addr_of_mut!((*self.qtd(from)).next), next) };
    }

    fn token(&self, index: usize) -> u32 {
        // SAFETY: as in `qh`.
        unsafe { ptr::read_volatile(ptr::addr_of!((*self.qtd(index)).token)) }
    }

    fn alternate(&self, index: usize) -> u32 {
        // SAFETY: as in `qh`; the controller does not write this field.
        unsafe { ptr::read_volatile(ptr::addr_of!((*self.qtd(index)).alternate)) }
    }

    /// Bytes descriptor `index` did not move.
    fn left(&self, index: usize) -> usize {
        ((self.token(index) >> TOKEN_BYTES_SHIFT) & TOKEN_BYTES_MASK) as usize
    }

    /// Hangs descriptors `0..count` (linked, the last gets IOC) on `pipe` and waits until the
    /// queue is done: the last descriptor retired, or a short IN one that stops it. A halt or a
    /// timeout leaves the queue head emptied, keeping its toggle. The tokens stay for the caller
    /// to read; [`clear`](Self::clear) them afterwards.
    fn run(&mut self, pipe: Pipe, count: usize, timeout_ms: u64) -> Result<(), Error> {
        // SAFETY: as in `fill`.
        unsafe {
            let last = self.qtd(count - 1);
            let token = ptr::read_volatile(ptr::addr_of!((*last).token));
            ptr::write_volatile(ptr::addr_of_mut!((*last).token), token | TOKEN_IOC);
        }
        let qh = self.qh(pipe);
        // SAFETY: as in `qh`.
        let overlay = unsafe { ptr::read_volatile(ptr::addr_of!((*qh).token)) };
        if overlay & (TOKEN_ACTIVE | TOKEN_HALTED) != 0 {
            self.clear(count);
            return Err(Error::Busy);
        }
        barrier();
        let first = self.to_phys(self.qtd(0) as usize) as u32;
        // SAFETY: as in `qh`; an idle queue head takes the next pointer as it comes (4.10.2).
        unsafe { ptr::write_volatile(ptr::addr_of_mut!((*qh).next), first) };
        barrier();

        let stop = self.stop_phys();
        let deadline = self.mct.count() + timeout_ms * 1000 * COUNTS_PER_US;
        let mut outcome = Err(Error::Timeout);
        let mut short = false;
        sleep::until(self.mct, &[soc::EHCI_IRQ], Some(deadline), || {
            self.op_write(USBSTS, USBSTS_USBINT | USBSTS_USBERRINT);
            let status = self.op_read(USBSTS);
            if status & (USBSTS_HSE | USBSTS_HALTED) != 0 {
                outcome = Err(Error::HostError(status));
                return true;
            }
            for index in 0..count {
                let token = self.token(index);
                if token & TOKEN_ACTIVE != 0 {
                    return false;
                }
                if token & TOKEN_HALTED != 0 {
                    outcome = Err(if token & TOKEN_BABBLE != 0 {
                        Error::Babble
                    } else if token & TOKEN_BUFFER_ERROR != 0 {
                        Error::DataBuffer
                    } else if token & TOKEN_XACT_ERROR != 0 {
                        Error::Transaction
                    } else {
                        Error::Stall
                    });
                    return true;
                }
                if self.left(index) != 0 && self.alternate(index) == stop {
                    short = true;
                    break;
                }
            }
            outcome = Ok(());
            true
        });
        barrier();
        // After an error, or a short packet that left the overlay pointing at the stop
        // descriptor with bytes to go, the queue head would not take the next transfer.
        if outcome.is_err() || short {
            self.with_schedule_off(|ehci| ehci.reset_overlay(pipe, true));
        }
        outcome
    }

    /// Takes descriptors `0..count` off duty, the ones a short packet skipped too.
    fn clear(&self, count: usize) {
        for index in 0..count {
            // SAFETY: as in `fill`.
            unsafe { ptr::write_volatile(ptr::addr_of_mut!((*self.qtd(index)).token), 0) };
        }
    }

    /// A control transfer: `setup`, then a data stage to the device from `data` or from it
    /// into `data`, as the request says, up to its `length` and [`SMALL_BYTES`]. Returns the
    /// bytes of the data stage.
    pub fn control(&mut self, setup: Setup, data: &mut [u8]) -> Result<usize, Error> {
        let len = usize::from(setup.length).min(data.len());
        if len > SMALL_BYTES {
            return Err(Error::BadBuffer);
        }
        let input = setup.is_in();
        let bytes = setup.to_bytes();
        let small = self.small_buffer();
        // SAFETY: the pool's SETUP and data buffers, which no descriptor uses now.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.setup_buffer(), 8);
            if !input {
                ptr::copy_nonoverlapping(data.as_ptr(), small, len);
            }
        }
        self.fill(0, PID_SETUP, false, self.to_phys(self.setup_buffer() as usize), 8, TERMINATE);
        let mut count = 1;
        if len != 0 {
            let pid = if input { PID_IN } else { PID_OUT };
            self.fill(1, pid, true, self.to_phys(small as usize), len, TERMINATE);
            self.link(0, 1);
            count = 2;
        }
        // USB 2.0, 8.5.3: the status stage goes the other way, DATA1, no data.
        let status_pid = if input && len != 0 { PID_OUT } else { PID_IN };
        self.fill(count, status_pid, true, 0, 0, TERMINATE);
        self.link(count - 1, count);
        if count == 2 && input {
            // A short data stage goes on to the status stage.
            let status = self.to_phys(self.qtd(2) as usize) as u32;
            // SAFETY: as in `fill`.
            unsafe { ptr::write_volatile(ptr::addr_of_mut!((*self.qtd(1)).alternate), status) };
        }
        count += 1;
        let result = self.run(Pipe::Control, count, 1000);
        let moved = if len != 0 { len - self.left(1) } else { 0 };
        self.clear(count);
        result?;
        if input {
            // SAFETY: as above; the controller is done with it.
            unsafe { ptr::copy_nonoverlapping(small, data.as_mut_ptr(), moved) };
        }
        Ok(moved)
    }

    /// Reads from the bulk IN endpoint into `buffer`, until it is full or the device ends with
    /// a short packet: the bytes read. A buffer on a cache line and of whole cache lines goes
    /// straight to the DMA; another one through the pool, up to [`SMALL_BYTES`].
    pub fn bulk_in(&mut self, buffer: &mut [u8], timeout_ms: u64) -> Result<usize, Error> {
        let address = buffer.as_ptr() as usize;
        let direct = address.is_multiple_of(mmu::DCACHE_LINE) && buffer.len().is_multiple_of(mmu::DCACHE_LINE);
        if !direct {
            if buffer.len() > SMALL_BYTES {
                return Err(Error::BadBuffer);
            }
            let small = self.small_buffer();
            let moved = self.bulk_once(PID_IN, self.to_phys(small as usize), buffer.len(), timeout_ms)?;
            // SAFETY: the pool's data buffer; the controller is done with it.
            unsafe { ptr::copy_nonoverlapping(small, buffer.as_mut_ptr(), moved) };
            return Ok(moved);
        }
        let mut done = 0;
        while done < buffer.len() {
            let chunk = (buffer.len() - done).min(QTDS * QTD_BYTES);
            let start = address + done;
            mmu::discard_dcache_range(start, chunk);
            let result = self.bulk_once(PID_IN, start, chunk, timeout_ms);
            mmu::invalidate_dcache_range(start, chunk);
            let moved = result?;
            done += moved;
            if moved < chunk {
                break;
            }
        }
        Ok(done)
    }

    /// Writes `data` to the bulk OUT endpoint. Ends on a packet boundary without a
    /// zero-length packet; empty `data` sends one.
    pub fn bulk_out(&mut self, data: &[u8], timeout_ms: u64) -> Result<(), Error> {
        let address = data.as_ptr() as usize;
        if data.len() <= SMALL_BYTES && !address.is_multiple_of(4) {
            let small = self.small_buffer();
            // SAFETY: the pool's data buffer, which no descriptor uses now.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), small, data.len()) };
            self.bulk_once(PID_OUT, self.to_phys(small as usize), data.len(), timeout_ms)?;
            return Ok(());
        }
        mmu::clean_dcache_range(address, data.len());
        let mut done = 0;
        loop {
            let chunk = (data.len() - done).min(QTDS * QTD_BYTES);
            self.bulk_once(PID_OUT, address + done, chunk, timeout_ms)?;
            done += chunk;
            if done >= data.len() {
                return Ok(());
            }
        }
    }

    /// One queue of descriptors over `len` bytes at controller address `address`.
    fn bulk_once(&mut self, pid: u32, address: usize, len: usize, timeout_ms: u64) -> Result<usize, Error> {
        let pipe = if pid == PID_IN { Pipe::BulkIn } else { Pipe::BulkOut };
        let alternate = if pid == PID_IN { self.stop_phys() } else { TERMINATE };
        let count = len.div_ceil(QTD_BYTES).max(1);
        for index in 0..count {
            let offset = index * QTD_BYTES;
            self.fill(index, pid, false, address + offset, (len - offset).min(QTD_BYTES), alternate);
            if index > 0 {
                self.link(index - 1, index);
            }
        }
        let result = self.run(pipe, count, timeout_ms);
        let mut moved = 0;
        for index in 0..count {
            let asked = (len - index * QTD_BYTES).min(QTD_BYTES);
            let left = self.left(index);
            moved += asked - left;
            if left != 0 {
                break;
            }
        }
        self.clear(count);
        result.map(|()| moved)
    }
}

impl Drop for Ehci {
    /// Stops the controller, gives the ports back to the companion and turns PHY0 off.
    fn drop(&mut self) {
        self.op_write(USBINTR, 0);
        self.op_modify(USBCMD, USBCMD_RUN | USBCMD_ASE, 0);
        wait_for(self.mct, REGISTER_TIMEOUT_US, || self.op_read(USBSTS) & USBSTS_HALTED != 0);
        self.op_write(CONFIGFLAG, 0);
        usb_phy::power_off();
        TAKEN.store(false, Ordering::Relaxed);
    }
}

fn read(offset: usize) -> u32 {
    // SAFETY: the capability registers, device memory mapped for the whole program; the
    // `usb_host` gate is open from `Ehci::new` on.
    unsafe { ptr::read_volatile((soc::EHCI_BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as in `read`.
    unsafe { ptr::write_volatile((soc::EHCI_BASE + offset) as *mut u32, value) }
}
