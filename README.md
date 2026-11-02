# Bare-metal Rust for the Exynos 4412

Bare-metal Rust on the Samsung Exynos 4412 (4 × Cortex-A9), started with `bootm` from the
board's own U-Boot. `exynos_4412_rs` provides the runtime and the drivers. An application
depends on this crate, supplies a `memory.x` and writes `main`.

- `exynos_4412_rs/`: startup, exceptions, drivers, console, panic, `link.x`
- `hello_world/`: console test
- `irq_timer/`: GIC and MCT test
- `mmu_test/`: MMU and cache test
- `l2_test/`: L2 cache test and MCT rate measurement
- `smp_test/`: CPU1-3 bring-up, inter-core memory and SGI tests, L2 latency measurement
- `timer_test/`: per-core MCT local timer test
- `gpio_test/`: GPIO pins and key interrupts test
- `i2c_test/`: I2C bus test, performs only reads on the devices
- `pmic_test/`: MAX77686 test, turns peripheral rails on and restores them

## Build

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release   # ELF
make bin                # hello_world.bin and its size
make img                # boot.img (with ~/mkbootimg/mkbootimg)
make disasm             # entry code and vector table
make img APP=irq_timer  # same targets for another application
```

Outputs are under `target/armv7a-none-eabihf/release/`. The target is set in
`.cargo/config.toml`, the toolchain in `rust-toolchain.toml`.

For a different load address, change `ORIGIN` in `hello_world/memory.x`; since U-Boot's
`bootcmd` uses `0x40008000`, the board side must match it too.

## Flashing the board

`bootcmd` does not change, the flow is the same as for Linux:

```
movi init; mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
```

1. `make img`
2. With the board in fastboot mode, using the partition name you use for Linux:
   ```sh
   fastboot flash <partition> target/armv7a-none-eabihf/release/boot.img
   ```
3. `boot` in U-Boot.

Console: UART2, 115200n8. `dcache off` is not needed on the `bootm` path, as `bootm` already
turns the caches off for the kernel. This overwrites the Linux image in the partition. To go
back, write `~/boot.img` with the same command; U-Boot is not touched.

Starting with `go` is also supported (`dcache off`, load, `go 0x40008000`), but this path has
not been tested.

## Image format

This U-Boot is signed and has no source, so how `bootm` recognises an image is unknown. The
only known working example is a Linux `zImage`, so the image starts with its header:

| Offset | Content |
|---|---|
| `0x00-0x1F` | 8 × `mov r0, r0` |
| `0x20` | `b __exynos_boot` |
| `0x24` | magic `0x016f2818` |
| `0x28` | start: `0` |
| `0x2C` | image size |
| `0x30` | `0x04030201` |

Verified on the board: `boot.img` is written to the partition with its Android header, and
`bootm` starts the image one page later, from `0x40009000`. Linux is unaffected because `zImage`
is position-independent, but our code is linked at `0x40008000`. The self-relocation step in
startup is therefore **required** and must not be removed.

## Board test results (`hello_world`)

Results from the `boot` command in U-Boot (`Willow #`). The `Unknown command 'movi'` warning
comes from `bootcmd` and does not affect booting.

| Action | Observed |
|---|---|
| `bootm` | `Boot with Android zImage`, `Boot with zImage`, `Boot with Android Ramdisk Image`, `Starting kernel ...` |
| Boot | `Hello Exynos 4412` |
| Start address | `0x40009000` |
| `r0`-`r2` | `0x43e00898`, `0x000010b4`, `0x40000100` |
| `MPIDR` | `0x80000a00` (CPU0, matches `cpu@a00` in the DTS) |
| `CPSR` | `0x600001d3`, Supervisor |
| Typing characters | Echo works |
| `p` | `*** Panic at hello_world/src/main.rs:26` and the message, then halts (since SMP the line starts `*** CPU0 panic at`) |
| `a` | `DataAbort`, `DFSR=0x00000001` (alignment), `DFAR=0x4000e175`, register dump, then halts |

