//! ARM PL330 DMA controller: `pdma0`, the one with the audio requests. Its channels run small
//! programs of the controller's own instructions, which [`Program`] assembles.
//!
//! Register offsets, instruction encodings and the start through the debug registers are those
//! of Linux 5.15.198 `drivers/dma/pl330.c`, cited as `pl330.c:<line>`. Nothing here takes code
//! from it: the encodings are the instruction set of the controller.
//!
//! A channel starts with `DMAGO`, which only the manager runs: [`Pl330::start`] hands it to the
//! manager through the debug registers (`pl330.c:890-914`). A program that ends with
//! [`Program::end`] stops the channel; one that loops forever runs until [`Pl330::kill`].
//!
//! `DMASEV` in a program raises an event; with the event's bit in `INTEN`, it is an interrupt
//! instead, which shows in `INTSTATUS` until cleared: [`Pl330::take_event`]. The events of all
//! channels share one GIC interrupt, [`PDMA0_IRQ`], level-triggered while any is set.
//!
//! # Waiting for an event
//!
//! Without the `embassy` feature, or without [`init_interrupt`], the events are polled. With
//! them, `Pl330::wait_event` awaits one on the interrupt, as the async eMMC transfers do: the
//! handler turns the interrupt off at the GIC, whatever raised it, and wakes every waiter; each
//! looks at its own event, and turns the interrupt on again if it still waits. A wait does not
//! clear its event: [`Pl330::take_event`] does, after it. An event enabled in `INTEN` that no
//! one takes keeps the line high, and wakes the waiters each time one turns it on again.
//!
//! # Security
//!
//! The manager runs secure or non-secure as the boot set it (`CR0`, `pl330.c:1850-1854`). A
//! non-secure manager runs non-secure channels only, so [`Pl330::start`] and [`ccr`] follow it
//! (`pl330.c:1520-1525`).

use core::ptr;

use crate::drivers::timer::Mct;
use crate::soc;

/// `pdma0`: `exynos4.dtsi:672-681`.
pub const PDMA0_BASE: usize = 0x1268_0000;
/// `exynos4.dtsi:675`, SPI 35.
pub const PDMA0_IRQ: u16 = 32 + 35;

/// `pl330.c:59`, and the states of `pl330.c:60-71`.
const DS: usize = 0x0;
/// `pl330.c:74-77`.
const INTEN: usize = 0x20;
const INTSTATUS: usize = 0x28;
const INTCLR: usize = 0x2C;
/// `pl330.c:78-79`: the manager's and the channels' fault bits.
const FSM: usize = 0x30;
const FSC: usize = 0x34;
/// `pl330.c:80,82-83`.
const FTM: usize = 0x38;
const FTC: usize = 0x40;
/// `pl330.c:85-90`.
const CS: usize = 0x100;
const CPC: usize = 0x104;
/// `pl330.c:92-96`: a channel's source and destination addresses now.
const SA: usize = 0x400;
const DA: usize = 0x404;
/// `pl330.c:125-130`.
const DBGSTATUS: usize = 0xD00;
const DBG_BUSY: u32 = 1 << 0;
const DBGCMD: usize = 0xD04;
const DBGINST0: usize = 0xD08;
const DBGINST1: usize = 0xD0C;
/// `pl330.c:132`, and its fields at `pl330.c:146-154`, read as `pl330.c:1835-1859` does.
const CR0: usize = 0xE00;
const CR0_BOOT_MAN_NS: u32 = 1 << 2;

/// `pl330.c:197-215`.
const CMD_DMAEND: u8 = 0x00;
const CMD_DMAKILL: u8 = 0x01;
const CMD_DMALD: u8 = 0x04;
const CMD_DMALP: u8 = 0x20;
const CMD_DMALPEND: u8 = 0x28;
const CMD_DMASTP: u8 = 0x29;
const CMD_DMAWFP: u8 = 0x30;
const CMD_DMASEV: u8 = 0x34;
const CMD_DMAFLUSHP: u8 = 0x35;
const CMD_DMAGO: u8 = 0xA0;
const CMD_DMAMOV: u8 = 0xBC;
/// The loop counter bit of `DMALPEND`, set in `DMALPFE`: see [`Program::loop_forever`].
pub const LPFE_COUNTER_BIT: u8 = 1 << 2;
/// `pl330.c:338-342`.
const MOV_SAR: u8 = 0;
const MOV_CCR: u8 = 1;
const MOV_DAR: u8 = 2;

