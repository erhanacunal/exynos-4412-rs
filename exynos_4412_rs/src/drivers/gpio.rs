//! GPIO pins and the wake-up external interrupts of the GPX banks.
//!
//! Paths are relative to the Linux 5.15.198 tree. `pinctrl-exynos-arm.c`, `pinctrl-exynos.c`
//! and `pinctrl-exynos.h` are in `drivers/pinctrl/samsung/`; `samsung.h` is
//! `include/dt-bindings/pinctrl/samsung.h`.
//!
//! ```ignore
//! let key = gpio::GPX1.pin(3);
//! key.set_input(Pull::Up);
//! let pressed = !key.read();
//! ```
//!
//! # What this does not check
//!
//! Nothing stops a program from reconfiguring a pin that the board needs. On this board,
//! `exynos4412-mehmet.dts` names what each pin drives: a wrong level can reset the eMMC or
//! turn off the charger.
//!
//! The controllers at [`soc::GPIO2_BASE`] and [`soc::GPIO3_BASE`] have only been read from
//! the DTS, never accessed on the board. Their clocks or power domains may be off.
//!
//! # Concurrency
//!
//! Changing a pin is a read-modify-write of a register that its whole bank shares. Each
//! one masks IRQs on the calling core, so interrupt handlers may change pins. Nothing
//! protects against another core: change pins of one bank from one core at a time. Reading
//! a pin, and everything [`Eint`] does in an interrupt handler
//! ([`is_pending`](Eint::is_pending), [`clear`](Eint::clear)), is safe from all cores.
//!
//! # Interrupts
//!
//! Only the wake-up interrupts of GPX0-GPX3 are here, which is what the board's keys, PMIC
//! and charger use. The group interrupts of the other banks are not.

use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::register_structs;
use tock_registers::registers::ReadWrite;

use crate::{cpu, soc};

register_structs! {
    /// Same layout in both bank types: `pinctrl-exynos-arm.c:25-33`. The two extra
    /// registers of the off-domain banks, for sleep mode, are not used.
    BankRegisters {
        /// `.reg_offset` 0, `.fld_width` 4: function.
        (0x00 => con: ReadWrite<u32>),
        /// `.reg_offset` 1, `.fld_width` 1: level.
        (0x04 => dat: ReadWrite<u32>),
        /// `.reg_offset` 2, `.fld_width` 2: pull.
        (0x08 => pud: ReadWrite<u32>),
        /// `.reg_offset` 3, `.fld_width` 2: drive strength.
        (0x0C => drv: ReadWrite<u32>),
        (0x10 => @END),
    }
}

/// Bits per pin in `con`.
const CON_WIDTH: u32 = 4;
/// Bits per pin in `pud` and `drv`.
const FIELD2_WIDTH: u32 = 2;

/// Function of a pin, in `CON`: `samsung.h:59-67`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Function {
    /// `EXYNOS_PIN_FUNC_INPUT`.
    Input,
    /// `EXYNOS_PIN_FUNC_OUTPUT`.
    Output,
    /// A peripheral function, `2..=14`. `samsung.h` names 2 to 6 (`EXYNOS_PIN_FUNC_2`..).
    /// Which peripheral each number means depends on the pin: see
    /// `arch/arm/boot/dts/exynos4412-pinctrl.dtsi`.
    Alt(u8),
    /// `EXYNOS_PIN_FUNC_EINT`.
    Eint,
}

impl Function {
    const fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Input,
            1 => Self::Output,
            0xF => Self::Eint,
            other => Self::Alt(other as u8),
        }
    }

    const fn raw(self) -> u32 {
        match self {
            Self::Input => 0,
            Self::Output => 1,
            Self::Eint => 0xF,
            Self::Alt(number) => {
                assert!(number >= 2 && number <= 14);
                number as u32
            }
        }
    }
}

/// Internal pull of a pin, in `PUD`: `samsung.h:13-15`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pull {
    /// `EXYNOS_PIN_PULL_NONE`.
    None,
    /// `EXYNOS_PIN_PULL_DOWN`.
    Down,
    /// `EXYNOS_PIN_PULL_UP`.
    Up,
}

