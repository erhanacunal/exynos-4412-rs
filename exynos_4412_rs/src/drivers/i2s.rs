//! I2S0 of the audio subsystem as clock master, sending 16-bit stereo at 48 kHz, and the
//! subsystem's clocks.
//!
//! Paths are relative to the Linux 5.15.198 tree: `i2s.c` and `i2s-regs.h` are in
//! `sound/soc/samsung/`, `clk-exynos-audss.c` in `drivers/clk/samsung/`. `exynos4.dtsi:77-91`
//! makes I2S0 a `samsung,s5pv210-i2s`, whose registers are `i2sv3_regs` (`i2s.c:1560-1572`,
//! `i2s.c:1622-1626`).
//!
//! # Clocks
//!
//! [`set_clocks`] runs the EPLL at 196.608 MHz and divides it in the audio subsystem
//! (`clk-exynos-audss.c:186-230`):
//!
//! ```text
//! fout_epll 196.608 MHz -> mout_audss -> dout_srp /4 -> dout_aud_bus /4 = 12.288 MHz
//!                                     -> mout_i2s -> dout_i2s /16          = 12.288 MHz
//! ```
//!
//! `dout_aud_bus` is both the clock of I2S0's registers (`i2s_bus`) and its `i2s_opclk0`,
//! which the controller takes as RCLK through a prescaler of 1. RCLK goes out on CDCLK as the
//! codec's master clock, 256 times the frame rate; BCLK is 32 times it, LRCLK the frame rate.
//! The dividers are written directly: Linux picks them from rates it computes rounded, which
//! can land on the next divider.
//!
//! # FIFO
//!
//! One 32-bit word is one frame, both 16-bit channels. [`I2s::try_write`] adds one while the
//! transmit FIFO has room. When the FIFO runs dry the controller sends what it has and flags an
//! underrun, which [`I2s::take_underrun`] reads and clears.
//!
//! # DMA
//!
//! [`DmaPlayback`] has `pdma0` fill the FIFO from a ring of two halves in DRAM, on I2S0's
//! transmit request ([`TX_DMA_REQUEST`]). Its channel program loops over the ring forever and
//! raises one event after each half: the CPU then refills that half while the other plays.
//! The ring's halves last [`DMA_HALF_FRAMES`] frames, 85 ms, so the CPU may be busy that long
//! between two refills. A half not refilled in time plays again, and counts as late.

use core::ptr;

use crate::drivers::cmu::{Cmu, EpllTimeout, Pll36xx};
use crate::drivers::gpio::{self, Drive, Function, Pin, Pull};
use crate::drivers::pl330::{self, Pl330, Program};
use crate::drivers::timer::Mct;
use crate::soc;

/// I2S0's transmit request on `pdma0`: `exynos4.dtsi:86-87`, `dmas`, the one named `tx`.
pub const TX_DMA_REQUEST: u8 = 12;
/// The transmit FIFO, where the DMA controller writes.
pub const TX_FIFO_ADDRESS: usize = soc::I2S0_BASE + I2STXD;
/// The `pdma0` channel [`DmaPlayback`] runs on.
pub const DMA_CHANNEL: u8 = 0;
/// The `pdma0` events [`DmaPlayback`] raises: after the first half, after the second.
pub const DMA_EVENTS: [u8; 2] = [0, 1];
/// Frames in each half of [`DmaPlayback`]'s ring: 16 loops of 256, 85.3 ms.
pub const DMA_HALF_FRAMES: usize = 16 * 256;

/// Frames per second.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// RCLK and CDCLK, 256 frames.
pub const RCLK_HZ: u32 = 256 * SAMPLE_RATE_HZ;
/// Depth of the transmit FIFO, in frames: the count field of `I2SFIC` has 7 bits
/// (`i2s.c:1021`). TODO(verify): 64, as the Exynos 4412 manual gives.
pub const TX_FIFO_FRAMES: u32 = 64;

