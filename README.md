# Bare-metal Rust for the Exynos 4412

Bare-metal Rust on the Samsung Exynos 4412 (4 × Cortex-A9), started with `bootm` from the
board's own U-Boot. `exynos_4412_rs` provides the runtime and the drivers. An application
depends on this crate, supplies a `memory.x` and writes `main`.

- `exynos_4412_rs/`: startup, exceptions, drivers, console, panic, `link.x`
- `exynos_4412_mfc/`: the MFC v5.1 video codec through its firmware; **GPL-2.0-or-later**, unlike the rest (see below)
- `hello_world/`: console test
- `irq_timer/`: GIC and MCT test
- `mmu_test/`: MMU and cache test
- `l2_test/`: L2 cache test and MCT rate measurement
- `smp_test/`: CPU1-3 bring-up, inter-core memory and SGI tests, L2 latency measurement
- `timer_test/`: per-core MCT local timer test
- `display_test/`: panel bring-up with FIMD: state report, scan checks, test patterns
- `touch_test/`: mXT1664S touch controller: object table, touches drawn on the panel, five-point calibration
- `touch_async_test/`: the touch controller read with `await` on its CHG interrupt, with a 1 ms ticker on the same core
- `audio_test/`: WM8758 codec over I2S0: a tone and WAV samples on the headphones and the speakers, the FIFO filled by the CPU
- `audio_dma_test/`: the same through `pdma0`: a DMA ring of two halves refilled by the CPU, under I2C and CPU load
- `audio_async_test/`: the same, the halves of the ring awaited in an Embassy task on `pdma0`'s interrupt
- `gpio_test/`: GPIO pins and key interrupts test
- `i2c_test/`: I2C bus test, performs only reads on the devices
- `pmic_test/`: MAX77686 test, turns peripheral rails on and restores them
- `rtc_test/`: MAX77686 RTC test: reads the time, and sets it from a UART command
- `mmc_test/`: eMMC read test, partition table, polled and DMA reads, no writes
- `mmc_write_test/`: eMMC write test: writes the last 2 MiB of CACHE and restores them
- `ext4_test/`: reads the ext4 file systems on the eMMC with the `ext4-view` crate, no writes
- `fat_test/`: FAT32 on CACHE with the `embedded-sdmmc` crate: formats it if needed, writes, reads and lists files
- `wifi_test/`: BCM4334 WiFi chip bring-up over SDIO: loads firmware, scans, joins a WPA2 network, DHCP, ping, TCP and a small HTTP server
- `wifi_async_test/`: the same chip under Embassy as an `embassy-net` device, its runner task woken by the host wake line
- `gpu_clear_test/`: first Mali-400 frames: the pixel processors clear a buffer, then the screen, through their MMUs
- `mfc_test/`: MFC test: firmware start, then H.264 decoding checked picture by picture, then played on the panel (GPL, links `exynos_4412_mfc`)
- `mfc_play/`: plays an H.264 stream on the panel, over and over (GPL, links `exynos_4412_mfc`)
- `uart_boot/`: a loader for the boot partition: takes each new build over UART2 from minicom, or over WiFi by HTTP, and starts it, instead of fastboot
- `gpu_triangle_test/`: flat, shaded and textured triangles through the GP and the PPs, with shaders compiled offline by Mesa's lima compiler
- `gpu_cube_test/`: two textured (bilinear), lit cubes turning in perspective, drawn with the `gpu` runtime: two draws a frame, depth tested by the PPs
- `gpu_spin_test/`: a textured square turning on the screen, drawn by the GP and four PPs every frame
- `gpu_rect_test/`: rectangles filled by the pixel processors with CPU-drawn edges, against the CPU alone, cached and uncached
- `gpu_test/`: Mali-400 MP4 bring-up: its supply, power domain and clock, what its blocks say they are, their resets and interrupt lines
- `heap_test/`: the TLSF global allocator: collections, timings, 48 MiB, then four cores and their interrupt handlers allocating at once
- `async_test/`: Embassy on all four cores: timers, tasks, and channels between the cores
- `mmc_async_test/`: async eMMC: reads and writes awaited on the controller's interrupt, from CPU0 and CPU1
- `uart_tx_test/`: buffered console: `print!` sent on the UART's transmit interrupt, the ring overflowing, two cores printing, flush before a panic and a restart
- `cpufreq_test/`: `cpufreq::set_arm_mhz`: the CPU clock from 200 MHz to 1.4 GHz with `vdd_arm` (BUCK2), measured, timed and checked
- `standby_test/`: light standby on the power key: screen off, CPU at 200 MHz and 0.9 V, asleep until the next press
- `power_test/`: restart, restart into fastboot, power off, and the same from the keys
- `charger_test/`: MAX8903 charger: input power, charging and fault pins, charging enabled and disabled from the UART

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

### Loading over UART (`uart_boot`)

`uart_boot` stays in the boot partition and takes each new build over the console, so fastboot
is needed only to install it:

1. Once: `make img APP=uart_boot`, then flash `boot.img` as above. `make img` also leaves each
   image as `<app>.img`, so `uart_boot.img` stays after other builds overwrite `boot.img`.
2. Each build: `make bin APP=<app>`. In minicom, Ctrl-A S, `uartboot`, then
   `<app>.bin`. `tools/uartboot.py` sends it, and the board checks and starts it.
3. To load again, restart the board: the application's `r` key, or the reset button.

minicom runs `tools/uartboot.py` with its standard input and output on the port. It is set up
in `~/.minirc.dfl` (or with Ctrl-A O, File transfer protocols):

```
pu pname10          YUNYNuartboot
pu pprog10          python3 /path/to/tools/uartboot.py
```

The image is a `make bin` output with its zImage header; a `boot.img` also works, as its
Android header is skipped. It is sent at 115200 baud, about 11 KB/s. `uart_boot` puts it at
`0x50000000`, checks its length, CRC-32 and zImage magic, and jumps there with the MMU and
caches off and U-Boot's `r0`-`r2`. The application's startup then moves it down to its link
address, as it does when `bootm` starts it at `0x40009000`. The protocol is described in
`uart_boot/src/main.rs`. While it waits, `f` restarts into fastboot and `r` restarts.

Without minicom (Windows, or a script), `tools/uartboot.py --port COM5 <app>.bin` opens the port
itself through pyserial; `--monitor` keeps printing the board's output afterwards.

#### The saved application

`uart_boot` can also start an application by itself, from the CACHE partition (p8, FAT32):

- At power-on it waits 3 s for an image over the UART. If none starts arriving, it loads
  `FERRITE/FERRITE.BIN` from CACHE, checks it against the CRC saved with it and starts it.
- Without that file, or with a bad one, it keeps waiting for the UART: a saved application that
  crashes costs one restart, never the board.
- Holding **volume up** at power-on skips the saved application.
- **Test mode** skips it at every power-on, until turned off: the key `s` while `uart_boot`
  waits (or `httpboot.py --stay`, `POST /stay`) creates the empty `FERRITE/STAY` on CACHE, and
  `a` (or `--autoboot`, `POST /autoboot`) deletes it. Every image sent then runs, however late
  it comes. `s` also holds the current power-on.
- `uartboot.py --save <app>.bin` replaces it: `uart_boot` saves the image only after its CRC
  checked out (`uart_boot: saved FERRITE/FERRITE.BIN`), then starts it. Without `--save` an
  image runs from RAM only, as before.
- `--wait 30` waits up to 30 s for `uart_boot`'s banner and sends the header then, so the
  script can be started before the board is reset. It never sends blindly: stray bytes in
  U-Boot's countdown would stop its autoboot.
- `--reset` restarts a running ferrite application first (its restart message) when
  `uart_boot` does not answer, then catches the banner. `--debug` prints every byte received.

The file is the 16-byte wire header (`EXYNBOOT`, length, CRC) followed by the image
(`uart_boot/src/cache.rs`). CACHE is never formatted by `uart_boot`; `fat_test` does that.

The application's data file, `FERRITE/FLASH.BIN`, is replaced the same way: `uartboot.py --flash
FLASH.BIN` (magic `EXYNFLSH`) or `httpboot.py --flash FLASH.BIN` (`POST /flash`). Any bytes are
taken; once their CRC checks out they are written as they came, with no header, and nothing
runs: `uart_boot` waits for an image again, the autoboot of this power-on cancelled. An image can
follow in the same call: `--flash FLASH.BIN --save <app>.bin`.

#### Loading over WiFi

Built with the WiFi firmware and a network, `uart_boot` also takes images by HTTP:

```
WIFI_FIRMWARE_DIR=<dir> WIFI_SSID=<name> WIFI_PASSWORD=<passphrase> make img APP=uart_boot
```

(flashed once, as above; the passphrase goes into the image, so do not publish it). At
power-on:

1. 3 s for the UART alone, as before, with the MMU and caches off.
2. It joins the network, gets an address by DHCP (with the name `exynos`) and prints
   `uart_boot: http://<address>/ (exynos): POST /boot or /save`. HTTP and the UART are served
   together from then on; whichever starts receiving first owns the image.
3. 5 s after the address, the saved application boots if nothing has started arriving and no
   HTTP connection has come. Holding **volume up** at power-on, test mode (above), or having no
   saved application keeps it waiting for good.

Without the network, or if joining fails, step 2 is left out and the UART waits alone.

Each build, from the host:

```
make bin APP=<app>
python3 tools/httpboot.py --host <address> --wait 30 target/armv7a-none-eabihf/release/<app>.bin
```

`--wait 30` retries until the board answers, so the script can be started before the reset
button. `--stay` turns test mode on first (it stays on across restarts, `--autoboot` ends it), so
`--stay --wait 30 <app>.bin` is the way to run test applications next to a saved one. `--save` saves the image to CACHE first, as `uartboot.py --save` does. `make send
APP=<app> HOST=<address>` does both steps. `httpboot.py` sends the image's CRC-32 in an
`X-CRC32` header, which the board checks along with the zImage magic; plain
`curl --data-binary @<app>.bin http://<address>/boot` works too, without the CRC. The answer
comes once the image is checked (and saved), then the image starts.