/// `pl330.c:101-117`.
const CC_SRCINC: u32 = 1 << 0;
const CC_DSTINC: u32 = 1 << 14;
const CC_SRCNS: u32 = 1 << 9;
const CC_DSTNS: u32 = 1 << 23;
const CC_SRCBRSTSIZE_SHIFT: u32 = 1;
const CC_DSTBRSTSIZE_SHIFT: u32 = 15;

/// How long the manager may stay busy with a debug instruction: `pl330.c:871-874` waits 5 ms.
const DBG_TIMEOUT_US: u64 = 5_000;

/// Why a call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The manager stayed busy with an earlier debug instruction.
    ManagerBusy,
    /// The program is not in DRAM the controller reads, or does not fit 32 bits.
    BadAddress,
}

/// A channel's state: `CS(n)` bits 3:0, `pl330.c:60-71`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Stopped.
    Stopped,
    /// Running instructions.
    Executing,
    /// Waiting for a peripheral's request (`DMAWFP`).
    WaitingForPeripheral,
    /// Faulted: see [`ChannelReport::fault`].
    Faulting,
    /// Any other state, raw.
    Other(u8),
}

/// What a channel holds now, for reports.
#[derive(Clone, Copy, Debug)]
pub struct ChannelReport {
    /// Its state.
    pub state: State,
    /// Its program counter.
    pub pc: u32,
    /// Its source address.
    pub source: u32,
    /// Its destination address.
    pub destination: u32,
    /// Its fault type register `FTC(n)`, if `FSC` says it faulted.
    pub fault: Option<u32>,
}

/// The controller's configuration from `CR0`, `pl330.c:1835-1859`.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// `CR0` raw.
    pub cr0: u32,
    /// Channels.
    pub channels: u8,
    /// Peripheral request lines, 0 if it has none.
    pub peripherals: u8,
    /// Events, each also an interrupt.
    pub events: u8,
    /// Whether the manager runs non-secure.
    pub manager_ns: bool,
}

/// The channel control word for a transfer of single beats of `1 << size_log2` bytes, the
/// source or destination address stepping after each when `src_inc` or `dst_inc`. Burst length
/// 1, no cache control (`pl330.c:1437-1466` with `CCTRL0`, `pl330.c:2541-2542`), non-secure
/// when the manager is.
pub const fn ccr(config: &Config, size_log2: u32, src_inc: bool, dst_inc: bool) -> u32 {
    let mut value = size_log2 << CC_SRCBRSTSIZE_SHIFT | size_log2 << CC_DSTBRSTSIZE_SHIFT;
    if src_inc {
        value |= CC_SRCINC;
    }
    if dst_inc {
        value |= CC_DSTINC;
    }
    if config.manager_ns {
        value |= CC_SRCNS | CC_DSTNS;
    }
    value
}

/// A channel program, assembled into a buffer the controller reads from DRAM.
///
/// Instructions are added in order; a loop starts with [`loop_start`](Self::loop_start), which
/// returns where its body begins, and ends with [`loop_end`](Self::loop_end) given that.
/// Assembling past the end of the buffer panics.
#[repr(C, align(32))]
pub struct Program<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

/// Where a loop's body starts, from [`Program::loop_start`] or [`Program::here`].
#[derive(Clone, Copy, Debug)]
pub struct Mark(usize);