/// `clk-exynos-audss.c:32-34`.
const ASS_CLK_SRC: usize = 0x0;
const ASS_CLK_DIV: usize = 0x4;
const ASS_CLK_GATE: usize = 0x8;
/// `clk-exynos-audss.c:186-189`: `mout_audss` is bit 0, `fout_epll` when set.
const SRC_AUDSS_EPLL: u32 = 1 << 0;
/// `clk-exynos-audss.c:197-200`: `mout_i2s` is bits 3:2, `mout_audss` at 0.
const SRC_I2S_MASK: u32 = 3 << 2;
/// `clk-exynos-audss.c:202-212`: `dout_srp` bits 3:0, `dout_aud_bus` bits 7:4, `dout_i2s`
/// bits 11:8, each the divider minus 1.
const DIV_SRP: u32 = 4;
const DIV_AUD_BUS: u32 = 4;
const DIV_I2S: u32 = 16;
/// `clk-exynos-audss.c:214-224`: `srp_clk`, `i2s_bus` and `sclk_i2s`.
const GATE_SRP: u32 = 1 << 0;
const GATE_I2S_BUS: u32 = 1 << 2;
const GATE_SCLK_I2S: u32 = 1 << 3;

/// `i2s-regs.h:12-15`.
const I2SCON: usize = 0x0;
const I2SMOD: usize = 0x4;
const I2SFIC: usize = 0x8;
const I2SPSR: usize = 0xC;
/// `i2s-regs.h:16`.
const I2STXD: usize = 0x10;

/// `i2s-regs.h:34`. Kept set: `QUIRK_NEED_RSTCLR` (`i2s.c:1056-1058`) holds the controller in
/// reset without it.
const CON_RSTCLR: u32 = 1 << 31;
/// `i2s-regs.h:42-43`: the underrun flag, cleared by writing 1, and its interrupt enable.
const CON_FTXURSTATUS: u32 = 1 << 17;
const CON_FTXURINTEN: u32 = 1 << 16;
/// `i2s-regs.h:52`.
const CON_TXFIFO_FULL: u32 = 1 << 8;
/// `i2s-regs.h:54,56`.
const CON_TXDMA_PAUSE: u32 = 1 << 6;
/// `i2s-regs.h:58`: requests to the DMA controller while the FIFO has room.
const CON_TXDMA_ACTIVE: u32 = 1 << 2;
const CON_TXCH_PAUSE: u32 = 1 << 4;
/// `i2s-regs.h:60`.
const CON_ACTIVE: u32 = 1 << 0;

/// `i2sv3_regs`, `i2s.c:1560-1572`: CDCLK is an input when bit 12 is set, RCLK comes from
/// `i2s_opclk1` when bit 10 is, and the controller is a clock slave when bit 11 is. All clear.
/// TX only (bits 9:8 at 0, `i2s-regs.h:92`), left channel while LRCLK is low (bit 7 at 0,
/// `i2s-regs.h:97`), I2S format (bits 6:5 at 0, `i2s-regs.h:100`), RCLK 256 fs (bits 4:3 at 0,
/// `i2s-regs.h:105`), BCLK 32 fs (bits 2:1 at 0, `i2s-regs.h:111`), 16 bits a channel (bits
/// 14:13 at 0, `i2s-regs.h:87`).
const MOD_MASTER_16BIT_I2S: u32 = 0;
/// `i2s-regs.h:134` and `i2s.c:919-920`: the prescaler on, dividing by bits 13:8 plus 1.
const PSR_PSREN: u32 = 1 << 15;
/// `i2s-regs.h:139,142`.
const FIC_TXFLUSH: u32 = 1 << 15;
const FIC_TXCOUNT_SHIFT: u32 = 8;
const FIC_TXCOUNT_MASK: u32 = 0x7F;

/// `exynos4412-pinctrl.dtsi:907-913`, `i2s0_bus`: SCLK, CDCLK, LRCLK, SDI, SDO0-2, function
/// 2, no pull. The board's DTS raises their drive to level 2.
const PINS: [u8; 7] = [0, 1, 2, 3, 4, 5, 6];
const PIN_FUNCTION: u8 = 2;

/// Rates of the clocks [`set_clocks`] set.
#[derive(Clone, Copy, Debug)]
pub struct ClockRates {
    /// The EPLL.
    pub epll_hz: u32,
    /// `dout_aud_bus`: the registers' clock and RCLK.
    pub aud_bus_hz: u32,
    /// `dout_i2s`, not used as RCLK here.
    pub i2s_hz: u32,
}