impl Pull {
    const fn raw(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Down => 1,
            Self::Up => 3,
        }
    }

    /// The value 2 has no name in `samsung.h`, and reads back here as `None`.
    const fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Down,
            3 => Self::Up,
            _ => Self::None,
        }
    }
}

/// Output drive strength of a pin, in `DRV`: `samsung.h:28-31`. The numeric order of the
/// register values is not the order of the strengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drive {
    /// `EXYNOS4_PIN_DRV_LV1`.
    Level1,
    /// `EXYNOS4_PIN_DRV_LV2`.
    Level2,
    /// `EXYNOS4_PIN_DRV_LV3`.
    Level3,
    /// `EXYNOS4_PIN_DRV_LV4`.
    Level4,
}

impl Drive {
    const fn raw(self) -> u32 {
        match self {
            Self::Level1 => 0,
            Self::Level2 => 2,
            Self::Level3 => 1,
            Self::Level4 => 3,
        }
    }
}

/// One bank of pins.
#[derive(Clone, Copy, Debug)]
pub struct Bank {
    base: usize,
    pins: u8,
    /// Position among the wake-up banks, GPX0-GPX3.
    wake_index: Option<u8>,
}

impl Bank {
    const fn off(controller: usize, offset: usize, pins: u8) -> Self {
        Self { base: controller + offset, pins, wake_index: None }
    }

    const fn wake(offset: usize, wake_index: u8) -> Self {
        Self { base: soc::GPIO1_BASE + offset, pins: 8, wake_index: Some(wake_index) }
    }

    /// Number of pins in this bank.
    pub const fn pin_count(&self) -> u8 {
        self.pins
    }

    /// One pin of this bank. Panics if the bank has no such pin.
    pub const fn pin(self, number: u8) -> Pin {
        assert!(number < self.pins);
        Pin { bank: self, number }
    }
}

macro_rules! banks {
    ($controller:ident: $($name:ident $offset:literal $pins:literal),* $(,)?) => {
        $(
            #[doc = concat!("Bank ", stringify!($name), " of ", stringify!($controller), ".")]
            pub const $name: Bank = Bank::off(soc::$controller, $offset, $pins);
        )*
    };
}

// `pinctrl-exynos-arm.c:355-370`, `exynos4x12_pin_banks0`.
banks!(GPIO0_BASE:
    GPA0 0x000 8, GPA1 0x020 6, GPB 0x040 8, GPC0 0x060 5, GPC1 0x080 5, GPD0 0x0A0 4,
    GPD1 0x0C0 4, GPF0 0x180 8, GPF1 0x1A0 8, GPF2 0x1C0 8, GPF3 0x1E0 6, GPJ0 0x240 8,
    GPJ1 0x260 5,
);
// `pinctrl-exynos-arm.c:373-393`, `exynos4x12_pin_banks1`.
banks!(GPIO1_BASE:
    GPK0 0x040 7, GPK1 0x060 7, GPK2 0x080 7, GPK3 0x0A0 7, GPL0 0x0C0 7, GPL1 0x0E0 2,
    GPL2 0x100 8, GPM0 0x260 8, GPM1 0x280 7, GPM2 0x2A0 5, GPM3 0x2C0 8, GPM4 0x2E0 8,
    GPY0 0x120 6, GPY1 0x140 4, GPY2 0x160 6, GPY3 0x180 8, GPY4 0x1A0 8, GPY5 0x1C0 8,
    GPY6 0x1E0 8,
);
// `pinctrl-exynos-arm.c:401-404`, `exynos4x12_pin_banks2`.
banks!(GPIO2_BASE: GPZ 0x000 7);
// `pinctrl-exynos-arm.c:407-414`, `exynos4x12_pin_banks3`.
banks!(GPIO3_BASE: GPV0 0x000 8, GPV1 0x020 8, GPV2 0x040 8, GPV3 0x060 8, GPV4 0x080 2);

