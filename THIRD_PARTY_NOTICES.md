# Third-party notices

## Broadcom FullMAC driver (`brcmfmac`)

The WiFi support, `exynos_4412_rs/src/drivers/bcm4334.rs` and `exynos_4412_rs/src/nvram.rs`, is written
for this project. Broadcom publishes no description of the chip's host interface, so its register
values and sequence of operations are derived from `drivers/net/wireless/broadcom/brcm80211/` in Linux 5.15.198, which
carries this notice:

```text
Copyright (c) 2010 Broadcom Corporation

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

`exynos_4412_rs/src/crypto.rs` and `exynos_4412_rs/src/wpa.rs` follow the published standards they
name. Everywhere else in this repository, Linux is used only as a source of register maps.

## Dependencies

Crates this builds with are listed in `Cargo.lock`, under their own licenses. The network stack,
`smoltcp`, is under 0BSD. `ext4_test` uses `ext4-view` (MIT or Apache-2.0) to read ext4 and
`linked_list_allocator` (MIT or Apache-2.0) for its heap. The `fat` feature uses `embedded-sdmmc`
(MIT or Apache-2.0) for FAT32.

## Firmware

The chip's firmware and NVRAM files are not in this repository and are not covered by its license.