/// The clock registers of the audio subsystem, for reports.
#[derive(Clone, Copy, Debug)]
pub struct AudioClocks {
    /// `ASS_CLK_SRC`.
    pub src: u32,
    /// `ASS_CLK_DIV`.
    pub div: u32,
    /// `ASS_CLK_GATE`.
    pub gate: u32,
}

fn audss_read(offset: usize) -> u32 {
    // SAFETY: the three registers of `soc::AUDSS_BASE`, device memory that stays mapped for
    // the whole program.
    unsafe { ptr::read_volatile((soc::AUDSS_BASE + offset) as *const u32) }
}

fn audss_write(offset: usize, value: u32) {
    // SAFETY: as in `audss_read`. Only the audio subsystem uses them.
    unsafe { ptr::write_volatile((soc::AUDSS_BASE + offset) as *mut u32, value) }
}

/// The clock registers of the audio subsystem as they are now. Readable only once its clock
/// runs: after [`set_clocks`].
pub fn audio_clocks() -> AudioClocks {
    AudioClocks { src: audss_read(ASS_CLK_SRC), div: audss_read(ASS_CLK_DIV), gate: audss_read(ASS_CLK_GATE) }
}

/// Runs the EPLL at 196.608 MHz, opens the audio gates in the CMU, and sets the subsystem's
/// dividers and muxes as in the [module documentation](self). Stop I2S0 first.
///
/// The EPLL comes first: the subsystem's registers run from `sclk_epll` (clk-exynos4.c:962),
/// which may be the EPLL, off from U-Boot. Until the dividers are written, a mux U-Boot left
/// on the EPLL passes it with U-Boot's dividers.
pub fn set_clocks(mct: Mct) -> Result<ClockRates, EpllTimeout> {
    let cmu = Cmu::new();
    let epll_hz = cmu.set_epll(Pll36xx::AUDIO_196_608_MHZ, mct)?;
    cmu.open_audio_gates();
    // The dividers before the mux, so that the EPLL never reaches the bus undivided.
    audss_write(ASS_CLK_DIV, (DIV_I2S - 1) << 8 | (DIV_AUD_BUS - 1) << 4 | (DIV_SRP - 1));
    audss_write(ASS_CLK_SRC, (audss_read(ASS_CLK_SRC) & !SRC_I2S_MASK) | SRC_AUDSS_EPLL);
    audss_write(ASS_CLK_GATE, audss_read(ASS_CLK_GATE) | GATE_SRP | GATE_I2S_BUS | GATE_SCLK_I2S);
    Ok(ClockRates { epll_hz, aud_bus_hz: epll_hz / DIV_SRP / DIV_AUD_BUS, i2s_hz: epll_hz / DIV_I2S })
}

/// Gives I2S0 its pins, `gpz-0` to `gpz-6`. Their bank is in the audio subsystem: call it
/// after [`set_clocks`].
pub fn take_pins() {
    for number in PINS {
        let pin: Pin = gpio::GPZ.pin(number);
        pin.set_pull(Pull::None);
        pin.set_drive(Drive::Level2);
        pin.set_function(Function::Alt(PIN_FUNCTION));
    }
}

/// One frame for [`I2s::try_write`]: the left channel in the high half. Checked on the board
/// with a tone in one channel only.
pub const fn frame(left: i16, right: i16) -> u32 {
    (left as u16 as u32) << 16 | right as u16 as u32
}

/// The registers of I2S0, for reports.
#[derive(Clone, Copy, Debug)]
pub struct I2sRegisters {
    /// `I2SCON`.
    pub con: u32,
    /// `I2SMOD`.
    pub mode: u32,
    /// `I2SFIC`.
    pub fic: u32,
    /// `I2SPSR`.
    pub psr: u32,
}

/// I2S0. Holds no state.
#[derive(Clone, Copy, Debug)]
pub struct I2s;