Not yet tested on the board: FIQ, the default undefined and SVC reports, and the wait loop of
a secondary core that reaches `_start` by accident.

## Dependency

`tock-registers` was chosen for MMIO: it has no dependencies, defines the register map and bit
fields with types, and can also express byte-wide `UTXH`/`URXH` access.

## Information taken from the DTS

Source: `exynos4412-mehmet.dts`. It is the same file as the copy in the Linux tree.
5.15 has no `exynos4x12.dtsi` or `exynos4x12-pinctrl.dtsi`; `exynos4412.dtsi` (which includes
`exynos4.dtsi`) and `exynos4412-pinctrl.dtsi` were used instead.

| Item | Value | Source |
|---|---|---|
| Console | UART2, `0x1382_0000`, 115200n8, pins `gpa1-0`/`gpa1-1` | `chosen/stdout-path = "serial2:115200n8"`, `aliases/serial2`, `serial_2`, `uart2_data` |
| DRAM | `0x4000_0000`, size `0x7EA0_0000` (2026 MiB), end address `0xBEA0_0000` exclusive | `memory@40000000` |
| Secure firmware | `0x0204_F000` | `firmware@204f000` |

Peripherals with `status = "okay"`:

- **UART:** `serial_0` (Bluetooth), `serial_2` (console)
- **I2C:** `i2c_0`, `i2c_1`, `i2c_3`, `i2c_5`, `i2c_6`, `i2c_7`, `i2c_8`
- **Storage:** `sdhci_2`, `sdhci_3` (WiFi), `mshc_0` (eMMC)
- **Display:** `fimd`, `gpu`, `mfc`, `pwm` (backlight)
- **Camera:** `camera`, `csis_1`, `fimc_0`, `fimc_1`, `fimc_3`
- **Audio:** `i2s0`
- **USB:** `ehci`, `exynos_usbphy`
- **System:** `adc`, `rtc`, `tmu`, `watchdog`
- **Bus frequency:** `bus_acp`, `bus_c2c`, `bus_display`, `bus_dmc`, `bus_fsys`,
  `bus_leftbus`, `bus_mfc`, `bus_peri`, `bus_rightbus`

## Assumptions

- **Cache:** Startup turns off the MMU and caches but does not clean dirty lines; `bootm` (or
  `dcache off` before `go`) must have done that. L2 is off at this point.
- **Clocks and pins:** U-Boot configures the clock, pins and baud rate of the console UART.
- **Memory:** `bootcmd` reads about 10 MiB from the partition and writes it from `0x40008000`;
  garbage following the image is not a problem, since `.bss` is zeroed. `make bin` rejects an
  image that exceeds the limit.
- **Single core:** Only CPU0 runs, the others wait in a `wfe` loop. The console has no lock.
- **Security state:** The board runs Non-secure (measured with `mmu_test`). `NSACR` allows VFP
  access. On another board without that permission, startup stops in a `wfi` loop before entering
  Rust and prints nothing to the console.
- **Alignment:** With the MMU off, memory is Strongly-ordered and the result of an unaligned
  access is undefined. Startup therefore sets `SCTLR.A`, so an unaligned access produces a
  precise data abort. The target's `strict-align` code generation keeps the compiler from
  emitting such accesses.
- **Atomics:** Because `LDREX`/`STREX` are not reliable on Strongly-ordered memory, the library
  uses only `load`/`store` on atomics of 32 bits and smaller. Applications must also avoid
  `compare_exchange`, `fetch_add`, `swap` and `AtomicU64` until the MMU is on.
- **Asynchronous abort:** `CPSR.A` is left as it came from U-Boot. If masked, asynchronous
  external aborts are not visible.

## Design notes

- **FIQ stack:** FIQ was added to the requested SVC/IRQ/ABT/UND stacks (`__fiq_stack_size`),
  because the FIQ handler needs a stack too.