/// Wake-up bank GPX0 of `GPIO1_BASE`: `pinctrl-exynos-arm.c:394`.
pub const GPX0: Bank = Bank::wake(0xC00, 0);
/// Wake-up bank GPX1 of `GPIO1_BASE`: `pinctrl-exynos-arm.c:395`.
pub const GPX1: Bank = Bank::wake(0xC20, 1);
/// Wake-up bank GPX2 of `GPIO1_BASE`: `pinctrl-exynos-arm.c:396`.
pub const GPX2: Bank = Bank::wake(0xC40, 2);
/// Wake-up bank GPX3 of `GPIO1_BASE`: `pinctrl-exynos-arm.c:397`.
pub const GPX3: Bank = Bank::wake(0xC60, 3);

/// One pin. Holds no state, so copies refer to the same pin.
#[derive(Clone, Copy, Debug)]
pub struct Pin {
    bank: Bank,
    number: u8,
}

impl Pin {
    fn regs(&self) -> &BankRegisters {
        // SAFETY: every `Bank` holds the address of a register block from the tables above,
        // device memory that stays mapped for the whole program, and the struct has
        // interior mutable cells only.
        unsafe { &*(self.bank.base as *const BankRegisters) }
    }

    /// The function this pin has now.
    pub fn function(&self) -> Function {
        let shift = u32::from(self.number) * CON_WIDTH;
        Function::from_raw((self.regs().con.get() >> shift) & 0xF)
    }

    /// Selects the function of this pin.
    pub fn set_function(&self, function: Function) {
        let shift = u32::from(self.number) * CON_WIDTH;
        modify(&self.regs().con, 0xF << shift, function.raw() << shift);
    }

    /// The internal pull of this pin.
    pub fn pull(&self) -> Pull {
        let shift = u32::from(self.number) * FIELD2_WIDTH;
        Pull::from_raw((self.regs().pud.get() >> shift) & 3)
    }

    /// Selects the internal pull of this pin.
    pub fn set_pull(&self, pull: Pull) {
        let shift = u32::from(self.number) * FIELD2_WIDTH;
        modify(&self.regs().pud, 3 << shift, pull.raw() << shift);
    }

    /// Selects the output drive strength of this pin.
    pub fn set_drive(&self, drive: Drive) {
        let shift = u32::from(self.number) * FIELD2_WIDTH;
        modify(&self.regs().drv, 3 << shift, drive.raw() << shift);
    }

    /// Makes this pin an input, with the pull set first so that it never floats on the way.
    pub fn set_input(&self, pull: Pull) {
        self.set_pull(pull);
        self.set_function(Function::Input);
    }

    /// Makes this pin an output at the level `high`. The level is set first so that the
    /// pin does not show the old one on the way, and the pull is turned off.
    pub fn set_output(&self, high: bool) {
        self.write(high);
        self.set_pull(Pull::None);
        self.set_function(Function::Output);
    }

    /// The level in this pin's `DAT` bit.
    pub fn read(&self) -> bool {
        self.regs().dat.get() & (1 << self.number) != 0
    }

    /// Sets the level of this pin, which appears on it when it is an output.
    pub fn write(&self, high: bool) {
        let mask = 1 << self.number;
        modify(&self.regs().dat, mask, if high { mask } else { 0 });
    }

    /// The wake-up interrupt of this pin, if its bank is one of GPX0-GPX3.
    pub const fn eint(self) -> Option<Eint> {
        match self.bank.wake_index {
            Some(_) => Some(Eint { pin: self }),
            None => None,
        }
    }
}

fn modify(register: &ReadWrite<u32>, mask: u32, value: u32) {
    let was_masked = cpu::mask_irq();
    register.set((register.get() & !mask) | value);
    cpu::restore_irq(was_masked);
}

register_structs! {
    /// Wake-up interrupt registers, one word per bank, in the controller of the GPX banks.
    /// Offsets: `pinctrl-exynos.h:24-26`. Word per bank: `.eint_offset` 0, 4, 8 and 12 at
    /// `pinctrl-exynos-arm.c:394-397`.
    WakeRegisters {
        (0x000 => _reserved0),
        /// `EXYNOS_WKUP_ECON_OFFSET`.
        (0xE00 => con: [ReadWrite<u32>; 4]),
        (0xE10 => _reserved1),
        /// `EXYNOS_WKUP_EMASK_OFFSET`. A set bit masks: `pinctrl-exynos.c:64`.
        (0xF00 => mask: [ReadWrite<u32>; 4]),
        (0xF10 => _reserved2),
        /// `EXYNOS_WKUP_EPEND_OFFSET`. Writing a set bit clears: `pinctrl-exynos.c:77`.
        (0xF40 => pend: [ReadWrite<u32>; 4]),
        (0xF50 => @END),
    }
}

