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
- `display_test/`: panel bring-up with FIMD: state report, scan checks, test patterns
- `gpio_test/`: GPIO pins and key interrupts test
- `i2c_test/`: I2C bus test, performs only reads on the devices
- `pmic_test/`: MAX77686 test, turns peripheral rails on and restores them
- `mmc_test/`: eMMC read test, partition table, no writes
- `wifi_test/`: BCM4334 WiFi chip bring-up over SDIO: loads firmware, scans, joins a WPA2 network, DHCP, ping, TCP and a small HTTP server
- `power_test/`: restart, restart into fastboot, power off, and the same from the keys

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
| `0xBEA0_0000..0xC000_0000`, `0xC100_0000` and above | fault | memory outside the DTS, speculation must not reach it |
| `0xC000_0000..0xC100_0000` | Normal non-cacheable, execute-never | `mmu::DRAM_UNCACHED`: the first 16 MiB of DRAM again, past the caches, to check cache maintenance |

- **Table:** Generated at compile time and placed in `.rodata`; since it is never written, it
  needs no cache maintenance. The image grows by a 16 KiB-aligned section because of this.
- **`SCTLR.A` stays on:** The code is `strict-align` anyway, a real unaligned access faults.
- **Atomics:** After `enable`, `fetch_add` and `AtomicU64` may be used and are valid across all
  started cores (Shareable mapping, `ACTLR.SMP`, SCU).
- **L2:** Comes off from U-Boot on this board. `L2c::enable()` is called after `mmu::enable()`;
  since the board runs Non-secure, this is done via SMC to the secure firmware (`firmware.rs`). The
  call order and values were verified against Linux 5.15 and the board's vendor kernel.
- **DMA:** For hardware that uses DRAM directly, `mmu` has three range calls over L1 and L2.
  Ranges must be whole 32-byte lines (`mmu::DCACHE_LINE`): a partial line would drop or write back
  a neighbour's data, so they assert.
  - `clean_dcache_range` before the hardware reads: L1, then L2, write back to DRAM.
  - `discard_dcache_range` before the hardware writes: L1, then L2, dropped without writing back,
    so no dirty line can be evicted over what the hardware writes.
  - `invalidate_dcache_range` after the hardware wrote: L2, then L1, dropped, so the CPU reads
    DRAM. L2 first, so L1 cannot refill from a stale L2 line. Nothing drives DMA yet.

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
| `L2 revision` | `0x08`, r3p2: errata 753970 and 769419 are fixed in it |
| `Discard of 128 KiB` | 16 of 16 rounds `OK`, 115 µs each. The caches wrote 48 words out before the drop (3 rounds) and 24 during it (round 1); nothing after |
| `Invalidate of 128 KiB` | 16 of 16 rounds `OK`, 115 µs each, reads matched DRAM right after. 8 words written out before (round 11) and 8 during (round 1) |
| Same, judged against the old values | 1 to 3 rounds in 8 or 16 "failed": the new value of 1 to 6 lines was in DRAM. Through `mmu::DRAM_UNCACHED`: in Invalidate rounds 3 and 16 already before the drop, in Discard round 1 during it. Afterwards, DRAM never changed again and reads always matched it |
| `SCU diagnostic` | `0x00000000`, and stays so after writing bit 0: Non-secure cannot set the 764369 workaround |
| Speed, L2 off/on (this run) | 256 KiB and 2 MiB: 12 ns off, 6 ns on |

This measurement is sequential access with a resolution of 1 ns. 4 ns is the duration of the loop
itself. That 2 MiB also speeds up comes not from L2 capacity but most likely from prefetch; L2's hit
latency cannot be considered measured by this test. That requires a random-access test.

The drop tests put old values in DRAM with `clean_dcache_range`, write new values over 128 KiB
and one guard line on each side, then drop the 128 KiB, 16 rounds each. Each round first
dirties 2 MiB elsewhere, so the L2 cache is busy writing lines out while the range comes in.
DRAM is read through `mmu::DRAM_UNCACHED`, a non-cacheable view of the first 16 MiB of DRAM at
`0xC000_0000`: after the clean, before the drop, after it, and after reading 2 MiB elsewhere,
which evicts the range from both caches, so a dirty line left behind would have reached DRAM by
then. That DRAM still holds the old values before the drop, while the caches hold the new ones,
shows that the view really goes past the caches.