- **VFP save:** The exception stubs also save `d0-d7`, `d16-d31` and `FPSCR`, because the
  target's code generation uses `d16-d31`.
- **SVC:** Because the application runs in SVC mode, its own `svc` call overwrites its `LR`. Code
  using inline `svc` must declare `lr` as a clobber.
- **`BootArgs.load_addr`:** In addition to the requested `r0`-`r2`, the address the image was
  started from.

## Handler override

Each handler is a linker symbol, whose default is bound by the `PROVIDE` lines in `link.x`:
`handle_undef`, `handle_svc`, `handle_prefetch_abort`, `handle_data_abort`,
`handle_irq`, `handle_fiq`.

```rust
#[unsafe(no_mangle)]
extern "C" fn handle_irq(frame: &mut exynos_4412_rs::exceptions::ExceptionFrame) { /* ... */ }
```

## Interrupts and timer

`drivers::gic::Gic` enables, acknowledges and ends interrupts; `drivers::timer::Mct` provides the
64-bit counter and the periodic interrupt. The application defines the `handle_irq` symbol.
Example: `irq_timer/src/main.rs`.

- **Counter:** MCT global counter, comparator 0, `GIC_SPI 57` (GIC ID 89), 24 MHz.
- **Order:** The handler first clears the source (`Mct::clear_interrupt`), then writes EOI.
- **Atomics:** `fetch_add` and `AtomicU64` are forbidden with the MMU off; even the `load` and
  `store` of `AtomicU64` produce `LDREXD`/`STREXD`. If only the handler writes a variable,
  `load` + `store` on atomics of 32 bits and smaller is enough.
- **FIQ:** `handle_fiq` exists, but the GIC is not configured to generate FIQs.

`irq_timer` test on the board: `make img APP=irq_timer`, same flashing step. Observed:

| Action | Observed |
|---|---|
| Boot | `MCT counter advancing: 0x19 -> 0x428d`, `GIC ready`. The counter comes stopped from U-Boot, `Mct::start` starts it |
| Every second | 11 ticks; the first `24000213`, the rest `24000000` ± 12 with no accumulating drift |
| Typing characters | Echo works between ticks, the IRQ return does not disturb the main loop |

The counter delta is the period itself and does not validate real time: for 24 MHz, 60 ticks in
60 seconds should be measured with a stopwatch.

## MMU and caches

`mmu::enable()` turns on the identity-mapped MMU, the L1 caches and branch prediction. Details
are at the top of `exynos_4412_rs/src/mmu.rs`.

| Range | Mapping | Reason |
|---|---|---|
| `0x0000_0000..0x0010_0000` | fault | catches null pointers |
| `0x0010_0000..0x4000_0000` | Device, execute-never | SYSRAM and peripherals, no speculation |
| `0x4000_0000..0xBEA0_0000` | Normal WBWA, Shareable | DRAM from the DTS |
| `0xBEA0_0000` and above | fault | memory outside the DTS, speculation must not reach it |

- **Table:** Generated at compile time and placed in `.rodata`; since it is never written, it
  needs no cache maintenance. The image grows by a 16 KiB-aligned section because of this.
- **`SCTLR.A` stays on:** The code is `strict-align` anyway, a real unaligned access faults.
- **Atomics:** After `enable`, `fetch_add` and `AtomicU64` may be used and are valid across all
  started cores (Shareable mapping, `ACTLR.SMP`, SCU).
- **L2:** Comes off from U-Boot on this board. `L2c::enable()` is called after `mmu::enable()`;
  since the board runs Non-secure, this is done via SMC to the secure firmware (`firmware.rs`). The
  call order and values were verified against Linux 5.15 and the board's vendor kernel.
- **DMA:** L1 and L2 cache maintenance for hardware that uses DRAM directly does not exist yet.

Test on the board: `make img APP=mmu_test`, same flashing step. Observed:

| Output | Observed |
|---|---|
| `Security state` | `Non-secure` (reading `SCR` was UNDEF, and returning from the handler also worked) |
| `ACTLR` | `0x00000047`: the `SMP` bit comes set from U-Boot |
| `NSACR` | `0x00060c00`: cp10/cp11 (VFP) allowed |
| `L2:` | `CTRL 0x0`, off. `AUX 0x02070000`: 16 ways |
| `SCTLR` | `0x00c5007a` → `0x00c5187f` (`M`, `C`, `Z`, `I` turned on) |
| Speed | 124540 µs → 1350 µs, 92 times (16 KiB, L1 only) |
| `LDREX/STREX` | `5 7`, no hang |
| `tick N  interrupted FP checks` | Every tick interrupted an FP check, `d0` and `d17` were preserved |
| `t` | `DataAbort`, `DFSR=0x00000005`, `DFAR=0xbea00000` |
| `x` | `PrefetchAbort`, `IFSR=0x0000000d`, `IFAR=0x10050000` |

### L2 test

`make img APP=l2_test`, same flashing step. Observed:

| Output | Observed |
|---|---|
| `Chip ID` | `0xe4412211`, revision `0x11`: prefetch value `0x71000007` is suitable |
| `MCT rate` | `23998400` Hz, 67 ppm off from 24 MHz |
| `L2:` (before) | `CTRL 0x0 AUX 0x02070000 TAG 0x777 DATA 0x777 PREFETCH 0x0 POWER 0x0` |
| `L2:` (after) | `CTRL 0x1 AUX 0x7e470000 TAG 0x110 DATA 0x120 PREFETCH 0x71000007 POWER 0x3` |
| `Data written before L2 enable is correct` | No data loss across the switch |
| Speed, L2 off | 16 KiB 4 ns, 256 KiB 7 ns, 2 MiB 7 ns |
| Speed, L2 on | 16 KiB 4 ns, 256 KiB 4 ns, 2 MiB 4 ns |
| `tick N ... no errors` | 11 seconds, about 2100 passes of 2 MiB verification, no errors |

This measurement is sequential access with a resolution of 1 ns. 4 ns is the duration of the loop
itself. That 2 MiB also speeds up comes not from L2 capacity but most likely from prefetch; L2's hit
latency cannot be considered measured by this test. That requires a random-access test.

## SMP

`exynos_4412_rs::smp` starts CPU1-3. The order on CPU0: `mmu::enable()` (also turns on the SCU),
optional `L2c::enable()`, `Gic::init()`, then `smp::start_cpu(n, entry)` for each core. A started
core arrives at `entry(n)` in SVC mode, with interrupts off, on its own stacks, with VFP and MMU + L1
on; if it will take interrupts, it first calls `Gic::init_this_cpu()`.

- **Bring-up path:** Core power (PMU), the entry address in SYSRAM (`0x0204F01C + 4n`),
  `SMC_CMD_CPU1BOOT` and an SGI; repeated until the core reports. The interface was taken from Linux 5.15
  (`platsmp.c`, `firmware.c`) and the vendor kernel. Linux was running on 4 cores on this board;
  `maxcpus=1` in the DTS had no effect (`CONFIG_CMDLINE_FORCE`).
- **Memory before the MMU:** A secondary core does not touch normal memory until its MMU is on: CPU0's cache may hold
  stale lines. Its stacks are therefore only loaded into registers, first use comes after the MMU.
- **Stacks:** `link.x` reserves one block per core (default 5 × 16 KiB). This space takes no room in
  the file, but is deducted from `RAM` in `memory.x`.
- **GIC:** The distributor and CPU interface have a separate per-core window (`0x4000` apart). `Gic`
  selects the calling core's window on every access. SPI configuration must be done from one core at a time.
- **Console:** The lock takes effect with the first `start_cpu`; every `print!` is atomic across cores.
  A core that faults while writing finishes its report, but halts holding the lock and cuts off the output.