impl I2s {
    /// A handle. Its registers answer once the audio gates are open.
    pub const fn new() -> Self {
        Self
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: registers of `soc::I2S0_BASE`, device memory that stays mapped for the whole
        // program.
        unsafe { ptr::read_volatile((soc::I2S0_BASE + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: as in `read`.
        unsafe { ptr::write_volatile((soc::I2S0_BASE + offset) as *mut u32, value) }
    }

    /// The registers as they are now.
    pub fn registers(&self) -> I2sRegisters {
        I2sRegisters {
            con: self.read(I2SCON),
            mode: self.read(I2SMOD),
            fic: self.read(I2SFIC),
            psr: self.read(I2SPSR),
        }
    }

    /// Stops it, and sets it up as clock master for 16-bit stereo I2S, RCLK from
    /// `i2s_opclk0` undivided, CDCLK out. Its clocks run once [`start`](Self::start) is called.
    /// Leaves the transmit FIFO empty.
    pub fn configure(&self) {
        self.stop();
        self.write(I2SMOD, MOD_MASTER_16BIT_I2S);
        self.write(I2SPSR, PSR_PSREN);
        self.flush();
    }

    /// Empties the transmit FIFO, as `i2s.c:473-495` does.
    pub fn flush(&self) {
        let fic = self.read(I2SFIC);
        self.write(I2SFIC, fic | FIC_TXFLUSH);
        // `i2s.c:489-492` waits 1 us. Each read takes at least a cycle of the 12.288 MHz
        // register clock, 81 ns: 32 of them take more than 2.6 us.
        for _ in 0..32 {
            let _ = self.read(I2SFIC);
        }
        self.write(I2SFIC, fic & !FIC_TXFLUSH);
    }

    /// Runs the clocks and sends from the FIFO, as `i2s.c:391-414` without DMA.
    pub fn start(&self) {
        let con = self.read(I2SCON) & !(CON_TXCH_PAUSE | CON_FTXURSTATUS);
        self.write(I2SCON, con | CON_RSTCLR | CON_TXDMA_PAUSE | CON_FTXURINTEN | CON_ACTIVE);
    }

    /// Runs the clocks and has the DMA controller fill the FIFO.
    pub fn start_dma(&self) {
        let con = self.read(I2SCON) & !(CON_TXCH_PAUSE | CON_TXDMA_PAUSE | CON_FTXURSTATUS);
        self.write(I2SCON, con | CON_RSTCLR | CON_TXDMA_ACTIVE | CON_FTXURINTEN | CON_ACTIVE);
    }

    /// Stops sending, the clocks and the DMA requests.
    pub fn stop(&self) {
        let con = self.read(I2SCON) & !(CON_ACTIVE | CON_TXDMA_ACTIVE | CON_FTXURSTATUS);
        self.write(I2SCON, con | CON_RSTCLR | CON_TXCH_PAUSE | CON_TXDMA_PAUSE);
    }

    /// Adds `frame` to the transmit FIFO, `false` if it is full.
    pub fn try_write(&self, frame: u32) -> bool {
        if self.read(I2SCON) & CON_TXFIFO_FULL != 0 {
            return false;
        }
        self.write(I2STXD, frame);
        true
    }

    /// Frames in the transmit FIFO.
    pub fn tx_level(&self) -> u32 {
        (self.read(I2SFIC) >> FIC_TXCOUNT_SHIFT) & FIC_TXCOUNT_MASK
    }

    /// Whether the transmit FIFO ran dry since the last call, and clears the flag.
    pub fn take_underrun(&self) -> bool {
        let con = self.read(I2SCON);
        if con & CON_FTXURSTATUS == 0 {
            return false;
        }
        self.write(I2SCON, con);
        true
    }
}

impl Default for I2s {
    fn default() -> Self {
        Self::new()
    }
}

/// The ring [`DmaPlayback`] plays: two halves of [`DMA_HALF_FRAMES`] frames.
#[repr(C, align(32))]
pub struct DmaRing(pub [[u32; DMA_HALF_FRAMES]; 2]);

impl DmaRing {
    /// A ring of silence.
    pub const fn new() -> Self {
        Self([[0; DMA_HALF_FRAMES]; 2])
    }
}

impl Default for DmaRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Room for [`DmaPlayback`]'s channel program, 54 bytes.
pub type DmaProgram = Program<64>;

/// What [`DmaPlayback::service`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Serviced {
    /// Halves refilled.
    pub refilled: u32,
    /// Of those, halves found played twice: both halves had ended when this looked.
    pub late: u32,
}

/// Playback through `pdma0`: see the [module documentation](self#dma).
pub struct DmaPlayback {
    ring: &'static mut DmaRing,
    program: &'static mut DmaProgram,
    dma: Pl330,
    /// The half whose event comes next.
    next: usize,
}

impl DmaPlayback {
    /// Assembles the channel program for `ring`. Open the controller's gate first:
    /// [`Cmu::open_pdma0_gate`].
    pub fn new(ring: &'static mut DmaRing, program: &'static mut DmaProgram) -> Self {
        let dma = Pl330::pdma0();
        let config = dma.config();
        *program = Program::new();
        // One frame a beat, from the ring stepping, to the FIFO, fixed.
        program.mov_control(pl330::ccr(&config, 2, true, false));
        program.mov_destination(TX_FIFO_ADDRESS as u32);
        let top = program.here();
        program.mov_source(ring.0.as_ptr() as u32);
        for event in DMA_EVENTS {
            let outer = program.loop_start(1, (DMA_HALF_FRAMES / 256) as u16);
            let inner = program.loop_start(0, 256);
            // Before each request, as `pl330.c:1191-1196` does: without it, requests the
            // controller holds from before run beats the FIFO has no room for.
            program.flush_peripheral(TX_DMA_REQUEST);
            program.wait_peripheral(TX_DMA_REQUEST);
            program.load();
            program.store_peripheral(TX_DMA_REQUEST);
            program.loop_end(0, inner);
            program.loop_end(1, outer);
            program.send_event(event);
        }
        program.loop_forever(top);
        Self { ring, program, dma, next: 0 }
    }

    /// The channel program, for reports.
    pub fn program(&self) -> &[u8] {
        self.program.bytes()
    }

    /// The ring, to fill before [`start`](Self::start).
    pub fn ring(&mut self) -> &mut DmaRing {
        self.ring
    }

    /// Starts I2S0 and the channel, from the first half. Configure I2S0 first
    /// ([`I2s::configure`]), and fill the ring: what is in it plays.
    pub fn start(&mut self, i2s: I2s, mct: Mct) -> Result<(), pl330::Error> {
        let dma = self.dma;
        if dma.channel(DMA_CHANNEL).state != pl330::State::Stopped {
            dma.kill(DMA_CHANNEL, mct)?;
        }
        for event in DMA_EVENTS {
            dma.enable_interrupt(event);
            dma.take_event(event);
        }
        crate::mmu::clean_dcache_range(self.ring.0.as_ptr() as usize, size_of::<DmaRing>());
        self.program.clean();
        self.next = 0;
        dma.start(DMA_CHANNEL, self.program.address(), mct)?;
        i2s.start_dma();
        Ok(())
    }

    /// Refills each half that has played since the last call, in order, with `fill`, and
    /// writes it out of the caches for the controller.
    pub fn service(&mut self, mut fill: impl FnMut(&mut [u32; DMA_HALF_FRAMES])) -> Serviced {
        let mut done = Serviced::default();
        while self.dma.take_event(DMA_EVENTS[self.next]) {
            // The other half's event too: it has ended as well, and this one plays again.
            if self.dma.events() & 1 << DMA_EVENTS[1 - self.next] != 0 {
                done.late += 1;
            }
            let half = &mut self.ring.0[self.next];
            fill(half);
            crate::mmu::clean_dcache_range(half.as_ptr() as usize, size_of_val(half));
            done.refilled += 1;
            self.next = 1 - self.next;
        }
        done
    }

    /// Where the channel reads now, as a frame of the ring; `None` if outside it.
    pub fn position(&self) -> Option<usize> {
        let source = self.dma.channel(DMA_CHANNEL).source as usize;
        let start = self.ring.0.as_ptr() as usize;
        (start..start + size_of::<DmaRing>()).contains(&source).then(|| (source - start) / 4)
    }

    /// The channel's state, for reports.
    pub fn channel(&self) -> pl330::ChannelReport {
        self.dma.channel(DMA_CHANNEL)
    }

    /// Stops I2S0 and the channel.
    pub fn stop(&mut self, i2s: I2s, mct: Mct) -> Result<(), pl330::Error> {
        i2s.stop();
        self.dma.kill(DMA_CHANNEL, mct)?;
        for event in DMA_EVENTS {
            self.dma.disable_interrupt(event);
        }
        Ok(())
    }
}