impl<const N: usize> Program<N> {
    /// An empty program.
    pub const fn new() -> Self {
        Self { bytes: [0; N], len: 0 }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    /// The assembled bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The address of the first instruction.
    pub fn address(&self) -> usize {
        self.bytes.as_ptr() as usize
    }

    /// Writes the program out of the caches, for the controller to read.
    pub fn clean(&self) {
        crate::mmu::clean_dcache_range(self.address(), N);
    }

    /// Where the next instruction goes: the start of a loop without a counter.
    pub fn here(&self) -> Mark {
        Mark(self.len)
    }

    fn mov(&mut self, register: u8, value: u32) {
        let v = value.to_le_bytes();
        self.push(&[CMD_DMAMOV, register, v[0], v[1], v[2], v[3]]);
    }

    /// `DMAMOV SAR`: the source address (`pl330.c:718-735`).
    pub fn mov_source(&mut self, address: u32) {
        self.mov(MOV_SAR, address);
    }

    /// `DMAMOV DAR`: the destination address.
    pub fn mov_destination(&mut self, address: u32) {
        self.mov(MOV_DAR, address);
    }

    /// `DMAMOV CCR`: the channel control word, from [`ccr`].
    pub fn mov_control(&mut self, value: u32) {
        self.mov(MOV_CCR, value);
    }

    /// `DMAFLUSHP`: drops the stale requests of peripheral `request` (`pl330.c:592-606`).
    pub fn flush_peripheral(&mut self, request: u8) {
        self.push(&[CMD_DMAFLUSHP, (request & 0x1F) << 3]);
    }

    /// `DMALP`: repeats the body up to the matching [`loop_end`](Self::loop_end) `count` times,
    /// 1 to 256, on loop counter `counter`, 0 or 1 (`pl330.c:647-660`).
    pub fn loop_start(&mut self, counter: u8, count: u16) -> Mark {
        assert!(counter < 2 && (1..=256).contains(&count));
        self.push(&[CMD_DMALP | counter << 1, (count - 1) as u8]);
        self.here()
    }

    /// `DMALPEND`: the end of the loop on `counter` whose body starts at `start`
    /// (`pl330.c:673-706`; the jump is back to `start`, `pl330.c:1353`).
    pub fn loop_end(&mut self, counter: u8, start: Mark) {
        assert!(counter < 2);
        let jump = u8::try_from(self.len - start.0).expect("loop body longer than 255 bytes");
        // Bit 4 set: a counted loop, not `DMALPFE`.
        self.push(&[CMD_DMALPEND | 1 << 4 | counter << 2, jump]);
    }

    /// `DMALPFE`: back to `start` forever. `DMALPEND` with bit 4 clear and bit 2, the loop
    /// counter, set: with bit 2 clear the board's `pdma0` faults with an undefined
    /// instruction, as `audio_dma_test` shows. Neither Linux nor the vendor kernel loops
    /// forever, so neither has an encoding to compare.
    pub fn loop_forever(&mut self, start: Mark) {
        let jump = u8::try_from(self.len - start.0).expect("loop body longer than 255 bytes");
        self.push(&[CMD_DMALPEND | LPFE_COUNTER_BIT, jump]);
    }

    /// `DMAWFPS`: waits for a single request of peripheral `request` (`pl330.c:804-827`).
    pub fn wait_peripheral(&mut self, request: u8) {
        self.push(&[CMD_DMAWFP, (request & 0x1F) << 3]);
    }

    /// `DMALDS`: loads a beat from the source (`pl330.c:608-624`).
    pub fn load(&mut self) {
        self.push(&[CMD_DMALD | 1]);
    }

    /// `DMASTPS`: stores a beat to the destination and tells peripheral `request` it was a
    /// single transfer (`pl330.c:783-802`).
    pub fn store_peripheral(&mut self, request: u8) {
        self.push(&[CMD_DMASTP, (request & 0x1F) << 3]);
    }

    /// `DMASEV`: raises `event` (`pl330.c:749-763`).
    pub fn send_event(&mut self, event: u8) {
        self.push(&[CMD_DMASEV, (event & 0x1F) << 3]);
    }

    /// `DMAEND`: stops the channel.
    pub fn end(&mut self) {
        self.push(&[CMD_DMAEND]);
    }
}

impl<const N: usize> Default for Program<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// One controller. Holds no state.
#[derive(Clone, Copy, Debug)]
pub struct Pl330 {
    base: usize,
}

impl Pl330 {
    /// `pdma0`. Its clock gate must be open: [`Cmu::open_pdma0_gate`](crate::drivers::cmu::Cmu::open_pdma0_gate).
    pub const fn pdma0() -> Self {
        Self { base: PDMA0_BASE }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: a register of this controller, device memory that stays mapped for the whole
        // program.
        unsafe { ptr::read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: as in `read`.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }

    /// Its configuration.
    pub fn config(&self) -> Config {
        let cr0 = self.read(CR0);
        let peripherals = if cr0 & 1 != 0 { ((cr0 >> 12) & 0x1F) as u8 + 1 } else { 0 };
        Config {
            cr0,
            channels: ((cr0 >> 4) & 7) as u8 + 1,
            peripherals,
            events: ((cr0 >> 17) & 0x1F) as u8 + 1,
            manager_ns: cr0 & CR0_BOOT_MAN_NS != 0,
        }
    }

    /// The manager's state, `DS` bits 3:0, and its fault type if `FSM` says it faulted.
    pub fn manager(&self) -> (u32, Option<u32>) {
        let fault = (self.read(FSM) & 1 != 0).then(|| self.read(FTM));
        (self.read(DS) & 0xF, fault)
    }

    /// What `channel` holds now.
    pub fn channel(&self, channel: u8) -> ChannelReport {
        let n = usize::from(channel);
        let state = match self.read(CS + 8 * n) & 0xF {
            0 => State::Stopped,
            1 => State::Executing,
            7 => State::WaitingForPeripheral,
            0xE | 0xF => State::Faulting,
            other => State::Other(other as u8),
        };
        let fault = (self.read(FSC) & (1 << channel) != 0).then(|| self.read(FTC + 4 * n));
        ChannelReport {
            state,
            pc: self.read(CPC + 8 * n),
            source: self.read(SA + 0x20 * n),
            destination: self.read(DA + 0x20 * n),
            fault,
        }
    }

    /// Hands `instruction` to the manager, or to `channel`, through the debug registers.
    fn debug(&self, instruction: [u8; 6], channel: Option<u8>, mct: Mct) -> Result<(), Error> {
        let end = mct.count() + DBG_TIMEOUT_US * u64::from(soc::MCT_CLOCK_HZ / 1_000_000);
        while self.read(DBGSTATUS) & DBG_BUSY != 0 {
            if mct.count() > end {
                return Err(Error::ManagerBusy);
            }
        }
        let mut first = u32::from(instruction[0]) << 16 | u32::from(instruction[1]) << 24;
        if let Some(channel) = channel {
            first |= 1 | u32::from(channel) << 8;
        }
        self.write(DBGINST0, first);
        self.write(DBGINST1, u32::from_le_bytes([instruction[2], instruction[3], instruction[4], instruction[5]]));
        self.write(DBGCMD, 0);
        Ok(())
    }

    /// Starts `channel` at the program at `address`, with `DMAGO` (`pl330.c:847-866`).
    /// The program must be in DRAM and cleaned: [`Program::clean`].
    pub fn start(&self, channel: u8, address: usize, mct: Mct) -> Result<(), Error> {
        let address = u32::try_from(address).map_err(|_| Error::BadAddress)?;
        if (address as usize) < soc::DRAM_BASE {
            return Err(Error::BadAddress);
        }
        let ns = u8::from(self.config().manager_ns);
        let a = address.to_le_bytes();
        self.debug([CMD_DMAGO | ns << 1, channel & 7, a[0], a[1], a[2], a[3]], None, mct)
    }

    /// Stops `channel` at once, with `DMAKILL`.
    pub fn kill(&self, channel: u8, mct: Mct) -> Result<(), Error> {
        self.debug([CMD_DMAKILL, 0, 0, 0, 0, 0], Some(channel), mct)
    }

    /// Makes `event` an interrupt rather than an event for `DMAWFE`, so that it shows in
    /// `INTSTATUS` (`pl330.c:1049`).
    pub fn enable_interrupt(&self, event: u8) {
        self.write(INTEN, self.read(INTEN) | 1 << event);
    }

    /// Makes `event` an event again, and clears it.
    pub fn disable_interrupt(&self, event: u8) {
        self.write(INTEN, self.read(INTEN) & !(1 << event));
        self.write(INTCLR, 1 << event);
    }

    /// Whether `event` was raised since it was last cleared, and clears it.
    pub fn take_event(&self, event: u8) -> bool {
        if self.read(INTSTATUS) & 1 << event == 0 {
            return false;
        }
        self.write(INTCLR, 1 << event);
        true
    }

    /// `INTSTATUS` raw.
    pub fn events(&self) -> u32 {
        self.read(INTSTATUS)
    }
}

/// Awaiting events on [`PDMA0_IRQ`]: see [Waiting for an event](self#waiting-for-an-event).
#[cfg(feature = "embassy")]
mod asynch {
    use core::future::poll_fn;
    use core::sync::atomic::{AtomicU32, Ordering};
    use core::task::Poll;