- **`smp::SpinLock<T>`:** IRQs are off on the holding core. Panics with the MMU off.
- **Error trace:** If a started core has to halt, it writes `<n>V` (no VFP permission) or `<n>S`
  (`ACTLR.SMP` could not be set) to UART2 without the lock.
- **Wake SGI:** On the board every secondary core received one `smp::WAKE_SGI`, even though
  `init_this_cpu` tries to clear it. The interrupt handler must ignore it.
- **Not yet present:** Core shutdown (hotplug), FIQ.

Test on the board: `make img APP=smp_test`, same flashing step. Observed (Cortex-A9 r3p0, `MIDR 0x413fc090`):

| Test | Observed |
|---|---|
| CPU1-3 bring-up | Each about 1.7 ms; `ACTLR` `0x47`, MPIDR `0x80000a01..03` |
| SCU | Firmware leaves it off, turned on by a Non-secure write; power state `0x30003` from the start |
| Atomic counter | 4,000,000 / 4,000,000, about 190 ms |
| Locked counter | 800,000 / 800,000, about 95 ms |
| Data handoff (256 KiB, 64 rounds) | No errors |
| SGI ring | 1000 laps, about 3 µs per lap |
| MCT tick (SPI routed to CPU1) | 1 per second, none lost over 60 s; did not go to other cores |
| CPU0 echo | Works with the locked console, while the other cores are writing |

Random access latency (pointer chasing, 32-byte stride):

| Working set | L2 off | L2 on |
|---|---|---|
| 16 KiB | 6 ns | 6 ns |
| 128 KiB - 768 KiB | 134-140 ns | 28 ns |
| 2 MiB | 138-140 ns | 97-109 ns |
| 8 MiB | 138-140 ns | 135-156 ns |

**Known limitation:** Do not leave cores in an endless, write-only loop (`str; add; b`) on a cache line
that other cores also write. On the board, in three attempts out of three, the other cores writing to
that line stopped without reporting an error. The same loop on a line nobody else touches caused no
problem for 40 s and kept taking interrupts. A loop that pauses once every 1 million writes on the shared
line also caused no problem. The cause is unknown and has not been matched to a published erratum.

## MCT local timer

`drivers::timer::LocalTimer` is each core's own interval timer. All of them interrupt only their own
core, through a single banked PPI (`soc::MCT_LOCAL_IRQ`, ID 28).

```rust
Gic::new().enable(soc::MCT_LOCAL_IRQ);                   // on the core that takes the interrupt
LocalTimer::this_cpu().start_periodic(24_000);           // counts at 24 MHz: 1 kHz
LocalTimer::this_cpu().clear_interrupt();                // in the interrupt handler
```

- **Period:** Given in global counter units (24 MHz), must be even and at least 32: the timer first
  divides the clock by 2. On the board, `ICNTB = n` measured a period of `2n + 2`; the driver accounts for this.
- **One-shot:** Call `stop` in the handler before `clear_interrupt`.
- **Access:** Every core can program every timer (`LocalTimer::new(n)`), but two cores must not program
  the same timer at the same time.

Test on the board: `make img APP=timer_test`, same flashing step. Observed (5 s measurement; period in counter units):

| Core | Rate | Ticks | Mean period | Min - max |
|---|---|---|---|---|
| CPU0 | 100 Hz | 500 | 239999.975 | 239984 - 240005 |
| CPU1 | 1 kHz | 5000 | 23999.998 | 23991 - 24005 |
| CPU2 | 500 Hz | 2500 | 48000.000 | 47995 - 48005 |
| CPU3 | 250 Hz | 1250 | 95999.996 | 95996 - 96004 |

In free running, exactly `100 1000 500 250` ticks were seen every second for 40 s.
The stopped core's timer did not affect the others, the one-shot gave a single tick in both attempts,
and there were no unexpected interrupts. CPU1-3 woke from `wfi`. No tick going to the wrong core was seen.

