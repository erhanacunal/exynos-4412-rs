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
name. Everywhere else in this repository, Linux is used only as a source of register maps, with
one exception, below.

## Samsung's `s5p-mfc` driver (GPL)

`exynos_4412_mfc/` and `mfc_test/` are not under this repository's MIT license: they are
GPL-2.0-or-later, as the Linux driver whose commands, buffer sizes and order of operations they
follow, `drivers/media/platform/s5p-mfc/` in Linux 5.15.198 (Copyright (C) 2010-2011 Samsung
Electronics Co., Ltd.). The license text is in `exynos_4412_mfc/COPYING`. Nothing in
`exynos_4412_rs` comes from that driver; its `drivers::mfc` takes register addresses only. The
MFC's firmware is not in this repository.

## Samsung's `s5p-jpeg` driver (GPL)

`exynos_4412_jpeg/` and `jpeg_test/` are not under this repository's MIT license: they are
GPL-2.0-only, as the Linux driver whose register settings, order of operations and encoder
tables they follow, `drivers/media/platform/s5p-jpeg/` in Linux 5.15.198 (Copyright (c)
2011-2014 Samsung Electronics Co., Ltd.). The license text is in `exynos_4412_jpeg/COPYING`.
Nothing in `exynos_4412_rs` comes from that driver; its `drivers::jpeg` takes addresses and clock
fields only.

## Mesa's lima driver

The Mali-400 support, `exynos_4412_rs/src/drivers/mali.rs`, follows the frame, stream, write-back
and command list layouts of Mesa 25.3.3's lima driver, `src/gallium/drivers/lima/`. The shader
binaries and the render state in `gpu_triangle_test/src/main.rs` are output of its compiler and
driver, run offline. These files carry this notice:

```text
Copyright (c) 2011-2013 Luc Verhaegen <libv@skynet.be>
Copyright (C) 2017-2019 Lima Project

Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the "Software"),
to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense,
and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
THE COPYRIGHT HOLDER(S) OR AUTHOR(S) BE LIABLE FOR ANY CLAIM, DAMAGES OR
OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.
```

## libdrm's Exynos G2D library

The 2D accelerator support, `exynos_4412_rs/src/drivers/g2d.rs`, sets the registers of each
operation (fill, scaled copy, blend) as libdrm 2.4.131's `exynos/exynos_fimg2d.c` does, which
carries this notice:

```text
Copyright (C) 2013 Samsung Electronics Co.Ltd
Authors:
	Inki Dae <inki.dae@samsung.com>

Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the "Software"),
to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense,
and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice (including the next
paragraph) shall be included in all copies or substantial portions of the
Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
VA LINUX SYSTEMS AND/OR ITS SUPPLIERS BE LIABLE FOR ANY CLAIM, DAMAGES OR
OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.
```

## Dependencies

Crates this builds with are listed in `Cargo.lock`, under their own licenses. The network stack,
`smoltcp`, is under 0BSD. `ext4_test` uses `ext4-view` (MIT or Apache-2.0) to read ext4 and
`linked_list_allocator` (MIT or Apache-2.0) for its heap. The `fat` feature uses `embedded-sdmmc`
(MIT or Apache-2.0) for FAT32. The `embassy` feature uses `embassy-executor`, `embassy-time-driver`,
`embassy-time-queue-utils`, `embassy-time`, `embassy-sync` and `critical-section`, and
`async_test` and `mmc_async_test` also `embassy-futures`, all MIT or Apache-2.0. The `wifi-net`
feature adds `embassy-net-driver-channel` and `embassy-futures`, and `wifi_async_test` uses
`embassy-net` with its own `smoltcp`, all MIT or Apache-2.0 but `smoltcp`, 0BSD. The `heap`
feature uses `rlsf` (MIT) for the allocator, with `const-default` (MIT or Apache-2.0).

## Firmware

The chip's firmware and NVRAM files are not in this repository and are not covered by its license.