After WiFi, `uart_boot` has the MMU on. Before jumping, it masks interrupts, turns off the timer
and UART interrupts it used, writes the image out to DRAM, then turns the MMU and caches off and
cleans and invalidates the L1 data cache, so the application starts in the state the UART path
gives it. On both paths, the WiFi chip is turned off before the jump (`bcm4334::power_off`):
held in reset, its radio off, its 3 V supply's enable low and its bus pins inputs, so it draws
nothing and its host-wake line stays low. An application that uses it powers it up from reset.

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
  backlight blinks directly with this pin. Since then, `entry!` turns it off before `main`, so
  `gpio_test` shows it low; `display::set_brightness` puts it in its PWM function (see Display).

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
That build matches the vendor firmware `mXT1664S__APP_V2-0-AB`. `drivers::mxt` does this power-up: see
[Touch screen](#touch-screen).

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

### RTC

`max77686::Rtc` reads the PMIC's real-time clock, which runs from the battery while the board is
off. It answers at its own I2C address, `0x06` (`rtc-max77686.c:20`), so none of its registers can
reach a regulator. A read sets `RBUDR` in `RTC_UPDATE0`, which copies the counters to the time
registers, waits 16 ms, then reads the seven of them; this is the only write. `Rtc::set` follows
`max77686_rtc_set_time`: the seven time registers in one write, then `UDR` in `RTC_UPDATE0`, then
16 ms. It writes nothing unless the RTC is in binary 24-hour mode and the time is a valid one from
2000 to 2099, the years the RTC holds. `time::DateTime` converts between dates and seconds since
1970.

Observed in `fat_test`: `RTC_CONTROL` `0x02`, binary and 24-hour mode as Linux left it, years
from 2000. Two reads 1216 ms apart were 1 s apart. The time was UTC, a few minutes behind the
host's clock at the moment the log was read, so Linux left the RTC in UTC.

`rtc_test` reads the RTC and writes it only on a UART command, `set YYYY-MM-DD HH:MM:SS` in UTC.
Observed:

| Command | Result |
|---|---|
| Start | `2026-11-03 09:41:10 UTC` |
| `set 2026-11-03 09:44:00` | Reads back 09:44:00, 0 s off (OK) |
| `read`, 24 s later | 09:44:24 |
| `set 2100-11-03 09:44:00` | `BadTime`, nothing written |
| `set 1980-02-26 09:44:00` | `BadTime`, nothing written |
| `set 2026-02-30 09:44:00` | Not a valid date, refused before the driver |

## Display

`drivers::display` drives the FIMD on the board's panel, `samsung,ltn101al03`, 1280x800 over LVDS:
one full-screen window in XRGB format, from a `Framebuffer` that the application owns. Vsync is
polled. `display_test` shows it working.

- **Order:** `Fimd::power_on` (domain, clocks, stops whatever U-Boot left running),
  `configure_pins`, draw and `flush`, `Fimd::start`, `set_lvds(true)`, wait `LVDS_SETTLE_MS`,
  `set_backlight(true)` (half brightness) or `set_brightness(level)`.
- **Caches:** The FIMD reads DRAM and does not see the caches. `Framebuffer::new` therefore draws
  through `mmu::DRAM_UNCACHED`, the uncached view of the first 16 MiB of DRAM, when the buffer is
  there, on whole cache lines, and the MMU is on: each write goes to DRAM, and `flush` and
  `flush_rect` only wait for the writes (`dsb`). Reads there wait for DRAM, so drawing that
  reads back, blending, wants `Framebuffer::new_cached`: then drawing appears only after `flush`
  or `flush_rect`, which use `mmu::clean_dcache_range`. `is_uncached` tells which.
- **Drawing:** `fill_rect` and `write_span` write blocks, the fast way uncached; `set_pixel` one
  pixel at a time. `write_span` copies a span of a line prepared in ordinary memory.
- **Clock:** `sclk_fimd0` runs from `mout_mpll_user_t`, undivided, as in the vendor kernel, and the
  FIMD divides it to the pixel clock: 800 MHz / 12 = 66.67 MHz, about 58 frames per second.
- **Brightness:** `set_brightness(0..=255)` runs PWM channel 1 (`drivers::pwm`, `TOUT1` on
  `gpd0-1`) at the device tree's 62.5 us period, high for the share asked; 0 and 255 make the pin
  a plain output. `set_backlight(true)` turns it on at `DEFAULT_BRIGHTNESS`, 128: full brightness
  drains the battery fast. The vendor kernel runs the channel with its output inverter on, so the
  output is high for `TCNTB - TCMPB` ticks. `mfc_play` and `g2d_test` step through the device
  tree's `brightness-levels` with `+` and `-`; verified by eye.
- **Not present:** Other windows, System MMU, display interrupts (they come through the interrupt
  combiner).

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
| Full-screen flush (4 MiB), cached | about 5.4 ms |
| Moving bar, cached | 58 frames/s, slowest frame 475 us |
| Full-screen flush, uncached | 0 us |
| Moving bar, uncached | 58 frames/s, slowest frame 86 to 102 us |
| Eight full-height bars, uncached | 2.2 ms |
| Gradient, 1 M pixels computed one by one, uncached | 57.7 ms through `set_pixel`, 46.3 ms through `write_span` |

The gradient is bound by its arithmetic: a division per pixel, in software on the Cortex-A9.

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

## Touch screen

`drivers::mxt` drives the mXT1664S on `i2c_5`. It powers the controller up as in the MAX77686 section,
reads its information block and object table, checks their CRC, and reads touch messages from T5 while
CHG (`gpx0-4`) is low. `touch_test` polls CHG; `touch_async_test` awaits its interrupt, GIC ID 52,
through `embassy::eint` (see [Async](#async-embassy)).

- **Configuration:** The controller keeps its configuration in non-volatile memory, written there by
  the vendor driver. This driver reads it. Its writes go to RAM only, and a reset brings back what was
  stored:
  - the calibrate command, which takes a new reference of the untouched screen;
  - run settings of T7 and T9, sent only if the stored ones would report nothing.

  The backup command, which would store a configuration, is never sent. Nothing is sent to the
  bootloader at `0x26` either, whose protocol replaces the firmware.
- **Calibration:** `touch::Calibration` maps controller positions to pixels with an affine map, fitted by
  least squares to touches on drawn targets. `touch_test` uses five targets: four 10 % in from the
  corners, and one in the middle. The map is printed, not stored.

`touch_test` on the board: `make img APP=touch_test`, same flashing step. It prints the object table and
the T9 and T7 settings, then draws the targets. After calibration each finger draws in its own color,
and press and release are printed. Keys: `c` calibrate again, `x` clear, `k` controller calibrate
command, `i` settings.

Observed, one finger:

| Item | Observed |
|---|---|
| Power-up | Answers at `0x4a` after one reset, 401 ms; CHG low with the reset message waiting |
| Information block | Family `0xa2`, version `0x20`, build `0xab`, 33 objects, CRC OK |
| T9 | CTRL `0x8f`, ORIENT `0x01` (X and Y switched), 11 touches, range 4095 x 4095, 2 instances |
| T7 | Idle and active 255 ms, as stored: nothing written |
| T6 after reset | `0x80` reset, then `0x00` running; configuration CRC `0x0342cc` |
| Calibration, five targets | Worst 12 px off with the range scaled onto the panel, 5 px with the fitted map |
| Fitted map | `x = 0.31574 rx + 0.00030 ry - 2.7`, `y = 0.00097 rx + 0.19425 ry - 2.7` |
| Drawing | Taps and strokes follow the finger; press and release reported for each |

The controller's own ORIENT already puts X across the panel and Y down it. The fitted scale is 1.1 %
above the plain one in X (0.31574 against 1279 / 4095 = 0.31233) and 0.4 % below it in Y, with an
offset of about 3 px. Several fingers at once have not been tried.

`touch_async_test` on the board: `make img APP=touch_async_test`. It draws with the map above, and every
5 s prints the messages, CHG interrupts, wake-ups that found no message, the time spent reading, and a
1 ms ticker's count and lateness. Observed, before the task yielded between messages:

| Check | Result |
|---|---|
| CHG data bit with the pin in its interrupt function | 0 with messages waiting, 1 after reading them: it follows the line |
| Nothing touching | No CHG interrupt and no report line; ticker 4965 ticks in 5 s, latest 18 us late |
| One finger | 140 messages, 140 interrupts in 5 s |
| Up to six fingers at once | Up to 1607 messages in 5 s against 229 interrupts: several read per interrupt |
| Wake-ups that found no message | 0 in all |
| Reading | 1.3 ms per message at 100 kHz: 2134 ms in the busiest 5 s |
| Ticker while fingers moved | 2925 to 4696 ticks in 5 s, latest up to 66 ms late |

The ticker lost about as many ticks as the milliseconds spent reading. With fingers moving, CHG stays
low, so the task read message after message without awaiting, each read polled. The reads now await
each I2C byte. The test also checks the async I2C transfers against the polled ones, and prints finger
lines only after `v`. Observed:

| Check | Result |
|---|---|
| PMIC device ID, `i2c_0` at 400 kHz, 100 reads | Polled 107 us each, async 115 us, 4 interrupts each; all equal |
| Touch information block, `i2c_5` at 100 kHz, 100 reads | Polled 1056 us each, async 1060 us, 11 interrupts each (one per byte); all equal; ticker 106 ticks in 106 ms |
| Async probe of `0x30`, nothing there | `AddressNack`; the next read `Ok` |
| Object table read, 198 bytes, cut by a timeout after 3 ms | Timed out; the next one `Ok`, equal to a polled read |
| Fingers moving, up to 1404 messages in 5 s | Ticker 4965-4968 ticks in 5 s, latest 23-28 us late; 0 read errors, 0 wake-ups with none |

With nothing to do the ticker gets 4965 ticks in 5 s, the same as with fingers moving now: each tick
takes a little over 1 ms. In the first 5 s it was once 120 ms late, from the check lines above, sent
polled in that period. An awaited byte costs about 2 us more than a polled one, the interrupt and the
wake-up.

## Audio

The WM8758 codec sits on `i2c_1` at `0x1a`, its four supplies on LDO25 (`vdd_audio`, 3300 mV).
I2S0, in the audio subsystem at `0x0383_0000`, is the clock master: it sends the codec its
master clock on CDCLK, and BCLK and LRCLK. 16-bit stereo I2S at 48 kHz.

- **Clocks:** `drivers::i2s::set_clocks` runs the EPLL at 196.608 MHz (`Cmu::set_epll`,
  M 197, P 3, S 3, K -25690, the entry of Linux's table) and divides it in the audio subsystem:
  `dout_srp` /4, `dout_aud_bus` /4 = 12.288 MHz, the registers' clock and I2S0's RCLK, 256 times
  48 kHz; BCLK is 32 times it. The dividers are written directly. Linux picks them from rates:
  the EPLL's 196608001.7 Hz is listed as 196608001, and `DIV_ROUND_UP(196608001, 16)` is
  12288001 (`drivers/clk/clk-divider.c:145`), above 12288000, so asking for 12288000 lands on /17, about 45.2 kHz and a mechanical
  sound. This board's DTS asks for 12288001 for that reason.
- **Order:** the EPLL first. The audio subsystem's registers run from `sclk_epll`, which
  U-Boot leaves on the EPLL, at 96 MHz here.
- **I2S0:** `drivers::i2s::I2s` sets it up as master and takes one frame per 32-bit word, the
  left channel in the high half (heard on the board). The CPU fills the FIFO, or DMA does, below.
  `take_underrun` reads and clears the FIFO's underrun flag.
- **Codec:** `drivers::wm8758`. Its registers are 9 bits and write-only, so only the
  acknowledgement can be checked. `start_headphones` follows Linux's anti-pop order: bias and
  VMID first (500 ms), then the DAC, mixers and headphone outputs, muted until they are on.
  `set_speakers` adds LOUT2/ROUT2 and the speaker amplifier (`gpm1-2`). The pop suppressor
  (`gpm1-3`) follows the vendor driver, not the mainline DTS, which holds it high: low from
  `power_up` through the VMID charge, high only after the DAC is unmuted (then 100 ms), low again
  before every mute. The jack pin (`gpx1-6`) is high while a jack is in, as the vendor kernel has
  it (`det_active_high`); the mainline DTS's `GPIO_ACTIVE_LOW` is wrong for this board. It is
  valid only with the headphone outputs on: before `start_headphones` it read low with a jack
  in, and in one run followed the pull-up: nothing drives it then. With the outputs on the
  pulls change nothing, and it was right on every plug and unplug. Putting a jack in also silenced the
  speakers with the amplifier on, so the socket seems to cut them in hardware.
- **DMA:** `drivers::pl330` drives `pdma0` (`0x1268_0000`), an ARM PL330. `Program` assembles
  its channel instructions; `Pl330::start` hands `DMAGO` to the manager through the debug
  registers. The manager runs non-secure here (`CR0` `0x003ff075`: 8 channels, 32 requests, 32
  events), so the channels and their control words are non-secure too. `i2s::DmaPlayback`
  plays a ring of two halves of 4096 frames, 85 ms each, on I2S0's transmit request 12:
  ```text
  CCR, DAR = I2STXD
  top: SAR = ring
    16 x 256 { FLUSHP 12; WFPS 12; LDS; STPS 12 }  SEV 0
    16 x 256 { FLUSHP 12; WFPS 12; LDS; STPS 12 }  SEV 1
  LPFE top
  ```
  The events are interrupts in the controller, in `INTSTATUS`; the CPU refills the half
  that ended and cleans it out of the caches. `service` polls them; with the `embassy`
  feature, `wait_half` awaits the next one on `pdma0`'s GIC interrupt (`pl330::PDMA0_IRQ`,
  ID 67), after `pl330::init_interrupt`. The handler turns the interrupt off at the GIC and
  wakes the waiters, which turn it on again while they wait, as the async eMMC does. Both
  events pending at once means a half played twice, counted as late; the half refilled is then
  the one the channel does not read, found from its source address, so the refills cannot stay
  a half behind the channel. Two findings on the board:
  - `FLUSHP` before each request, as Linux does. With one at the start only, the channel ran
    at 11 times the frame rate, its beats lost in a full FIFO, and stopped after the ring.
  - `DMALPFE` needs the loop counter bit set (`0x2c`). With it clear (`0x28`) the channel
    faults with an undefined instruction. Neither Linux nor the vendor kernel loops forever.
- **WAV:** `wav::Wav` reads the header of a PCM WAV file and its 16-bit samples.

`make img APP=audio_test`, same flashing step. It embeds alsa-utils' `speaker-test` voices if
the build finds them (`AUDIO_SAMPLES_DIR`, by default buildroot's alsa-utils tree); they are
not in this repository, being under alsa-utils' license. The console is buffered: a polled
line holds the CPU for 7 ms, and the FIFO lasts 1.3 ms. Observed:

| Check | Result |
|---|---|
| From U-Boot | EPLL `0xa0400203`/`0x66010000`, 96 MHz; `sclk_epll` from the EPLL; all MAUDIO gates open |
| Clocks set | EPLL `0xa0c50303`/`0x66019ba6`, 196608001 Hz, locked; `dout_aud_bus` 12288000 Hz; AUDSS src `0x1`, div `0xf33`, gate `0x1ff` |
| Frame rate against the MCT, 1 s after start | 47999.85-47999.90 Hz |
| Frame rate, 5 s windows while playing | 48000.18-48000.20 Hz: 4 ppm, the same in every window, so the MCT and the EPLL disagree by that much; not a divider |
| 1 kHz at -12 dBFS on the headphones, -30 dB to 0 dB | Clean, no gaps; volume steps heard |
| Left and right | Swapped at first; right after putting the left channel in the high half, checked with the `Front_Left` and `Front_Right` voices |
| Speakers | Sound with `set_speakers`; off again with it |
| Codec MCLK divider /2 (Linux, vendor kernel) and /1 | No difference heard |
| Nine voices, 16-bit mono 48 kHz | Frame counts as on the host; clear, no distortion |
| Underruns | None while playing. One each time the speakers were switched, both times in silence: the five I2C writes take longer than the FIFO lasts |

`make img APP=audio_dma_test` plays the same through `pdma0`. Observed:

| Check | Result |
|---|---|
| `DMALPFE` probe, `SEV` then a jump back to it | `0x28`: undefined instruction fault; `0x2c`: loops, the event raised again |
| First 10 ms | 48 frames a millisecond; FIFO at 63 frames; channel waiting for the peripheral, no fault |
| Frame rate, 1 s from event to event | 13 halves in 1109333 us, 48000.01 Hz; no FIFO underrun |
| 5 s windows while playing | 48000.00 Hz in most; 47995.86-48004.14 Hz where an event was seen late, made up in the next window |
| Tone and the nine voices | Heard; no FIFO underrun in any window |
| 100 volume writes in a row (`w`) | 59 ms of I2C; 0 late, no underrun (the CPU-filled FIFO lasts 1.3 ms) |
| CPU busy 50 ms (`h`) | 0 late |
| CPU busy 200 ms (`H`), longer than two halves | Counted late, each time |

`make img APP=audio_async_test` plays the same from an Embassy task that awaits each half,
with a task ticking every 1 ms on the same core. It also reports the wake-up: how far the
channel has read into the next half when the task looks, the time from the event through the
interrupt and the executor, in whole frames of 21 us.

First run on the board, twice: clicks every 85 to 170 ms from the start, worst on the
samples, until `H`, after which the sound was clean. The log showed 30 of 60 halves late in
every 5 s, about 104000 `pdma0` interrupts a second, and a wait for the next half that ended
two halves later. Cause: `service` took the events in its own order. After the 500 ms of the
codec's start both were pending, and taking them in that order left it expecting the event of
the half the channel had just started, a half off. It then waited for the wrong event while
the right one, unclaimed, held the shared line high: an interrupt storm, and one wake-up per
two halves, which refilled the half the channel had begun to read. `H` let both pile up again,
and by chance the order came out right. Fixed: `service` follows the events that are set and,
when both are, the channel's position; `wait_half` waits for either event. After `H` the old
code showed 59 interrupts for 59 halves, none late.

With the fix, on the board:

| Check | Result |
|---|---|
| Halves before the executor | 1 refilled, 1 late (the codec's 500 ms); channel in half 1, half 1 next |
| Wait cut after 10 ms | Timed out after 10023 us; the next ended 85332 us after the cut began, a half |
| Silence, 1 s | 12 halves, 12 interrupts; 48001.03 Hz, the events timed by the task to a frame |
| 5 s windows while playing | 47996.93-48004.15 Hz; 59 halves, 59 interrupts; 0 late, 0 resynced, 0 underruns |
| Wake-up | 0 us in every window without load: under a frame, 21 us |
| Ticker beside it | 19-547 us late at most |
| Tone and the nine voices on headphones | Clean from the start, no clicks |
| `h` 50 ms, then `H` 200 ms twice | 2 late, 0 resynced, no underrun; wake-up 29625 us at most; clean in the next window |
| `q` | 456 interrupts in all |

Lines longer than the terminal used, 182 columns with wrapping off, showed only their last
character past column 181, which looked like lost bytes; the report is two lines since.

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

## MAX8903 charger

`drivers::max8903` reads the charger's pins and sets its charge enable. The chip has no bus, only
five active-low GPIOs (`exynos4412-mehmet.dts:186-201`): `DOK` and `UOK` for power on the DC and
USB inputs, `CHG` while charging, `FLT` after a charge timer fault, and `CEN`, the one output.
`power::charger_connected` reads `DOK` and `UOK` through it.

- **Enable:** `Max8903::new` leaves `CEN` as U-Boot left it; `Status::enabled` is `None` while
  the pin is not an output. `set_charging` makes it one. Both Linux drivers enable charging
  whenever an input has power.
- **State:** `CHG` is high whenever the chip is not charging, so `NotCharging` covers a full
  battery, charging disabled and a fault alike.
- **Disable:** has no effect on the board. U-Boot leaves `CEN` an output driven low; driving it
  high left `CHG` low, and charging started again on a new DC connection. The vendor kernel
  never drives it high either (nothing calls `max8903g_charger_disable`), so `CEN` may not reach
  the chip as the DTS says, or be held low on the board.
- **Fault:** `restart_charging` disables charging for 10 ms and enables it again, meant to start
  a new charge timer; given the above it may do nothing. No fault was seen.
- **Not wired:** `DCM` and `USUS` have no GPIO on this board.

`charger_test` on the board: `make img APP=charger_test`. It prints the pins, then a line each
time one changes; `e`, `d` and `r` enable, disable and restart charging, `s` prints the state.
Observed on the board:

| Check | Result |
|---|---|
| At start, DC connected | DC yes, USB no, charging yes, fault no, enable on |
| `d` | enable off, still charging |
| DC unplugged | DC no, charging no: `Discharging` |
| DC plugged back, enable off | `NotCharging`, then charging 100 ms later |

## eMMC

`drivers::mmc` reads the board's eMMC through the DW MMC controller `mshc_0`, and writes it only
through `MmcWriter`. `partition::scan` parses its MBR and the chain of extended boot records, and
gives each partition the block of the record that describes it.

- **Commands:** The driver can send only `CMD8`, `CMD13`, `CMD17`, `CMD18`, `CMD24` and `CMD25`,
  with the controller's automatic `CMD12` as the stop. It has no erase command, and no switch
  command (`CMD6`). So the only hardware partition it can reach is the user area.
- **Boot loaders out of reach:** `PARTITION_CONFIG` is `0x48`: the board boots from boot
  partition 1, where the boot ROM finds BL1, BL2 and U-Boot. Without `CMD6`, the driver cannot
  select it.
- **Writes:** `MmcWriter::new(card, rca, number)` reads the partition table from the card
  itself. It writes only inside that partition. It refuses:
  - the extended partition;
  - a table with any partition below `PROTECTED_BLOCKS` (20480, the first 10 MiB: the MBR, and
    room Samsung's layouts keep for boot loader copies);
  - a partition with a table record inside it.

  Below the writer, the driver itself refuses any write below `PROTECTED_BLOCKS`. Each write
  waits until the card has programmed the data and is back in its transfer state. The card's
  own write cache stays off, so the data is on the card when the call returns.
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

Write test: `make img APP=mmc_write_test`, same flashing step. It reads the first 10 MiB, showing
only which blocks are not zero. It copies the last 2 MiB of CACHE (p8), twice to be sure of the
copy. It checks that the writer refuses what it must. It writes 1, 9 and 4096 blocks of a pattern
there, reads each back through DMA and polled, and writes the copy back at the end, also after
a failure. Observed:

| Check | Result |
|---|---|
| First 10 MiB of the user area | Only block 0 (the MBR) and blocks 3447..3449 are not zero: no boot loader there, which fits their being in boot partition 1 |
| Table records | The MBR, and EBRs at 65536, 86016, 106496, 116790, 1028096, 2461696: all outside the partitions |
| Refusals | All six refused before reaching the card: the extended partition, a partition not in the table, past the end, across the end, a buffer off a cache line, part of a block |
| 1 block | Written in 11.7 ms, read back right through DMA and polled |
| 9 blocks, off a 4 KiB boundary | 0.7 ms, about 6 MB/s, read back right |
| 4096 blocks, two commands | 97.5 ms, about 21 MB/s, read back right |
| Restore | 106 ms; the region reads back with the copy's hash |

In the DMA cases, each read is first done polled. Then stale bytes go over the buffer and guard
values into a line on each side, all cleaned to DRAM. The DMA read must then give the polled
data, and DRAM, read through `mmu::DRAM_UNCACHED`, must still hold the guard values. 9 blocks
cross a descriptor off a 4 KiB boundary. 4096 blocks take two commands.

The first version left `IDINTEN` at 0. The card finished each read (`DATA_OVER`, then `ACD`), but
`IDSTS` never showed `RI`, and data stayed in the FIFO. With `IDINTEN` set as Linux sets it
(`NI | RI | TI`), every case passed. Interrupts still stay off, since `CTRL_INT_ENABLE` is clear.

`EXT_CSD` `BUS_WIDTH` reads 0 although the controller is in 8-bit mode, and data arrives without CRC
errors. p10 is 28295104 blocks on the card, which ends it exactly where the extended partition ends.

## ext4 (read-only)

`ext4_test` reads the eMMC's ext4 file systems with the [`ext4-view`](https://crates.io/crates/ext4-view)
crate, which only reads. The library stays without an allocator: the test brings its own
12 MiB heap (`linked_list_allocator`, in `.bss`), and its image is 15 MiB, still below
`0x41000000` and inside `mmu::DRAM_UNCACHED`.

- **Reader:** `ext4-view` asks for bytes at any offset. The test's `Ext4Read` serves them from
  a 6 MiB cache of 32 KiB chunks, which DMA (`Mmc::read_blocks_dma`) fills directly, the least
  recently used going first. Reads past the end of the partition are refused. The first version
  had no cache, only a 64 KiB bounce buffer; the measurements below are from it.
- **What is printed:** For each file system: the label, the UUID and how many entries the root
  has. Beyond that:
  - EFS (calibration data): nothing more.
  - SYSTEM: not an Android system on this board. The root's names and a walk of the tree.
  - USERDATA: the Linux root file system when Linux ran on this board. The root's names, the
    distribution lines of `/etc/os-release`, a walk of the whole tree, and two timed reads of the
    largest file under 4 MiB in `/usr`.

  Walks print only counts, so no names from places such as `/home` appear.

Test on the board: `make img APP=ext4_test`, same flashing step. Observed:

| Check | Result |
|---|---|
| EFS | Loads in 0.8 ms (4 reads). No label; 5 entries in the root |
| SYSTEM | Loads in 1 ms. No label; only `lost+found/` and `update/` |
| USERDATA | Loads in 183 ms: 1728 reads, 108 KiB of group descriptors. Label `rootfs`; 20 entries in the root |
| Heap | About 360 KiB per loaded file system, all freed after |
| SYSTEM walk | 3 directories, no files, 1 ms |
| USERDATA `os-release` | `Debian GNU/Linux 13 (trixie)` |
| USERDATA walk | 4568 directories, 34517 files (2446 MiB), 3601 symbolic links, 8 others, in 36.6 s. 48038 reads of 32 KiB, 1.5 GiB in all, and 32.3 s of the time in the reader |
| Largest file in `/usr`, read twice | `/usr/lib/udev/hwdb.d/20-pci-vendor-model.hwdb`, 4132304 bytes, about 39 MB/s, same hash both times |

The walk is bound by reads. `ext4-view` keeps its own cache of only 256 KiB, read 32 KiB at a
time. Asking for one inode reads 32 KiB, and the next directory's inodes have usually been
evicted by then, so the walk reads 1.5 GiB to look at 43000 entries. A cache below it, in the
reader, would keep what it evicts. A file read moves 32 KiB per call too, so it reaches about
half of the 75 MB/s that DMA reaches with 2 MiB reads.

With the reader's 6 MiB cache:

| Check | Without cache | With cache |
|---|---|---|
| USERDATA load | 182 ms, 1728 card reads | 9.8 ms, 4 chunks from the card |
| USERDATA walk | 36.6 s, 48038 card reads (1.5 GiB) | 10.1 s, 4960 chunks (155 MiB), 5.7 s in the reader |
| Largest file, first read | about 39 MB/s | about 37.6 MB/s, 134 chunks |
| Largest file, second read | about 39 MB/s | about 155 MB/s, all from the cache |
| Each load | | about 3 ms more, zeroing the 6 MiB cache |

The walk still asks the reader for 1.5 GiB, now mostly copies out of the cache.

## FAT32 on CACHE

The library's `fat` module, behind the `fat` feature, puts a FAT32 file system on one eMMC
partition through the [`embedded-sdmmc`](https://crates.io/crates/embedded-sdmmc) crate.
CACHE (p8, 445 MiB) is the one used, since nothing on this board needs what it held.

- **Device:** `PartitionDevice` is the crate's `BlockDevice`. It shows the partition as a whole
  disk: block 0 is a master boot record made up in memory, listing one FAT32 (`0x0C`) partition
  from block 1 on, and block `n` is block `n - 1` of the partition. The card's own table is not
  changed, so p8 keeps its Linux type byte there. Writes to the made-up block 0 are refused, and
  all others go through an `MmcWriter` for p8, with the limits of the eMMC section. Transfers go
  through a buffer the caller gives, since the crate's blocks need not start on a cache line.
- **Write cache:** writes stay in the device until `PartitionDevice::flush`, or until the cache
  is full. Consecutive blocks, the file data, gather in one run, written with one command; the
  test gives it 256 KiB. Up to 8 other blocks, the FAT blocks of both copies and directory
  entries, which the crate writes again for each cluster, wait in slots, and each new write of
  one replaces the cached copy. Reads see the cached blocks. A flush writes the run before the
  slots, so file data reaches the card before the FAT entries that point at it. The crate never
  asks for a flush: call it after closing files, through `VolumeManager::device`.
- **Formatter:** `embedded-sdmmc` does not format. `PartitionDevice::format` follows Microsoft's
  FAT specification (`fatgen103`): 512-byte sectors, 8 per cluster up to 8 GiB, 32 reserved
  sectors, two FATs, the FSInfo sector at 1 and a copy of the boot sector at 6. It clears the
  reserved sectors, both FATs and the root cluster, and writes the boot sector last. For CACHE:
  890 sectors per FAT, 113685 clusters of 4 KiB.
- **Probe:** `PartitionDevice::probe` checks the boot sector itself before the crate reads it,
  so that another file system is not taken for FAT. Only a FAT32 boot sector whose geometry fits
  the partition passes.
- **Names:** the crate makes 8.3 names only.
- **Time:** `MctClock` starts at a given time and runs on the MCT. `fat_test` starts it from the
  RTC, plus 3 hours: FAT keeps local time and Turkey is UTC+3 all year. If the RTC holds no valid
  time, the clock starts at 2026-01-01 00:00. `FixedTime` stamps every file with one time.

Test on the board: `make img APP=fat_test`, same flashing step. The first run formats CACHE.
Each run then counts itself in `BOOTS.TXT`, writes `TEST.BIN` (1 MiB), reads it back, timed and
hashed, and deletes it, appends a line to `LOGS/BOOT.LOG` and lists the root. Observed, over
two boots:

| Check | First boot | Second boot |
|---|---|---|
| Probe | No FAT32 (p8 held ext4): formatted in 64 ms, probe passes after | FAT32 found |
| `BOOTS.TXT` | boot 1 | boot 2 |
| `TEST.BIN` write, 1 MiB | 2992 ms, 342 KiB/s | 1847 ms, 554 KiB/s |
| `TEST.BIN` read back | 245 ms, 4.1 MB/s, same hash, no bad byte | 244 ms, 4.1 MB/s, same hash, no bad byte |
| `TEST.BIN` delete | OK | OK |
| `LOGS/BOOT.LOG` | made, 8 bytes | 16 bytes, as expected |
| Card's table | p8 still type `0x83` | p8 still type `0x83` |

Both directions are far below what the driver reaches (75 MB/s reading, 21 MB/s writing with
DMA), because `embedded-sdmmc` moves one block per call: each written block was its own `CMD24`,
waited on until programmed, besides the FAT and directory updates. With the write cache, on the
third boot:

| Check | Without cache | With cache |
|---|---|---|
| `TEST.BIN` write, 1 MiB, flush included | 1847 ms, 554 KiB/s | 85 ms, 11980 KiB/s |
| Blocks the crate wrote, while writing it | | 3073 |
| Write commands to the card | one per block | 12, for 2056 blocks |
| Reads while writing | | 7 from the card, 264 from the cache |
| `TEST.BIN` read back | 244 ms | 253 ms, same hash, no bad byte: 2304 single block reads |
| `LOGS/BOOT.LOG` | | 24 bytes after the third boot, as expected |
| Whole test | 2136 ms | 426 ms |

Reads were then most of the time. The crate reads one block per call, and also reads the FAT
block again for each 4 KiB cluster: 2048 data blocks and 256 FAT reads for 1 MiB.

- **Read-ahead:** reads go through four windows of 16 KiB. A block in none of them refills the
  one used least recently, from that block on, with one command, and the dirty blocks of the
  write cache go over what the card gave. File data and the FAT are read in turns, so each keeps
  a window. Writes update every window that holds the block, since windows can overlap.

With read-ahead too, on the fourth boot:

| Check | Write cache only | With read-ahead |
|---|---|---|
| `TEST.BIN` read back, 1 MiB | 253 ms, 4041 KiB/s: 2304 read commands | 39 ms, 26071 KiB/s: 64 read commands of 16 KiB, 2240 blocks from the windows; same hash, no bad byte |
| `TEST.BIN` write, flush included | 85 ms, 11980 KiB/s | 89 ms, 11380 KiB/s |
| Reads from the card, whole test | 2328 commands | 70 commands for 2240 blocks |
| `LOGS/BOOT.LOG` | 24 bytes | 32 bytes, as expected |
| Whole test | 426 ms | 213 ms |

On the fifth boot, with the RTC: `BOOTS.TXT` stamped 2026-11-03 12:36:28, local time, and
`LOGS/BOOT.LOG` got `boot 5 at 2026-11-03 12:36:28`. `LOGS`, made on the first boot, keeps
2026-01-01.

## GPU

The Mali-400 MP4 at `0x13000000` (`exynos4.dtsi:398`): a geometry processor (GP), four pixel
processors (PP0-3), each with an MMU, an L2 cache and a power unit. `drivers::mali` takes the
register map from Linux's `lima` driver, and the interrupts from `exynos4412.dtsi:729-750`.

- **Supply:** BUCK4 `vdd_g3d` (`mali-supply`), always on. BUCK2 to BUCK4 have eight voltage
  registers each, chosen by the pins `gpl0-0` to `gpl0-2`. The DTS drives them to select `DVS2`;
  U-Boot leaves them inputs, read low, which selects `DVS1`. `Max77686::dvs_buck` reads the
  registers, nothing sets them yet.
- **Power and clock:** `Mali::power_on` turns on the domain at `0x10023C60`, opens `CLK_G3D`
  and runs `sclk_g3d` from the MPLL. 160 MHz at 875 mV is the lowest operating point
  (`exynos4412.dtsi:756-771`); the vendor kernel ran up to 440 MHz from the VPLL.
- **What it says:** `identify` reads the GP's and the PPs' `VERSION` and the L2's `SIZE`;
  `pmu_power_up` brings up the blocks the GPU's own power unit keeps off.
- **Resets:** the GP's and the PPs' soft resets, each MMU's hard reset after it takes a table
  address, and emptying the L2.

`make img APP=gpu_test`: reads the supply and the pins, powers the GPU at 160 MHz only if the
supply is at least 875 mV, identifies the blocks before and after the power unit, resets each
with its completion left on its interrupt line for the GIC to see, then the MMUs and the L2.
On the board:

| Check | Result |
|---|---|
| BUCK4 | on; `DVS1` 1.100 V, `DVS2` to `DVS8` 1.000 V; `gpl0-0` to `gpl0-6` inputs, low: `DVS1` |
| As U-Boot left it | domain on (`0x60007`), every `GATE_IP_G3D` gate open, `sclk_g3d` 50 MHz from the MPLL (divider 16) |
| After `power_on` | `sclk_g3d` 160 MHz (divider 5) |
| GP | `0x0B070101`: Mali-400 r1p1 |
| PP0 to PP3 | `0xCD070101` each: Mali-400 r1p1 |
| L2 | `0x06110206`: 128 KiB, 4-way, 64-byte lines, 64-bit bus |
| GPU power unit | status 0, every block on already; nothing to power up |
| Resets | GP and PP0-3 under 1 us each; GIC 159 and 155-158 pending after, not after clearing |
| MMUs | GP's and PP0-3's take a table address and reset; `STATUS` `0x18`, idle, paging off |
| L2 emptied | under 1 us |

The power unit's status reads 0 whether every block is on or there is no power unit: the vendor
kernel does not list one. The supply is at 1.1 V, above the 1.025 V of the highest operating
point, 440 MHz.

### First frames

`IdentityMap` gives the GPU's MMUs the CPU's addresses, read and write. `PpFrame::clear` and
`clear_stream` make a PP frame that draws nothing: each tile is written back in its clear colour,
B8G8R8A8 into a linear buffer, which the FIMD shows as XRGB. The frame and stream layouts come from
Mesa's lima driver (MIT). A stream lists the tiles a PP renders; the GP and its polygon lists are
not used yet, so a tile is either its position alone or points at an empty list.

`make img APP=gpu_clear_test`: PP0 clears a 64 x 64 buffer, trying both kinds of tile, and the CPU
checks every pixel; then the screen, by PP0 alone and by all four, against a CPU fill. On the
board, GPU at 160 MHz:

| Check | Result |
|---|---|
| PP0, 64 x 64 | 40 us, every pixel right, with tiles as their position alone |
| PP0, 1280 x 800 | 4782 us, every pixel right: about 860 MB/s |
| PP0-3, 1280 x 800 | 3623 to 3626 us over 20 frames, every pixel right: about 1.1 GB/s |
| CPU fill and cache flush, 1280 x 800 | 7101 us |

Four PPs are only 1.3 times one: the frame is 4 MB of writes and nothing else, with the FIMD
reading the same DRAM meanwhile, so the bus is the likely limit, not measured.

`Rect::split_for_gpu` gives the GPU what it can fill of a rectangle and the CPU the rest. The PP
writes whole 16 x 16 tiles and its write-back address rounds down to 8 bytes, as found on the
board, so the GPU gets an even left edge and whole tiles inside; up to three strips are the CPU's.

`make img APP=gpu_rect_test`: eleven rectangles, at odd columns and at the screen's edges, each
checked inside and out; then 200 random ones, about 7.8 M pixels, the same ones each way. On the
board, all eleven right:

| Way | GPU at 160 MHz | GPU at 266 MHz |
|---|---|---|
| CPU fill and flush, cached | 108.6 ms | 108.6 ms |
| GPU, CPU edges cached (flush, then invalidate after the GPU) | 105.0 ms | 93.6 ms |
| GPU, CPU edges uncached | 58.3 ms | 46.9 ms |
| CPU fill, uncached | 17.0 ms | 16.9 ms |

Plain fills are the CPU's: through the uncached view it writes about 1.8 GB/s, combining writes,
and needs no cache maintenance. Each of the GPU's 200 frames costs about 200 to 270 us. The GPU is
for what the CPU cannot do as fast: triangles, textures, blending.

### First triangle

`gpu_triangle_test` draws triangles through the whole pipeline, first a flat orange one: the
GP runs the vertex shader and the PLBU, which bins the triangle into each tile's polygon list,
and a PP renders the tiles from those lists with the fragment shader. The shaders are compiled offline
by Mesa 25.3.3's lima compiler (MIT), run on a PC with `drm-shim` and `LIMA_DEBUG=dump`; the
binaries and the render state are copied from that dump into the application. The command
lists are built on the board with `mali::Commands`, whose commands are those of Mesa's
`lima_gpu.h`, in the order Mesa's driver writes them. `TileEnd::Blocks` points each tile of the
PP's stream at its polygon list block, and `Mali::start_gp` and `wait_gp` run the GP.

On the board, GPU at 160 MHz, every pixel checked, none wrong:

| Frame | GP, both lists | PP0 | Tiles: covered, listing the triangle |
|---|---|---|---|
| 64 x 64, own buffer | 8 to 12 us | 24 us | 12, 16 |
| 256 x 256, middle of the screen | 20 to 22 us | 367 us | 84, 169 |

- The vertex shader's positions are exactly the viewport transform of the three vertices.
- The PLBU bins by more than the triangle's pixels: tiles near its edges get it too. Every
  block gets at least the end of its list, so the PP needs no other empty list.
- The colour's green, 0.5, comes out 0x7F or 0x80 by pixel: the frame dithers, as Mesa sets it.
- The PP's first line is the viewport's bottom, so the triangle is upside down on the screen.

A second scene passes a colour per vertex through a varying: red, green and blue corners,
interpolated by the PP. The vertex shader writes the colours after the positions, 64 bytes on,
as Mesa lays its GP output out, and the render state points the PP at them. On the board every
pixel inside is within 1/255 of the exact interpolation, in both sizes, with the same times as
the flat scene.

The vertex shader's viewport scale has a negative y, so the frame's first line is the scene's
top and the triangles stand apex up on the screen. Each frame is also rendered by all four PPs,
each taking every fourth tile of the stream. On the board, 160 MHz, every pixel right either way:

| Frame | 1 PP | 4 PPs |
|---|---|---|
| Shaded, 64 x 64 | 40 us | 70 us |
| Flat or shaded, 256 x 256 on the screen | 387 us | 261 us, 1.48 times as fast |

The 256 x 256 frame is 256 KiB of write-back: four PPs write about 1.0 GB/s, near the 1.1 GB/s of
the four-PP screen clear, so DRAM, not the shaders, is the likely limit. On the 64 x 64 frame,
16 tiles, four PPs take longer than one; the cause is not known, perhaps starting and waiting on
each PP in turn.

A third scene samples a texture: 16 x 16 RGBA8888 texels in lines, nearest, clamped to the
edges, its descriptor from `mali::texture_rgba8888`, which packs it as Mesa does; the render
state points the PP at the texture list. The vertex shader passes texture coordinates on as it
did the colours. On the board every pixel checked is its texel's within 1/255 (those within a
tenth of a texel of a texel's edge are not checked), in both sizes, on one PP and four, with the
flat scene's times: 387 us and 261 us at 256 x 256.

Where the frame starts matters. The PP writes from its target rounded down to 8 bytes
(`PpFrame::clear`): a frame at column 85 of the screen came out a pixel to the left, and took
540 us on one PP. A target off a 64-byte line is slow too: at column 938, 8- but not 64-byte
aligned, the textured frame took 879 us; at column 928, 386 us. The test puts its frames on
tile columns.

The GP and the PP need a soft reset before each frame, as Linux gives them after each job:
without one, the PP drew the shaded scene after the flat one in black, every pixel inside
`0xFF000000`, though its render state named the new fragment shader.

### Animation

`gpu_spin_test` turns a textured square, two triangles, in a 320 x 320 frame in the middle of
the screen: a turn every 6 s, its size pulsing every 2 s. The texture is 64 x 64, a white border
round a red and green gradient on a checkerboard. The command lists, the texture and the render
state are built once; each frame the CPU writes the six turned positions and cleans their line,
then the GP and the PPs are reset, the GPU's L2 cleared, the GP run, and the four PPs render
straight into the screen. The frame starts at the start of vsync and is done in half a
millisecond, before the FIMD scans the middle lines, so the screen shows whole frames from one
buffer. Key `v` turns the wait for vsync off.

On the board, 160 MHz, means over 2 s (largest in brackets):

| Vsync | Frames a second | GP | 4 PPs | Frame: resets, GP and PPs |
|---|---|---|---|---|
| on | 58.1, the panel's rate | 29 us (35) | 391 us (397) | 457 us (468) |
| off | 2150 to 2176 | the same | the same | the same |

The PPs write 400 KiB a frame, about 1.05 GB/s, as in the triangle test: DRAM sets the rate.

`gpu_cube_test` first turned a cube (before the runtime below), 12 triangles, in perspective in the same frame. The CPU makes one
matrix a frame, perspective times camera times the turns, and writes it into the uniforms; the
vertex shader, Mesa's for `gl_Position = mvp * pos`, takes the 36 vertices through it. Each face
is a cell of a 128 x 64 texture atlas, its own colour. The depth test, `Less`, is one word of the
render state; the depth buffer is the PPs' tile memory, cleared per tile by the frame and never
written to DRAM. On the board the GP's positions of the first frame are those of the CPU's own
transform, and the far faces stay hidden:

| Vsync | Frames a second | GP | 4 PPs | Frame |
|---|---|---|---|---|
| on | 58.1 | 38 us (45) | 392 us (398) | 467 us (475) |
| off | 2092 to 2119 | 38 us (45) | 395 us (409) | 470 us (488) |

The 36 vertices and the matrix cost the GP 9 us more than the square's 6; the PPs' time is the
write-back's, as before.

The cube is lit by one distant light from the upper left front: the vertex shader gives each
vertex a brightness from its face's normal, a quarter ambient and three quarters
`max(normal . light, 0)`, and the fragment shader scales the texel by it. The CPU turns the
light back by the cube's turns each frame, so the normals stay the cube's own. On the board the
first frame's texture coordinates and brightnesses are the CPU's, and the faces darken as they
turn away. The fragment shader is now three instructions, not one, and the PPs take 435 to
500 us (largest 543), more the more of the frame the cube covers: the PPs' shading starts to
show beside the write-back. 1730 to 1920 frames a second without vsync, 58.1 with.

The texture is then sampled bilinearly: `mali::texture_rgba8888` takes a `Filter`, and for
`GL_LINEAR` Mesa's descriptor differs from `GL_NEAREST`'s in two fields only, the nearest bits
off and the LOD bias 0 rather than 0.5; the shaders are the same. Key `b` switches the filter
while the cube turns. The atlas's two unused cells are white, as the cells' borders, so that the
filter at a cell's edge takes nothing else from its neighbour. On the board the edges of the
checkerboard are smooth, and the PPs' time, 435 to 509 us, cannot be told from nearest's: the
four texels a pixel cost no more than the one.

### Shaders: tools/shaderc

Shaders are written in GLSL ES 1.00 and compiled on the PC into Rust, kept in the repository:
building an application needs no Mesa. An application keeps them as `shaders/NAME.vert` with
`shaders/NAME.frag` beside it, and

    tools/shaderc/setup.sh              # once: libdrm and Mesa 25.3.3 under ~/.cache/exynos-shaderc
    make shaders APP=gpu_cube_test      # shaders/*.vert and .frag -> src/shaders.rs

writes one `exynos_4412_rs::gpu::Program` a pair, `pub const NAME`: the binaries, the vertex
shader's prefetch, the attributes in the order declared, each uniform's stage and place by name,
the vertex shader's constants, the varyings and their stride, the samplers, and the render state
with its addresses zero. The application then looks uniforms up by name,
`LIT.uniform("mvp", Stage::Vertex)`, rather than knowing where the compiler put them.

`setup.sh` builds lima's `drm-shim`, which plays the board's Mali-400 MP4 on the PC (patched
from the Mali-450 it plays by default), and makes the driver's dump print floats exactly. It
takes the tarballs from buildroot's downloads when there, checked by SHA-256, and buildroot's
host `python3` (with mako), `bison` and `flex`. `shaderc.py` runs `harness.c` under the shim with
`LIMA_DEBUG=dump`: it draws a triangle with the pair, every uniform set to sentinel values, and
`shaderc.py` reads the dump, finding each uniform by its sentinels. The render state is drawn
with no depth test and no blending; those are the application's. `gpu_cube_test` takes its
shaders from it; its binaries, uniform places and constants are those copied by hand from the
dump before, and its render state too but for the depth test's word.

### Runtime: `exynos_4412_rs::gpu`

Above the generated programs, `gpu` draws with no addresses, caches or command lists in the
application:

```rust
let mut gpu = Gpu::start(mct, BACKGROUND)?;      // GPU power, MMUs, screen, the pool
let atlas = gpu.texture_with(128, 64, Filter::Bilinear, texel)?;   // texel(x, y) -> 0xAARRGGBB
let cube = gpu.mesh(&shaders::LIT)
    .attribute("pos", &positions)                // by name, checked against the shader
    .attribute("uv", &uvs)
    .attribute("normal", &normals)
    .build()?;
loop {
    let mut frame = gpu.frame(Area::centered(512, 256), GREY);   // waits for vsync
    for model in models {
        frame.draw(&cube).uniform("mvp", &(projection * model)).uniform("light", &light)
            .texture("t", &atlas).depth_test(true);
    }
    let times = frame.show()?;                   // the GP, then the four PPs
}
```

- `Gpu::start` needs the MMU on. It powers the GPU at 160 MHz, turns the screen on uncached,
  maps DRAM up to the end of the pool and the screen for the GPU's MMUs, and resets the GP and
  PPs. It runs once: the pool is one static, `AlreadyStarted` the second time.
- Textures take 0xAARRGGBB texels, as the screen does, and are converted to the GPU's RGBA8888:
  `texture` from a slice, `texture_with` from a function of x and y, written straight into the
  pool. CPU0's stack is 16 KiB: a texture built in a local array first overflows it into the
  pool's end, the GPU's page tables (the first board run did, and jumped into `.bss`).
  `set_filter` changes nearest or bilinear between frames.
- A mesh copies its attributes, by name, and the program's binaries into the pool. A wrong
  name, a missing attribute, unequal vertex counts or a count not a multiple of three is an
  `Error` from `build`.
- A frame is an `Area` of the screen, corner and size multiples of 16 and at most 512 tiles
  (`MAX_TILES`: 512 x 256, 352 x 352...), cleared to a colour. Each draw (up to `MAX_DRAWS`, 64)
  gets its own uniforms, render state, varyings and GP output in the frame's memory, and joins the
  frame at the end of its statement; a draw's mistakes (an unknown or unset uniform, a wrong
  size, a sampler without a texture) are kept and `show` returns the first. Uniforms take an
  `f32`, `[f32; N]` or a `Mat4`, and go to whichever shader declares them: half floats for the
  fragment shader.
- `show` runs the frame: per draw, Mesa's command blocks (lima_draw.c) one after another in the
  VS and PLBU lists, the semaphores ordering each draw's PLBU after its vertex shader; then the
  four PPs, the depth buffer their tile memory, shared by the draws. It returns the GP's, the
  PPs' and the whole frame's microseconds.
- `gpu::Mat4` (column-major, `a * b` applies `b` first: `translation`, `scale`,
  `rotation_x/y/z`, `perspective`, `transform_direction_back`) and `sin_cos`, `sqrt`,
  `normalize`, `dot`: the arithmetic, without `std`.

The pool is about 3 MiB in `.bss`, plus the 4 MiB screen: 2 MiB for meshes and textures
(`LASTING_BYTES`), kept until restart, and 512 KiB for a frame's draws (`FRAME_BYTES`), reused
each frame. All of it must be in the first 16 MiB of DRAM, where the screen has its uncached
view. `gpu_cube_test` is written on it: one mesh drawn twice a frame, two cubes turning opposite
ways in a 512 x 256 frame. On the board both turn, lit and depth tested, as the single cube did:

| Vsync | Frames a second | GP, 2 draws | 4 PPs | Frame |
|---|---|---|---|---|
| on | 58.1 | 50 to 57 us (58) | 541 to 565 us (601) | 632 to 654 us (689) |
| off | 1446 to 1540 | 51 to 57 us (58) | 534 to 568 us (590) | 623 to 665 us (681) |

The second draw costs the GP about 14 us; the PPs write 512 KiB, about 0.95 GB/s, DRAM's rate
as before. Nearest and bilinear cannot be told apart.

Not yet: frames larger than 512 tiles (tiles sharing polygon list blocks), blending, indexed
draws, more than one level of texture.

## Heap

The `heap` feature gives a global allocator: TLSF (the `rlsf` crate) behind a lock that masks
IRQs, so `Box`, `Vec` and the rest work on every core and in interrupt handlers. Finding and
freeing a block take a bounded number of steps whatever the heap's history; growing a block
allocates, copies with the lock released, and frees. An application:

1. writes `exynos_4412_rs::global_heap!();` and `extern crate alloc;`; the library does not name
   the allocator itself, so `ext4_test` keeps its own;
2. sets `__heap_size` in `memory.x`, up to the DRAM free (`heap_test`: `1024M`), and makes `RAM` that much longer: `link.x` places the
   heap after the stacks of all cores, not cleared at boot;
3. calls `heap::init()` on CPU0 after `mmu::enable`, before anything allocates.

`heap::stats()` gives bytes in use, the peak, and allocations, frees and refusals. A refused
allocation returns null, which `Box` and `Vec` turn into a panic.

`make img APP=heap_test`: collections on CPU0, 20000 timed allocations and frees of 16 B to
4 KiB, a 768 MiB block written through and a 1.5 GiB one refused; then all four cores at once,
100000 operations each on blocks of random size and alignment with a pattern each, resizes,
blocks handed to the next core to check and free, and SGIs whose handlers allocate. At the end
nothing may be left in use. On the board, heap 1 GiB (MCT resolution 42 ns):

| Check | Result |
|---|---|
| `heap::init`, 1 GiB | 1 us |
| Collections | OK |
| CPU0 alone, 16 B to 4 KiB | allocation mean 166 ns, max 666 ns; free mean 125 ns, max 583 ns |
| 768 MiB block | 791 ns; written in 503 ms (1527 MiB/s), read and compared in 1912 ms; 1.5 GiB refused with null |
| Four cores, 100000 operations each | 314 ms; about 43700 allocations and 12500 resizes per core, 0 corrupt, 0 refused |
| Per call, on four cores | allocation mean 291 ns, max about 10.1 us; free mean 250 ns, max about 10 us |
| Across cores and in handlers | about 1440 blocks handed on per core; 391 allocations in SGI handlers per core |
| After all | 243827 allocations, as many frees, 0 bytes in use |

The worst cases on four cores include waiting for the lock, and an SGI handler that may run
between the two reads of the counter: they are not the allocator's own time.

## Async (Embassy)

The library's `embassy` module, behind the `embassy` feature, runs [Embassy](https://embassy.dev)
async tasks, with one executor per core. Tasks stay on the core that spawned them; cores talk
through `embassy-sync` channels, signals and mutexes, which the critical section below covers.

- **Critical section:** the `critical-section` implementation masks IRQs on the calling core and
  takes one lock shared by the four cores. The same core may enter again inside; the restore
  state records it. It needs the MMU on, for exclusive accesses.
- **Time:** the `embassy-time` driver counts the MCT global counter at 1 MHz. Wake-ups go through
  comparator 0, now also usable once (`Mct::set_alarm`), whose interrupt goes to CPU0. One queue
  holds the timers of all cores. The application's `handle_irq` passes each interrupt to
  `embassy::on_interrupt`.
- **Executor:** `embassy::run` runs `embassy-executor`'s raw executor on the calling core. It
  sleeps with `WFE`; a task woken from any core or interrupt sends `DSB` then `SEV`. The crate's
  own Cortex-A platform is not used: its `SEV` has no barrier before it, so a woken core could
  look at its run queue before the task is there, and sleep again.
- **UART:** `embassy::uart::UartRx` receives on a UART's interrupt (UART0-3, GIC IDs 84-87):
  the handler clears it, moves the FIFO into a ring of 2 KiB and wakes the reader, which
  awaits `read`. The ring holds what comes while the reading task's core is busy. Its setup
  masks the UART's other interrupts: U-Boot leaves `UINTM` at 0, and the transmit interrupt,
  pending whenever the transmit FIFO is low, then came back on every return, 370000 times a
  second. Sending stays polled.
- **eMMC:** `Mmc::read_blocks_async` and `MmcWriter::write_async` run the same DMA transfers
  as the polled calls, but await the controller's interrupt (GIC ID 109, to CPU0) for the end
  of data. The handler disables it at the GIC and wakes the waiter, which arms it again. An
  async mutex keeps one transfer at a time; the card's busy wait after a write is polled every
  50 us with a timer. A transfer dropped part way, by a timeout for one, is let finish, polled,
  before its DMA is stopped: the driver sends no `CMD12`, so a card stopped in the middle would
  go on sending. `MmcWriter::write_async` refuses what `write` refuses.
- **DMA events:** `Pl330::wait_event` and `wait_events` await events of `pdma0` on its
  interrupt (GIC ID 67, to CPU0), and `DmaPlayback::wait_half` the end of either half of the
  audio ring. All events
  share the one interrupt, level-triggered: the handler disables it at the GIC and wakes every
  waiter, each looks at its own event and enables it again if it still waits. A wait leaves
  its event set, for `take_event` or `service`.
- **GPX pins:** `embassy::eint::EintInput` waits for a level on a GPX pin, on its wake-up
  interrupt, level-triggered. The interrupt stays masked except during a wait, and the handler
  masks it again, so a level that lasts interrupts once. A wait first clears what an earlier
  level left pending, so it can end with the level gone: the caller checks the device, and
  waits again if it has nothing. `Mxt::next_message` waits on CHG this way, and awaits its I2C
  bytes.
- **I2C:** `I2c::write_read_async` and the calls built on it run the polled driver's steps, but
  await the controller's interrupt (GIC IDs 90-97) after each byte. The handler disables it at
  the GIC and wakes the transfer. An async mutex per bus keeps one async transfer at a time; a
  byte not done in 10 ms is a timeout. A transfer dropped part way finishes its byte, reads one
  more without acknowledging it if it was reading, and sends a stop. A polled call on a bus
  during an async transfer on it breaks both.
- **Console:** `embassy::uart::buffer_console` makes `print!` buffered: what does not fit in the
  UART's FIFO goes into a ring of 16 KiB, and the UART's transmit interrupt, on CPU0, sends it on.
  `print!` stays atomic across cores and keeps the order. With the ring full, the printing core
  sends the oldest bytes itself, polled, so nothing is lost, with IRQs masked too; its core
  runs nothing else meanwhile.
  `console::flush` sends what is left, polled. The panic handler, the default exception
  handlers, `power::restart` and `power::power_off` call it. `flush_console().await` waits for
  the ring to be sent.
- **Not yet present:** preemptive threads.

Test on the board: `make img APP=async_test`, same flashing step. Observed:

| Check | Result |
|---|---|
| CPU0 timers of 1, 10, 100, 500 ms | 7, 7, 7, 4 us late by `embassy-time`; 7, 6, 6, 3 us by the MCT |
| Timers of 100, 200 and 300 ms awaited together | 300 ms |
| CPU1-3, 10 timers of 50 ms each, woken through CPU0's interrupt and `SEV` | 2-3 us late on average, 4 us at most |
| Primes below 200000 three times on CPU0 | 17984 each, 1443 ms |
| The same three jobs sent over a channel to CPU1-3 | 17984 on each core, 480 ms each, 487 ms in all: 2.95x |
| Console UART | `UCON` `0x3c5` from U-Boot, kept; `UFCON` `0x111`; `UINTM` `0xe` after setup |
| `busy`: CPU0 runs no task for 3 s, nothing typed | 0 interrupts |
| `busy` with 1753 bytes pasted meanwhile | All 1753 in the ring, in 884 interrupts, 0 dropped; echoed after |

`make img APP=mmc_async_test` checks the eMMC side, with a task ticking every 1 ms on CPU0
throughout. Observed:

| Check | Result |
|---|---|
| 16 MiB of USERDATA read async, 2 MiB at a time | 240 ms reading, 69 MB/s, 16 interrupts; the ticker ran on |
| The same read polled | 238 ms, 70 MB/s, same hash; the ticker stood still while each 2 MiB was read |
| The same read async from a task on CPU1 | 238 ms, same hash |
| A 1 MiB read dropped after 300 us, then read again | Timed out; the next one `Ok`, same hash as before |
| The last 2 MiB of CACHE written async with a pattern, read back, then restored | 122 ms, 17 MB/s; read back equal; restored hash equal to the one kept |

`make img APP=uart_tx_test` checks the buffered console at 115200 baud, with a task ticking
every 1 ms on CPU0. Observed:

| Check | Result |
|---|---|
| One line of 100 characters, polled | 7380 us in `println!` |
| The same line buffered | 4 us in `println!`; the ring out 12.3 ms later, with what was in it before; ticker 22 us late at most |
| Ten lines | 38 us in `println!`, out in 97 ms; ticker 7 us late at most; 0 bytes polled |
| 200 lines, 20400 bytes, through the 16 KiB ring | 358 ms in `println!`, out in 1781 ms; 4120 bytes polled by the printer, the overflow; ring full at 16384; the ticker held back 357 ms while the printer polled |
| CPU0 and CPU1, 20 lines each at once | All 40 whole, in order on each core |
| Totals | 26149 bytes through the ring, 4120 polled, 1576 transmit interrupts |
| `p`: 30 lines, then a panic at once | All 30 out, then the panic message, which said 3043 bytes were still in the ring |
| `r` and `f`: 30 lines, then a restart, normal or into fastboot | All 30 out, then the restart line, also with 3043 bytes held when printed |

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
- **Host wake:** `Bcm4334::enable_host_wake` has the chip drive `gpx0-3` high while it has
  something for the host, as the vendor kernel did (`HW_OOB`; the Linux that worked here had it
  off). `WifiDevice::gate_on_host_wake` then reads the chip only when the line is high, or
  something is known to wait, and once every 50 ms regardless; what that backstop read finds
  counts as missed by the line. `wifi_test` turns it on unless built with `WIFI_HOST_WAKE=0`,
  and reports the reads every 10 s. First run on the board, gating on the line's level: the line stayed low with the
  interrupt status at `0x208000c0`, every frame was found by a backstop read, and a 1 MiB
  download fell to 292 KiB/s. The probe since: the chip takes each `0xF2` setting (read back
  equal; `0x04` at reset), function 1 has an interrupt pending, yet the pin reads 0 with every
  setting and every pull, the pull-up included, so something drives it low. The board's NVRAM
  has `sd_gpout=0` and `sd_oobonly=1`: the firmware signals on its GPIO 0 alone, not on DAT1.
  Watched at every read without a pull, the line went high only on reads that found a frame,
  never on empty ones, but on few of them. Gated on its level without a pull, 91 reads came
  from the line and 78 from backstops: 250 KiB/s. The line seems to pulse, shorter than the
  5 us between two looks. `wifi_test` now gates on the pin's wake-up interrupt, masked, rising
  edge, whose pending bit the controller latches: `HostWake::Pending`. On the board:

  | Check | Result |
  |---|---|
  | Idle, every 10 s | 200 empty reads, all backstops, 0 missed; about 2150000 polls skipped |
  | 1 MiB on port 80 | 3020 KiB/s (3150-3531 reading every poll); 461 frames, 0 backstops, 0 missed |
  | 10 s around it | 467 frames, 191 backstops, 0 missed |
  | Upload | Received |

  Every frame came after an edge, and no backstop found anything. Each run of frames ends with
  one empty read, the one that finds the chip has no more.
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

### Under Embassy (`embassy::wifi`)

With the `wifi-net` feature, `embassy::wifi::new` makes a joined chip an `embassy-net` device
(`embassy-net-driver-channel`). Its `Runner::run` task is the only thing that touches the chip: it
awaits a rising edge on `gpx0-3` (`EintInput::wait_latched`, the pending bit latched while
masked), a frame from the stack, room in the stack's receive queue, or a deadline, and the core
sleeps meanwhile. It reads the chip after an edge, while frames it announced wait, every 50 ms
regardless (a read that finds something counts as missed), and every 200 us while a frame waits
for room in the firmware (`room_polls`). `embassy::wifi::stats` gives the counts. The runner and
`embassy_net`'s runner must be on the same executor.

`WIFI_FIRMWARE_DIR=<directory> make img APP=wifi_async_test`, with `WIFI_SSID` and
`WIFI_PASSWORD` as for `wifi_test`. It joins as `wifi_test` does, polled, then runs on CPU0: DHCP,
a fetch from `1.1.1.1:80`, three HTTP servers on port 80 (`/`, `/data` 1 MiB, `POST /upload`),
pings answered, a 1 ms ticker whose lateness shows how busy the core is, and a report every 10 s
(`s` for one now). On the board, bus at 50 MHz:

| Check | Result |
|---|---|
| DHCP | 2123 to 2142 ms; fetch from `1.1.1.1:80` 17 to 19 ms |
| 1 MiB of `/data` to `curl` | 113 ms, 9048 KiB/s (`curl`: 8904 KiB/s); 8406 KiB/s before the slice |
| 3 MiB `POST /upload` from `curl` | 1438 ms, 2086 KiB/s; 2161 frames in, 374 ms reading the chip |
| Idle, every 10 s | 200 empty reads, all backstops, 1 ms reading the chip; 0 missed |
| Around the transfers | 624 and 151 edges, 9 room polls, rx queue never full, 0 missed |
| 1 ms ticker on the same core | 25 to 30 us late at most idle |
| Ticker during the download | 2052 us late at most before the slice, 1277 us with it |
| Ticker during the upload | 1888 us late at most, with the slice |
| Pinged from another computer | 1.2 to 1.5 ms |

No read found what the line had not announced. The runner returns to the executor once 500 us
have passed (`embassy::wifi::SLICE`) and goes on where it stopped; that took the download's worst
from 2052 to 1277 us. What is left is longer than a slice and a frame: it is likely the stack
taking in all that is queued in one poll, not yet measured.

## Video codec (MFC)

The MFC v5.1 at `0x1340_0000` decodes and encodes video through a firmware that runs on its own
RISC core. Samsung documents how the host talks to it (commands, buffer sizes, their order)
only in GPL drivers, so that part lives in its own crate, `exynos_4412_mfc`, licensed
GPL-2.0-or-later after Linux 5.15.198's `drivers/media/platform/s5p-mfc` (text in
`exynos_4412_mfc/COPYING`). An application that links it is GPL too. `exynos_4412_rs` stays MIT
and has only the register-level part, `drivers::mfc`: the power domain (`0x1002_3C40`), the clock
gate and `sclk_mfc` (200 MHz from the MPLL, as the vendor kernel sets it).

The firmware, `s5p-mfc.fw`, is not in this repository:

```
MFC_FIRMWARE=<path>/s5p-mfc.fw make img APP=mfc_test
```

`mfc_test` copies it to the start of port A (`0x6000_0000`; port B at `0x6400_0000`, 36 and
8 MiB as Linux reserves them), resets the codec, releases the RISC, waits for its
`FW_STATUS_RET`, sends `SYS_INIT` and prints the firmware's version. The system MMUs in front of
the ports are not used.

Then it decodes H.264 with `exynos_4412_mfc::h264::Decoder`: `open` (`OPEN_INSTANCE`), `header`
with the SPS and PPS (`SEQ_HEADER`, then the frame buffers and `INIT_BUFS`), `decode` once per
picture (`FRAME_START`), `finish` at the end (`LAST_FRAME`). All of it is polled. A picture handed
out stays the caller's until `release`. The pictures are NV12MT: 64x32 tiles in Z and mirrored-Z
groups, which `exynos_4412_mfc::tiles::Plane` reads.

The test stream, `mfc_test/data/pcm320x240.h264`, comes from `tools/h264_pcm.py`. It has only
I_PCM and P_Skip macroblocks, so a correct decoder gives exactly the pictures drawn, and the
script also writes their CRC-32s. On the board all 30 pictures match: about 3.9 ms for an IDR
picture and 1.4 ms for a P picture, at 320x240. Once all match, `mfc_test` plays the stream on
the panel at three times its size, 30 fps.

### Playing a video: mfc_play

`mfc_play` plays an H.264 stream on the panel, over and over. `exynos_4412_mfc::annexb` cuts it
into pictures, and the decoder puts them in display order, B-frames included.
FIMC 0 (`drivers::fimc`, below) converts each one into a framebuffer while the MFC decodes
the next one (`Decoder::start` and `Decoder::wait`). The picture fills the panel as far as its
shape allows, turned a quarter turn clockwise when that makes it larger. There are three
framebuffers: one on the panel, one waiting for its vsync, one being drawn (`Fimd::show`,
`Fimd::wait_shown`). If the FIMC cannot take the picture, the CPU draws it
(`exynos_4412_mfc::convert`). The stream is built into the image;
an MP4 file's video track comes out with `tools/mp4_annexb.py`:

```
python3 tools/mp4_annexb.py video.mp4 video.h264
MFC_FIRMWARE=<path>/s5p-mfc.fw MFC_VIDEO=video.h264 MFC_FPS=24 make bin APP=mfc_play
```

Without `MFC_VIDEO` it plays `mfc_test`'s stream. Port B gets 64 MiB here: a 720p frame buffer
takes about 1.2 MiB of it. On the board a 720x1280 High profile stream with B-frames plays at
24 fps, turned to fill 1280x720: 35.9 s for 860 pictures. The FIMC takes 6.4 ms per picture,
where the CPU took 35.4 ms; the CPU waits 1.2 ms on average for the decoder, and is otherwise
free.

### Scaler and colour converter: FIMC

The four FIMCs at `0x1180_0000` (`exynos4.dtsi:214-264`) are the camera interface's back end:
each scales, converts between YCbCr and RGB, turns by quarter turns and writes the result to
memory. `drivers::fimc` uses one from memory to memory: a two-plane YCbCr 4:2:0 picture,
linear or in the MFC's 64x32 tiles, comes out as `0x00RRGGBB` pixels in a framebuffer, at any
size. `Fimc::power_on` (camera power domain `0x1002_3C00`, gates, `sclk_fimc` at 160 MHz from
the MPLL, reset), `configure` once per size, then `start` and `wait` per picture, polled on
`CISTATUS`. FIMC 3 has no rotator. The registers come from Linux's `exynos4-is/fimc-reg.h`;
nothing here uses its System MMU.

## 2D accelerator (G2D)

`drivers::g2d` drives FIMG2D 4.1 at `0x1080_0000` (`exynos4412.dtsi:298-305`): solid fills,
copies between images, scaled with nearest or bilinear filtering when the sizes differ, and
Porter-Duff "over" blending of an image with alpha (straight or premultiplied) times a global
alpha. Images are XRGB8888, ARGB8888 or RGB565, by physical address, so uncached framebuffers or
cleaned memory. `G2d::power_on` (gates `GATE_IP_DMC` 23-24, `sclk_fimg2d` at 200 MHz from
`sclk_mpll`; no power domain), then per operation `fill`, `copy` or `blend`, which start it,
and `wait`, polled on `INTC_PEND`. The register map comes from Linux's `exynos_drm_g2d.c` and the
vendor kernel's `fimg2d4x_regs.h`; which registers each operation sets follows libdrm's
`exynos_fimg2d.c` (MIT, `THIRD_PARTY_NOTICES.md`). Rectangles' right and bottom edges are
exclusive. Written XRGB pixels get 0xFF as their top byte.

`g2d_test` checks the result pixel by pixel and times the CPU doing the same (1280x800, uncached
framebuffers). The colours are compared, not the top byte: asked `0x00123456`, the G2D writes
`0xff123456`. Every check passes:

| Operation | G2D | CPU | Result |
|---|---|---|---|
| Fill 1280x800 | 1.28 ms (3.2 GB/s) | 2.1 ms | right |
| Copy 1280x800 | 1.73 ms (2.4 GB/s) | 65.4 ms | right |
| Scale 320x200 to 1280x800, nearest | 1.29 ms | 59.7 ms | right |
| Scale 320x200 to 1280x800, bilinear | 5.28 ms | | 0 out of range |
| Blend 128x128 ARGB, global alpha 255 or 128 | 53 us | 3.4 ms | worst error 2 |
| Blend 128x128 scaled to 384x384, bilinear | 0.77 ms | | by eye |
| Blend 1280x800 at half | 2.77 ms | 68.3 ms | worst error 1 |

The demo, a bilinear full-screen background and 12 blended sprites from 64 to 256 pixels, takes
6.8 ms per frame and keeps the panel's 58 frames per second; the CPU sleeps 99% of the time
(see Sleeping instead of polling). The CPU's copies are
slow because they read uncached memory.

## Sleeping instead of polling

`exynos_4412_rs::sleep::until` waits for a device asleep: the core stops in `wfi` with IRQs
masked in the CPSR, the device's interrupt enabled only in the GIC, which wakes it anyway; no
handler runs. A `ready` check after each wake-up clears what the device flagged. Timeouts are
the MCT's comparator 0, left armed from one wait to the next, as each write to it waits for the
MCT's clock domain. `until_count` and `for_us` sleep for a time, `read_key` until a key on the
console (UART receive interrupt). `slept` counts the time asleep and `take_wakes` which
interrupt woke the core, read from the GIC's highest-pending register without taking it.

The G2D, FIMC and MFC waits, the FIMD's vsync waits (its frame interrupt at the start of vsync,
through the interrupt combiner, group 11 = `GIC_SPI 11`; polled if the MCT is not running) and
`mfc_play`'s frame pacing all sleep this way.

- **U-Boot's interrupts:** U-Boot leaves the GIC on with interrupts of its own enabled, among them
  `gpx0-3`'s (ID 51), whose line stays high: `wfi` then returned at once, 800,000 times a second,
  and the CPU slept 29% of the time instead of 99%. `until` therefore runs `Gic::init` once if the
  program has not, which `Gic::is_initialized` tells.
- **Measured:** `g2d_test`'s demo sleeps 99% of the time at 58 frames per second, woken per 2 s
  1508 times by the G2D (13 operations a frame), 116 by the vsync, 19 by the alarm. `mfc_play`
  sleeps 96-97% at 24 fps, one late picture per pass as before; per pass, 860 wake-ups by FIMC 0
  and 860 by the pacing alarm.

## CPU clock

U-Boot leaves the CPU at 1 GHz. `cpufreq::set_arm_mhz` moves all four cores to another rate at
run time, one of `cmu::ARM_POINTS` (200 MHz to 1.4 GHz, every 100 MHz); nothing changes at boot.

- **Clock:** `Cmu::set_arm_clock` follows `clk-cpu.c`: the CPU runs from the MPLL (800 MHz),
  divided down if that is faster than the old or the new rate, takes the new dividers, the APLL
  is set and locks, and the CPU moves back. PLL settings, dividers and voltages are Linux's
  (`exynos4x12_apll_rates`, `e4412_armclk_d`, `cpu0_opp_table`). 1.5 GHz (`turbo-mode`) is left out.
- **Voltage:** `vdd_arm` is BUCK2. The voltage goes into the DVS register the pins `gpl0-0..2`
  choose (`max77686::selected_dvs`): `DVS1`, as U-Boot leaves them inputs, read low. It is raised
  before a faster clock, with the ramp time plus 100 µs, and lowered after a slower one.
- **Not covered:** per-chip ASV voltages, and temperature: no TMU driver throttles the CPU.

`make img APP=cpufreq_test`: the registers and BUCK2 as U-Boot left them, then 1200, 1400, 800,
200, 1000 and 1400 MHz, each measured with the cycle counter against the MCT, its voltage read
back and an integer workload timed and checked; then 10 s at 1.4 GHz. On the board:

| Check | Result |
|---|---|
| As U-Boot left it | APLL `0xa07d0300`, 1 GHz, measured 999.998 MHz; BUCK2 `DVS1` 1.200 V, `DVS2` to `DVS8` 1.100 V; pins `lll`: `DVS1` |
| Each step | rate, measured rate (within 5 ppm), voltage read back and workload result all OK |
| Workload | 30.0 ms at 1 GHz, 25.0 ms at 1.2 GHz, 21.4 ms at 1.4 GHz, 37.5 ms at 800 MHz, 150 ms at 200 MHz |
| Voltages | 1.1875 V at 1.2 GHz, 1.2875 V at 1.4 GHz, 1.000 V at 800 MHz, 0.900 V at 200 MHz, 1.0875 V at 1 GHz |
| Soak, 10 s at 1.4 GHz | 467 runs, 0 wrong |

U-Boot runs 1 GHz at 1.2 V; Linux's table, which `set_arm_mhz` applies, at 1.0875 V.

## Standby

`standby::enter` is a light standby, as a phone's on a press of the power key: backlight, LVDS
and the FIMD's output off, the CPU at 200 MHz and 0.9 V, the calling core asleep in `wfi` until
the power key (`gpx1-3`, its wake-up interrupt on both edges); then the CPU rate, the FIMD (its
settings kept, `Fimd::resume`), LVDS and the brightness come back. The chip stays powered and
DRAM runs: not suspend to RAM, nothing needed from the bootloader. `PowerButton::poll` tells a
short press (30 ms to 2 s, on release) from a long one (2 s, while held).

Only the display and the CPU clock are handled: the program turns off and on what else it runs
(touch, audio, WiFi, GPU). IRQs stay masked on the calling core meanwhile; an interrupt left
enabled in the GIC still wakes it, which `Report::slept_us` shows.

`make img APP=standby_test`: bars on the panel at 1.4 GHz; a short press enters standby, the
next wakes it, and the bars move. On the board:

| Check | Result |
|---|---|
| Standby, 5.7 s | asleep 99%, 9 wake-ups (the press's edges, and every 10 ms while held) |
| CPU | 1.4 GHz -> 200 MHz -> 1.4 GHz |
| Screen | back as it was; the key reads correctly in its interrupt function |

## QEMU

A `qemu` target was not added, by project decision; testing is done on the real board.

## TODO(verify)

`drivers::cmu` opens the clock gate of the FIMD's System MMU, which stays off. Whether a closed gate
would block the FIMD's reads is not verified.