## GPIO

`drivers::gpio` gives pins in the form `gpio::GPX1.pin(3)`: function, level, pull and drive strength.
On GPX0-GPX3 pins, `Pin::eint()` gives the wakeup interrupt. Group interrupts of the other banks are
not present. The bank table is the same as Linux `pinctrl-exynos-arm.c:355-414`.

- **Cores:** Only one core at a time should change pins of the same bank. An interrupt handler on the
  same core may change pins.
- **No protection:** The driver does not prevent changing a pin the board needs.
- **Untried controllers:** The `GPZ` and `GPV*` banks were never accessed on the board; their clocks or
  power domains may be off.

`gpio_test` on the board: `make img APP=gpio_test`, same flashing step. Observed:

| Step | Observed |
|---|---|
| Read | `gpa1-0`, `gpa1-1` `Alt(2)`, `gpk0-0` `Alt(3)`; same as the DTS, the bank addresses of two controllers verified |
| Output | `gpd0-1` readback 6/6, the backlight blinked 3 times, the pin returned to its previous function |
| Pull | `vol+` field read back as `Down` and `Up`; level 1 in both states, so the line has a pull-up on the board |
| Polling | All three keys gave `pressed` / `released` |
| Interrupt | GIC 59 (power) and shared GIC 64 (`vol+`, `vol-`) worked; empty and wrong interrupts 0 |
| Mask | With `vol+` masked, edges were not counted; after unmasking, counting continued |

- **Contact bounce:** The volume keys produce more than 2 edges per press (10 edges seen in one interval).
  The power key gave exactly 8 edges in 4 presses. The driver has no filter; edge-driven code must do its
  own filtering. 5 ms polling showed no bounce in this test.
- **State from U-Boot:** `gpd0-1` comes as `Output` and high, not in its PWM function. That is why the
  backlight blinks directly with this pin.

## I2C

`drivers::i2c` is a polling master driver for controllers 0-7:
`I2c::new(Bus::I2c3).init(100_000, 100)`, then `probe`, `read`, `write`, `write_read`.
`init` opens the clock gate and configures the pins. It selects the rate based on `aclk100`, read from `drivers::cmu`.

- **Missing:** Interrupt operation and bus recovery are not implemented. A stuck line returns an error.
- **HDMI bus:** `i2c_8` is out of scope because it behaves differently.
- **Clock:** `drivers::cmu` does not change PLLs, muxes or dividers; it only reads them and opens the I2C gates.

`i2c_test` on the board: `make img APP=i2c_test`, same flashing step. It sends only addresses and
register numbers, so multi-byte writes are first exercised by `pmic_test`. Observed:

| Item | Observed |
|---|---|
| Clocks | MPLL 800 MHz, `aclk100` 100 MHz |
| Fuel gauge, `i2c_3` | Absent `0x37` gives `AddressNack`, then `0x36` answers; 3853 mV; 1000 reads, 0 errors |
| PMIC, `i2c_0` | Device ID `0x02`; 1000 reads, 0 errors |
| Time per read | 497 us at 97656 Hz, 107 us at 390625 Hz |
| LDO6, 24, 25, 26 | All mode 0, off, as U-Boot leaves them |
| `i2c_6`, `i2c_1`, `i2c_7` | Lines `1 1`, devices give `AddressNack` |
| `i2c_5` | Lines `0 0`, `Timeout`: both lines are held low with the touch controller unpowered |

- **Bus rate:** The times fit about 3 extra bus clocks and 7.5 us of software per read. That is a fit of
  two unknowns to two measurements, not a measurement of SCL.
- **State of charge:** The gauge's raw value is 162. The vendor kernel halves it
  (`max17040_battery.c:178`), giving 81 %. This board's DTS lacks `maxim,double-soc`, without which the
  Linux 5.15 driver does not halve it. The 81 % has not been compared with another reading.

## MAX77686