- A round passes if the clean put every old value in DRAM, DRAM does not change after the drop,
  reads through the caches match DRAM after the 2 MiB, and the guard lines keep the new values.
  `invalidate_dcache_range` must also match DRAM right after the drop.
- What the CPU wrote may reach DRAM before the drop or during it, and the summary counts those
  words. Before a device starts, that does no harm: the device writes over it. The board does
  both. Before: a busy L2 cache evicts lines on its own. During either drop, mostly in round 1:
  the cause is not known. Possibly `DCIMVAC` writes a dirty line out, which the architecture
  allows, and the L2 cache sends it on to DRAM. The first test design took these for failures.
- Right after a discard, a read may still differ from DRAM: a line can come back clean into L1
  from L2 between the two steps. It is not dirty and never reaches DRAM.
- Linux's workaround for Cortex-A9 erratum 764369 sets bit 0 of the SCU diagnostic register:
  a maintenance operation by address on Inner Shareable memory, which is all of DRAM here, may
  not reach its point of coherency. The vendor kernel enables it in all its configurations, but
  writes the register directly, as `Scu::enable` does, and from Non-secure that write is
  ignored. Only the other half, a `dsb` before the maintenance loops, is in effect. The tests
  pass without the bit.

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
  Since then, `entry!` turns it off before `main`, so `gpio_test` shows it low.
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

## Display

`drivers::display` drives the FIMD on the board's panel, `samsung,ltn101al03`, 1280x800 over LVDS:
one full-screen window in XRGB format, from a `Framebuffer` that the application owns. Vsync is
polled. `display_test` shows it working.

- **Order:** `Fimd::power_on` (domain, clocks, stops whatever U-Boot left running),
  `configure_pins`, draw and `flush`, `Fimd::start`, `set_lvds(true)`, wait `LVDS_SETTLE_MS`,
  `set_backlight(true)`.
- **Caches:** The FIMD reads DRAM and does not see the caches, so drawing appears only after
  `flush` or `flush_rect`. They use `mmu::clean_dcache_range`, which cleans L1 and then L2.
- **Clock:** `sclk_fimd0` runs from `mout_mpll_user_t`, undivided, as in the vendor kernel, and the
  FIMD divides it to the pixel clock: 800 MHz / 12 = 66.67 MHz, about 58 frames per second.
- **Not present:** Other windows, System MMU, display interrupts (they come through the interrupt
  combiner), brightness control (the backlight pin needs the PWM; here it is fully on or off).

Every application starts with the panel off: `entry!` calls `display::panel_off` before `main`,
which turns off the backlight, then the LVDS transmitter, both left on by U-Boot. This saves the
battery. An application that shows something turns them on again, as `display_test` does.

`Fimd::set_solid_color` shows one color from a register, without reading DRAM. It tells a panel
problem from a framebuffer problem.

The framebuffer takes 4 MiB of `.bss`, so `display_test` has a `memory.x` of 12 MiB. Nothing of it
goes into the image.

Test on the board: `make img APP=display_test`, same flashing step. Observed:

| Check | Result |
|---|---|
| Buffer address in use after start | `0x40019000`, as programmed |
| Refresh over 120 frames | 58.188 Hz, computed 58.025 Hz |
| Full-screen flush (4 MiB) | about 5.4 ms |
| Moving bar | 58 frames/s, slowest frame 475 us |

The measured refresh is 0.28% above the computed one, more than the error of the MCT rate. The
cause is not known.

U-Boot leaves the display running, so the panel works from the moment the application starts:

| Register | Left by U-Boot | Set by `Fimd::start` |
|---|---|---|
| `sclk_fimd0` | 400 MHz (`DIV_LCD0` field 1) | 800 MHz |
| `VIDCON0` | `0x00000143`, divider 6 | `0x000002f3`, divider 12, free-running VCLK |
| `VIDCON1` (low 12 bits) | `0x060` | `0x660`, VCLK runs in blanking |
| `VIDTCON0`, `VIDTCON1`, `VIDTCON2` | `0x000b0002`, `0x003f0f2f`, `0x0018fcff` | the same |
| `WINCON0`, `SHADOWCON` | `0x0000802d`, `0x00000001` | the same |
| Buffer | `0x5f800000` | the application's |