    use embassy_sync::waitqueue::AtomicWaker;

    use super::{PDMA0_BASE, PDMA0_IRQ, Pl330};
    use crate::drivers::gic::Gic;

    /// One waiter per event: `CR0` gives 32 on this board.
    const EVENTS: usize = 32;

    static WAKERS: [AtomicWaker; EVENTS] = [const { AtomicWaker::new() }; EVENTS];
    static INTERRUPTS: AtomicU32 = AtomicU32::new(0);

    /// Interrupts [`on_interrupt`] has taken, since the start.
    pub fn interrupts() -> u32 {
        INTERRUPTS.load(Ordering::Relaxed)
    }

    /// Routes `pdma0`'s interrupt to CPU0, still disabled: each wait enables it. Call it after
    /// [`Gic::init`], before the first wait.
    pub fn init_interrupt() {
        let gic = Gic::new();
        gic.disable(PDMA0_IRQ);
        gic.set_target(PDMA0_IRQ, 0);
    }

    /// Handles `pdma0`'s interrupt, if `id` is it: turns it off at the GIC and wakes every
    /// waiter, which looks at its event itself. The events stay set for [`Pl330::take_event`].
    pub fn on_interrupt(id: u16) -> bool {
        if id != PDMA0_IRQ {
            return false;
        }
        Gic::new().disable(PDMA0_IRQ);
        INTERRUPTS.fetch_add(1, Ordering::Relaxed);
        for waker in &WAKERS {
            waker.wake();
        }
        true
    }