`drivers::max77686` reads LDO states and turns LDOs on and off. U-Boot leaves the peripheral rails off,
so their devices do not answer on I2C until `Max77686::enable_ldo` turns them on, despite
`regulator-boot-on` in the DTS.

- **Voltages:** The driver only checks that the LDO can produce a voltage. Take it from the DTS: a wrong
  value can damage what the rail feeds. Turning off the wrong LDO can stop the board.
- **Not covered:** BUCK1-4, and the GPIO control of BUCK8, BUCK9 and LDO20-22.

| Rail | Feeds | From U-Boot | Set by `pmic_test` |
|---|---|---|---|
| LDO6 `vdd_tsp_1v8` | Touch controller | Off, 1800 mV | 1800 mV |
| LDO26 `vdd_tsp` | Touch controller | Off, 2700 mV | 3000 mV |
| LDO24 `vdd_sensor` | Gyroscope | Off, 3000 mV | 3000 mV |
| LDO25 `vdd_audio` | Codec | Off, 3000 mV | 3300 mV |

`pmic_test` on the board: `make img APP=pmic_test`, same flashing step. It writes only these four
registers, then restores them. Observed:

| Item | Observed |
|---|---|
| Rails | All four read back mode 3 at the voltages above; all restored and read back |
| Light sensor `0x44` | `AddressNack` with LDO24 off, answers with it on |
| Codec `0x1a` | `AddressNack` with LDO25 off, answers with it on |
| `i2c_5` lines | `(0, 0)` with LDO6 and LDO26 off, `(1, 1)` with them on, `(0, 0)` again after restoring |
| Other rails from U-Boot | Of the DTS `always-on` LDOs only 22, 24, 25 and 26 are off. BUCK5, 6 and 7 are on |

The touch controller needs more than its own rails. This power-up works from a power-on, in one reset:

1. Set BUCK7 to 3300 mV and BUCK6 to 2000 mV, the values Linux uses. U-Boot leaves 2000 and 1350 mV.
2. Hold reset (`gpb-4`) low and wake (`gpm3-3`) high.
3. Turn on LDO26 at 3000 mV, then LDO6 at 1800 mV.
4. After 70 ms release reset, and after 300 ms read the controller at `0x4a`.

Observed: family `0xa2`, version `0x20`, build `0xab`, matrix 32 x 52, 33 objects, 100 reads with 0 errors.
That build matches the vendor firmware `mXT1664S__APP_V2-0-AB`. No touch input is read yet: there is no
driver for the controller.

| Touch power-up | Result |
|---|---|
| Own rails only | Nothing on `i2c_5`, CHG high |
| BUCK7 raised first | Nothing on `i2c_5`, CHG high |
| BUCK7 and BUCK6 raised first | `0x4a`, CHG low |
| BUCK6 and BUCK7 raised while it is already powered, one reset | `0x26`, the bootloader address (`atmel_mxt_ts.c:480-484`) |
| The same, one more reset | `0x4a` |

BUCK6 is needed with BUCK7 raised. BUCK6 alone has not been tried, so BUCK7 may not be needed. Why BUCK6 matters
is not known: nothing read here says what it feeds.

Other findings:

- **Gyroscope:** Nothing answers on `i2c_6`, and `i2cdetect -y 6` under Linux finds nothing either, with
  `vdd_sensor` on. It is not fitted on this unit, although the DTS has a node for it.
- **BUCK8 and BUCK9:** Their mode bits read 0. BUCK8 feeds the eMMC, which works, so 0 does not mean off
  for these two: Linux has GPIO control for them (`max77686-regulator.c:103-107`).

## QEMU

A `qemu` target was not added, by project decision; testing is done on the real board.

## TODO(verify)


The UART4 address is not in the Linux 5.15 DTS. Its source is the vendor kernel (`/home/developer/willow`,
3.0.101, details in `soc.rs`). Comparing it against the board's user manual would be good.