/// Bits per pin in the `con` of [`WakeRegisters`]: `pinctrl-exynos.h:46` `EXYNOS_EINT_CON_LEN`.
const ECON_WIDTH: u32 = 4;
/// `pinctrl-exynos.h:45` `EXYNOS_EINT_CON_MASK`.
const ECON_MASK: u32 = 0xF;

/// What makes a wake-up interrupt pending: `pinctrl-exynos.h:40-44`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `EXYNOS_EINT_LEVEL_LOW`.
    LevelLow,
    /// `EXYNOS_EINT_LEVEL_HIGH`.
    LevelHigh,
    /// `EXYNOS_EINT_EDGE_FALLING`.
    FallingEdge,
    /// `EXYNOS_EINT_EDGE_RISING`.
    RisingEdge,
    /// `EXYNOS_EINT_EDGE_BOTH`.
    BothEdges,
}

impl Trigger {
    const fn raw(self) -> u32 {
        match self {
            Self::LevelLow => 0,
            Self::LevelHigh => 1,
            Self::FallingEdge => 2,
            Self::RisingEdge => 3,
            Self::BothEdges => 4,
        }
    }
}

/// The wake-up interrupt of one GPX pin, from [`Pin::eint`].
///
/// To use one: [`listen`](Self::listen), [`Gic::enable`](crate::drivers::gic::Gic::enable)
/// its [`irq`](Self::irq), and in the handler [`clear`](Self::clear) it before the
/// `end_of_interrupt`. GPX2 and GPX3 share one GIC interrupt, so their handler looks for
/// the source with [`is_pending`](Self::is_pending).
#[derive(Clone, Copy, Debug)]
pub struct Eint {
    pin: Pin,
}

impl Eint {
    fn regs(&self) -> &WakeRegisters {
        // SAFETY: GPIO1_BASE is the controller holding the GPX banks and these registers,
        // device memory that stays mapped for the whole program, and the struct has
        // interior mutable cells only.
        unsafe { &*(soc::GPIO1_BASE as *const WakeRegisters) }
    }

    fn word(&self) -> usize {
        // `Eint` is only built from a pin whose bank has a wake index.
        usize::from(self.pin.bank.wake_index.unwrap_or(0))
    }

    fn bit(&self) -> u32 {
        1 << self.pin.number
    }

    /// The pin this interrupt belongs to.
    pub const fn pin(&self) -> Pin {
        self.pin
    }

    /// The GIC interrupt ID this interrupt reaches.
    pub fn irq(&self) -> u16 {
        match self.word() {
            0 => soc::EINT_GPX0_IRQ + u16::from(self.pin.number),
            1 => soc::EINT_GPX1_IRQ + u16::from(self.pin.number),
            _ => soc::EINT_GPX23_IRQ,
        }
    }

    /// Hands the pin to this interrupt and unmasks it. An edge caused by this
    /// reconfiguration is discarded. Set the pull first, with [`Pin::set_pull`].
    pub fn listen(&self, trigger: Trigger) {
        self.mask();
        let shift = u32::from(self.pin.number) * ECON_WIDTH;
        modify(&self.regs().con[self.word()], ECON_MASK << shift, trigger.raw() << shift);
        self.pin.set_function(Function::Eint);
        self.clear();
        self.unmask();
    }

    /// Stops this interrupt from reaching the GIC. It can still become pending.
    pub fn mask(&self) {
        modify(&self.regs().mask[self.word()], self.bit(), self.bit());
    }

    /// Lets this interrupt reach the GIC, including an edge that came while it was masked.
    pub fn unmask(&self) {
        modify(&self.regs().mask[self.word()], self.bit(), 0);
    }

    /// Whether this interrupt is pending.
    pub fn is_pending(&self) -> bool {
        self.regs().pend[self.word()].get() & self.bit() != 0
    }

    /// Clears this interrupt only. Other pins of the bank are not affected.
    pub fn clear(&self) {
        self.regs().pend[self.word()].set(self.bit());
    }
}