    impl Pl330 {
        /// Waits until `event` is set in `INTSTATUS`, at once if it is now, and leaves it set.
        /// The event must be an interrupt ([`enable_interrupt`](Self::enable_interrupt)), and
        /// [`init_interrupt`] called. One task waits for an event at a time; a later waiter
        /// takes the earlier one's place, which is then not woken. Dropping the wait leaves
        /// nothing behind but the interrupt on, which ends at the next event.
        ///
        /// # Panics
        ///
        /// On a controller other than `pdma0`, or an event above 31.
        pub async fn wait_event(&self, event: u8) {
            assert!(event < EVENTS as u8, "pdma0 has 32 events");
            self.wait_events(1 << event).await;
        }

        /// Waits until any event of `mask` is set, as [`wait_event`](Self::wait_event), and
        /// returns those set. The waiter takes the place of the lowest event's.
        ///
        /// # Panics
        ///
        /// On a controller other than `pdma0`, or an empty `mask`.
        pub async fn wait_events(&self, mask: u32) -> u32 {
            assert!(self.base == PDMA0_BASE, "only pdma0's interrupt is handled");
            assert!(mask != 0, "no event to wait for");
            let waker = &WAKERS[mask.trailing_zeros() as usize];
            poll_fn(|cx| {
                let set = self.events() & mask;
                if set != 0 {
                    return Poll::Ready(set);
                }
                // Registered before the interrupt is on, so that the handler finds it.
                waker.register(cx.waker());
                Gic::new().enable(PDMA0_IRQ);
                // What was raised before the interrupt was on still ends the wait now.
                match self.events() & mask {
                    0 => Poll::Pending,
                    set => Poll::Ready(set),
                }
            })
            .await
        }
    }
}

#[cfg(feature = "embassy")]
pub use asynch::{init_interrupt, interrupts, on_interrupt};