So the timing derived from the DTS matches U-Boot's, and the panel also runs without the two VCLK
settings taken from the vendor kernel. `GATE_IP_LCD0` reads `0xffffffff` before this code touches
it, so the System MMU gate this code opens was already open on this boot.

## Restart and power off

`drivers::power` restarts the chip with `SWRESET` and powers off by dropping `PS_HOLD`, so the long
hardware press of the power key is not needed. `power_test` tries each.

- **Boot mode:** `restart` takes a `BootMode`, written to `INFORM5` for U-Boot. `Fastboot` goes
  straight to flashing. These values come from the vendor kernel.
- **Charger:** With a charger connected, `power_off` does what the vendor kernel does: it restarts
  with `INFORM6` = 2 instead of cutting the power. On the board, dropping `PS_HOLD` with the charger
  connected left it running, and the application started again after this restart.
- **Keys:** `PowerKeys::service`, called from a main loop. Hold for 2 s, then release: power = off,
  power + volume up = restart, power + volume down = fastboot. It acts after release, so that a held
  power key cannot turn the board back on and U-Boot does not read a held volume key.
- **Limit:** Keys work only while the application calls `service`: `display_test` and `power_test`
  do. An application that has halted, panicked or hung still needs the long hardware press.
- **PMIC:** `restart` leaves the PMIC rails as they were. Only a real power off clears them.

Tested on the board with the charger connected (`Charger: DC yes, USB no`). `PS_HOLD_CONTROL` reads
`0x00005300` while running. Powering off with no charger was not checked from a log.

## eMMC (read-only)

`drivers::mmc` reads the board's eMMC through the DW MMC controller `mshc_0`. `partition::scan` parses
its MBR and the chain of extended boot records.

- **Read-only by construction:** The driver can send only `CMD8`, `CMD13`, `CMD17` and `CMD18`, with
  the controller's automatic `CMD12` as the stop. It has no write, erase or switch command.
- **U-Boot's setup is kept:** The card is not reset or initialized. U-Boot read this image from it, so
  it is ready, and bus width and clock stay as they are. `CMD6` would be needed to change them, and it
  writes to the card's `EXT_CSD`.
- **Limit:** A failed read may leave the card unusable until a restart, as there is no recovery.
- **Transfer:** Two ways, both polled, without interrupts.
  - `read_blocks`: the CPU reads the FIFO.
  - `read_blocks_dma`: the controller's DMA engine, the IDMAC, writes DRAM through a chain of up
    to 256 descriptors of 4 KiB, 1 MiB per command. The order of setup is Linux's
    (`dw_mci_submit_data_dma`, `dw_mci_idmac_start_dma`). `FIFOTH` is set the way
    `dw_mci_adjust_fifoth` sets it, for the DTS FIFO depth of 128. The buffer must start on a
    32-byte line in DRAM, and is discarded from the caches before the read and invalidated
    after it. Every return, error or not, stops the IDMAC first, so it cannot write the buffer
    after the call.

Test on the board: `make img APP=mmc_test`, same flashing step. Observed:

| Check | Result |
|---|---|
| Card | 30777344 blocks (15028 MiB), `EXT_CSD` revision 6, address 1, in transfer state |
| Hardware partition | User area; `PARTITION_CONFIG` `0x48` |
| Bus | Controller `CTYPE` 8-bit; `HS_TIMING` 1; FIFO 32 bits |
| Partition table | MBR at block 0 with an extended chain; 10 partitions |
| `bootcmd` block `0x10040` | Start of p5, begins with `ANDROID!` |
| Ext superblock magic | p3, p9, p10: yes. p8: no, it reads `0x3ba0` |
| Single against multiple block reads | Same data |
| 16 MiB from SYSTEM, read twice | Same data, about 15800 KiB/s with 1 MiB reads, 16529 KiB/s with 2 MiB reads |
| DMA: 1, 9, 2048, 4096 blocks | Same data as polled, guard lines kept, with both Linux's `FIFOTH` `0x503f0040` (64-word bursts) and U-Boot's `0x20400040` (8-word bursts) |
| DMA: 16 MiB from SYSTEM, read twice | Same data as polled, 74902 KiB/s (U-Boot's `FIFOTH`), 4.5 times the polled speed |

In the DMA cases, each read is first done polled. Then stale bytes go over the buffer and guard
values into a line on each side, all cleaned to DRAM. The DMA read must then give the polled
data, and DRAM, read through `mmu::DRAM_UNCACHED`, must still hold the guard values. 9 blocks
cross a descriptor off a 4 KiB boundary. 4096 blocks take two commands.

The first version left `IDINTEN` at 0. The card finished each read (`DATA_OVER`, then `ACD`), but
`IDSTS` never showed `RI`, and data stayed in the FIFO. With `IDINTEN` set as Linux sets it
(`NI | RI | TI`), every case passed. Interrupts still stay off, since `CTRL_INT_ENABLE` is clear.

`EXT_CSD` `BUS_WIDTH` reads 0 although the controller is in 8-bit mode, and data arrives without CRC
errors. p10 is 28295104 blocks on the card, which ends it exactly where the extended partition ends.

## WiFi

The board has a BCM4334 on `sdhci_3`, over 4-bit SDIO. `drivers::sdhci` is the host controller,
`drivers::sdio` the card protocol, and `drivers::bcm4334` the chip's power, its firmware, the
control channel to it, scanning, and joining. `wpa` is the client's side of the WPA2-Personal
handshake and `crypto` the algorithms it needs: the firmware of this board leaves both to the host.
`drivers::wifi_device` makes the chip a device of the `smoltcp` network stack.

- **Sources:** Broadcom publishes no description of this interface, so the chip's side is derived
  from Linux's `brcmfmac`, under the ISC license: see `THIRD_PARTY_NOTICES.md`. This is the one
  exception to using Linux only as a register map.
- **Firmware:** The firmware and NVRAM files are not in this repository. `wifi_test` takes them at
  build time from the directory in `WIFI_FIRMWARE_DIR`, and stops before loading without it.
- **Clock:** This controller has no usable divider of its own, so the card clock is `sclk_mmc3`,
  set in the CMU: 400 kHz to start, then the data rate `wifi_test` asks for. The board's DTS gives
  16 MHz, with high speed off. `wifi_test` asks for 50 MHz, or for `WIFI_SDIO_MHZ` when it is set.
  Above 25 MHz, the most of default speed, the card is switched to high speed first, with `EHS` in
  its `CCCR`; a card that does not offer it is an error. The controller has no high speed setting
  of its own (`sdhci-s3c.c:573`). From 25 MHz on, it samples with other feedback settings
  (`sdhci-s3c.c:290-291`).
- **Transfer:** Polled through the buffer port, with no DMA and no interrupts.
- **Flow control:** The firmware says in each frame it sends how many more it can take. While it
  has no room, `drivers::wifi_device` tells `smoltcp` it cannot send, so the stack keeps reading
  instead of dropping frames, such as acknowledgements, while it waits.
- **Power:** LDO22 and the chip's enables stay on after the test, and the PMIC keeps its state over
  a restart. The chip is reset each time it is powered up.
- **eMMC:** Not involved: it is on another controller and other pins.

Test on the board:
`WIFI_FIRMWARE_DIR=<directory> make img APP=wifi_test`, same flashing step.

Observed in the first stage, without firmware:

| Check | Result |
|---|---|
| Rails found after U-Boot | LDO22 off; of the 32 kHz outputs, only `AP` on |
| Card | OCR `0xffff00`, 2 functions, address 1 |
| Vendor and device | `0x02d0`, `0x4334` |
| Chip ID | `0x16034334`: chip `0x4334`, revision 3, AXI backplane |
| Bus | 4 bits at 16 MHz |
| 10000 reads of the ID at that rate | No errors, about 9.6 us each |

Observed with the firmware, 6.10.191:

| Check | Result |
|---|---|
| Chip contents | PMU revision 17, 512 KiB of RAM, nine entries in its table of cores |
| 398010 bytes of firmware, and the NVRAM | Written and read back in 148 ms |
| Firmware | Starts, answers `ver` and `cur_etheraddr` |
| MAC | `44:39:c4:64:12:3d`, the chip's own, not the placeholder in the NVRAM file |

Scanning, on the board: 8 networks on both bands in 1212 ms, with the firmware reporting status 0.
The firmware has no `sup_wpa`, so the WPA2 handshake runs on the host.

Joining, on the board: an access point on channel 11 authenticated and associated the client, the
4-way handshake finished and the keys went in 1298 ms after asking to join. The key from the
passphrase took 77 ms. It then stayed connected for 30 s and received 12 frames, which the chip
decrypted.

`wifi_test` then gets an address by DHCP, pings the router and 8.8.8.8, and answers ARP and pings on
the network until restarted, all with the `smoltcp` crate over `drivers::wifi_device`. Only the
features this needs are on. The stack renews its lease, which no test has yet lasted long enough to
show.

TCP, both ways:

- **Out:** After the pings, `wifi_test` connects to `1.1.1.1:80`, asks for `/` and prints the first
  line of the answer, how many bytes came, and whether both sides closed cleanly.
- **In:** It then serves HTTP/1.0 on port 80, up to 4 connections at once. `http://<address>/` is a
  text page about the station; `http://<address>/data` is 1 MiB of text, and the serial log gives
  the time until the last byte was acknowledged. Other paths get 404, other methods 405.
- **Closing:** Each response closes its connection. The client is left to close first, so the 10 s
  wait after a close falls on its side and the socket can listen again at once. A connection with
  no whole request in 5 s is dropped, as browsers open some ahead of time.
- **Memory:** The buffers are on the stack: 8 KiB to send and 1 KiB to receive per server
  connection. The 8 KiB bounds the bytes in flight, and so the speed of `/data`.
- **Second port:** Port 8080 takes two connections, each with 64 KiB to send and 32 KiB to
  receive, to compare against. With one, a request that came while the last connection was still
  closing was refused. Their buffers are in `.bss`, the rest on the stack.
- **Upload:** A `POST` to `/upload`, of up to 64 MiB with a `Content-Length`, is read and hashed.
  The answer, also in the serial log, gives the bytes, the time and their SHA-1. `curl` asks
  first with `Expect: 100-continue`, and gets its answer.
- **Checking it all:** `wifi_test/soak.sh <address> [rounds] [port]` downloads `/data` and
  uploads 1 MiB of new random bytes each round, 100 rounds on port 8080 by default, and compares
  the SHA-1 of each with what it should be. It needs `curl`, `sha1sum` and `head`.

`soak.sh` on the board, at 50 MHz on channel 48, to a computer on a cable to the router:

| | Result |
|---|---|
| 100 downloads of 1 MiB | All with the right SHA-1. 6191 to 8939 KiB/s, half above 8626 |
| 100 uploads of 1 MiB | All with the right SHA-1. 781 to 1053 KiB/s, half above 967 |
| Serial log | No error |
| TCP data that came in | No segment after a gap, none sent again |

The first run had one connection on port 8080, and 158 of its 200 transfers were refused at once,
for the reason above; none of those that ran went wrong.

- **Uploads:** Nothing was lost, and the board waited for data about half the time. The sender
  said why (`ss -ti`): it was limited by the receive window 100 % of the time, and `tcpdump`
  showed the board offering 1462 bytes in its `SYN-ACK` and 1474 after, of its 32 KiB. The device
  told `smoltcp` it takes bursts of one frame (`max_burst_size`), and `smoltcp` then shrinks
  every window it offers to one frame of data (`iface/packet.rs:155`). The device no longer says
  so. Fed the same `SYN` on the development machine, `smoltcp` answers with 32768 instead of
  1462. On the board after that, `soak.sh` again had all 200 transfers right, with uploads at
  4338 to 4675 KiB/s, half above 4591, and downloads as before, half above 8649. Of the 74085 TCP
  segments that came in, none came after a gap or again. Where an upload's 249 ms went:
  receiving 125 ms, 736 frames at 170 us each, 166 us of it the bus read; the rest, hashing and
  the stack, 81 ms; empty polls 41 ms. Reading a frame takes longer than writing one (114 us):
  it takes two `CMD53`s, the first for the header.
- **Frames before acknowledgements:** `wifi_test` also takes frames in one at a time, lets the
  servers read after each, and sends the acknowledgements last. `Interface::poll` takes in all
  that is waiting first, so a burst would be acknowledged before the server read it, with the
  window nearly shut: over `smoltcp`'s loopback with the same buffers, the window offered went
  from alternating between 32768 and 648 to 32768 throughout.
- **Where the time goes:** When `/data` is all acknowledged, the log splits the time since the
  request into sending frames, receiving them, polling the chip for nothing, and the rest (the
  stack, HTTP and the console). It gives the frames each way, the time each took, how much of
  sending was bus writes and waiting for room in the firmware, and how often the stack was told
  there was no room. It also counts the TCP segments with data that came in, and of those the
  ones that start past where the last ended (something was lost or late) and the ones that start
  before (data sent again). Other traffic in that time counts too, so test one transfer at a time.
  The rate and the signal are asked only after joining: the firmware's answer would cost the
  frames that come meanwhile.

Measured on the board, 1 MiB of `/data` to `curl` each, with the bus at 16 MHz. The access point's
band was not recorded:

| | Port 80, 8 KiB in flight | Port 8080, 64 KiB in flight |
|---|---|---|
| Until all acknowledged | 558 ms, 1835 KiB/s | 233 ms, 4394 KiB/s |
| Sending frames | 192 ms, 767 frames | 190 ms, 721 frames |
| Of that, bus writes | 189 ms, 246 us each | 187 ms, 260 us each |
| Waiting for room in the firmware | 0 ms | 0 ms, told the stack 117 times |
| Empty polls | 238 ms | 18 ms |
| Receiving, and the rest | 22 and 103 ms | 5 and 17 ms |

- **8 KiB:** The time with nothing to do is the wait for acknowledgements: the buffer, not the
  bus, sets the speed.
- **64 KiB:** 82 % of the time is bus writes. A frame of about 1532 bytes takes at least about
  192 us on 4 bits at 16 MHz, against the 260 us measured, so the bus is busy for about 3/4 of
  each write. The rest is the commands and the CPU at the buffer port. DMA could take only part of
  that rest away. The clock of the bus sets the speed here.

The same at 25 MHz, on channel 11 (2.4 GHz). The bus passed the same checks there: 10000 reads of
the ID without error, and the firmware written and read back in 112 ms instead of 149:

| | Port 80, 8 KiB in flight | Port 8080, 64 KiB in flight |
|---|---|---|
| Until all acknowledged | 563 ms, 1818 KiB/s | 231 ms, 4432 KiB/s |
| Sending frames | 137 ms, 768 frames | 136 ms, 721 frames |
| Of that, bus writes | 134 ms, 175 us each | 133 ms, 184 us each |
| Waiting for room in the firmware | 0 ms | 0 ms, told the stack 272 times |
| Empty polls | 262 ms | 52 ms |
| Receiving, and the rest | 18 and 144 ms | 5 and 36 ms |

Bus writes went down by 30 %, the speed did not move: at 25 MHz something past the bus sets it.
Which band the 16 MHz run used is not known, so the two do not tell whether that is the radio.
`WIFI_BAND`, `2.4` or `5`, keeps joining to one band, and the log gives the rate the firmware sends
at and the signal, after joining and after each `/data`.

The same at 25 MHz with `WIFI_BAND=5`: channel 48, the firmware sending at 150 Mbit/s, signal
-60 to -64 dBm:

| | Port 80, 8 KiB in flight | Port 8080, 64 KiB in flight |
|---|---|---|
| Until all acknowledged | 364 ms, 2813 KiB/s | 161 ms, 6360 KiB/s |
| Sending frames | 138 ms, 769 frames | 136 ms, 721 frames |
| Of that, bus writes | 134 ms, 174 us each | 133 ms, 185 us each |
| Waiting for room in the firmware | 0 ms | 0 ms, told the stack 135 times |
| Empty polls | 129 ms | 5 ms |
| Receiving, and the rest | 20 and 76 ms | 6 and 13 ms |

- **2.4 GHz:** The radio set the 4432 KiB/s above, not the bus.
- **5 GHz, 64 KiB:** 84 % of the time is bus writes again: the bus sets the speed.
- **Per frame:** A write takes about 60 us more than its bytes need on the bus, at 16 MHz
  (260 against 192 us) and at 25 MHz (185 against 123 us) alike: the commands, and the CPU at the
  buffer port. A faster clock shortens only the other part.

At 50 MHz in high speed, again on channel 48 at 150 Mbit/s, to a computer on a cable to the
router, so the frames cross the air once. The bus passed the same checks: 10000
reads of the ID without error, and the firmware written and read back in 77 ms:

| | Port 80, 8 KiB in flight | Port 8080, 64 KiB in flight |
|---|---|---|
| Until all acknowledged | 336 ms, 3047 KiB/s | 153 ms, 6692 KiB/s |
| Sending frames | 87 ms, 769 frames | 86 ms, 721 frames |
| Of that, bus writes | 83 ms, 108 us each | 82 ms, 114 us each |
| Waiting for room in the firmware | 0 ms | 0 ms, told the stack 245 times |
| Empty polls | 132 ms | 31 ms |
| Receiving, and the rest | 12 and 104 ms | 2 and 32 ms |

- **The bus:** Writes took 38 % less time than at 25 MHz, the speed went up by 5 %. Sending is now
  57 % of the time: the bus no longer sets the speed, and DMA would not either.
- **Past the bus:** The firmware ran out of room 245 times, so frames reach it faster than it
  sends them on. The radio, or the firmware, now sets the speed, at about 56 Mbit/s of TCP over
  a 150 Mbit/s link.
- **Not yet shown:** Whether 50 MHz holds over long use. Linux ran this board at 16 MHz with
  high speed off, for reasons not recorded.

From another computer on the network, for example:

```
curl http://<address>/
curl -o NUL -w "%{speed_download} bytes/s\n" http://<address>/data
```

(`-o /dev/null` instead of `-o NUL` outside Windows.)

Observed on the board; the access point's band was not recorded:

| Check | Result |
|---|---|
| Connecting to `1.1.1.1:80` | 5 ms |
| Its answer | `HTTP/1.1 301 Moved Permanently`, 381 bytes in 8 ms, closed cleanly by both sides |
| `/` and `/favicon.ico` from a browser | Both answered, the second with 404 |
| `/data` to a browser | 1 MiB acknowledged in 436 ms, 2348 KiB/s |

`/data` was also the first test that sent full 1514-byte frames, so the first that needed SDIO
writes of more than one block.

Observed on the board, on a 5 GHz access point:

| Check | Result |
|---|---|
| Joining, with the keys in | 1306 ms |
| DHCP | Address, router and name servers in 2140 ms |
| Pinging the router | 4 of 4, 5 ms for the first, which also finds its address, then 1 ms |
| Pinging 8.8.8.8 | 4 of 4, 46 to 47 ms |
| Pinged from another computer on the network | 4 of 4, 2 to 4 ms |

`WIFI_SSID` and `WIFI_PASSWORD` name the network at build
time and go into the image, so an image built with them must not be published. Only AES on both
traffic and broadcasts with a shared passphrase is supported: not TKIP, WPA3, or networks that demand
protected management frames. The handshake nonce comes from timing the scan, not a hardware random
generator: fine to test with, unproven beyond that.

`crypto` and `wpa` have tests that run on the development machine, with published vectors and values
from Python's `hashlib`. Their code needs nothing of the board, so a root that includes only them
builds with `rustc --test`.

## QEMU

A `qemu` target was not added, by project decision; testing is done on the real board.

## TODO(verify)

`drivers::cmu` opens the clock gate of the FIMD's System MMU, which stays off. Whether a closed gate
would block the FIMD's reads is not verified.

