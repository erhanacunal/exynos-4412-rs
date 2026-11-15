#!/usr/bin/env python3
"""Android boot image (header v0, as Android 4.3's mkbootimg makes) in Python.

    python tools/mkbootimg.py --kernel app.bin -o boot.img
    python tools/mkbootimg.py --info boot.img
    python tools/mkbootimg.py --unpack boot.img --kernel-out kernel.bin

The options and defaults are those of AOSP's mkbootimg (base 0x10000000,
kernel at +0x8000, ramdisk +0x01000000, second +0x00F00000, tags +0x100),
except that the page size defaults to 4096 and the command line to
`console=ttySAC2,115200`, as this board's Makefile passes them, so
`--kernel X -o Y` alone gives the same image as
`mkbootimg --kernel X --ramdisk NONE --cmdline "console=ttySAC2,115200"
--pagesize 4096 -o Y`.

Layout: a header page, then the kernel, the ramdisk and the second-stage
image, each padded to the page size. The header's id is the SHA-1 of each
part followed by its size (u32 LE), which U-Boot does not check but which
keeps the image identical to mkbootimg's.
"""

import argparse
import hashlib
import struct
import sys

MAGIC = b"ANDROID!"
# magic, kernel size/addr, ramdisk size/addr, second size/addr, tags addr,
# page size, 2 unused, name[16], cmdline[512], id[32]
HEADER = struct.Struct("<8s10I16s512s32s")
NAME_MAX = 16
CMDLINE_MAX = 512
DEFAULT_CMDLINE = "console=ttySAC2,115200"


def read(path):
    if path is None or path.upper() == "NONE":
        return b""
    with open(path, "rb") as f:
        return f.read()


def padded(data, page):
    return data + b"\0" * (-len(data) % page)


def build(kernel, ramdisk=b"", second=b"", cmdline=DEFAULT_CMDLINE, board="", base=0x10000000,
          kernel_offset=0x00008000, ramdisk_offset=0x01000000, second_offset=0x00F00000,
          tags_offset=0x00000100, page=4096):
    if not kernel:
        raise ValueError("the kernel is empty")
    if page & (page - 1) or not 2048 <= page <= 131072:
        raise ValueError(f"page size {page}: a power of two, 2048..131072")
    cmd = cmdline.encode()
    if len(cmd) >= CMDLINE_MAX:
        raise ValueError(f"command line longer than {CMDLINE_MAX - 1} bytes")
    name = board.encode()
    if len(name) >= NAME_MAX:
        raise ValueError(f"board name longer than {NAME_MAX - 1} bytes")
    sha = hashlib.sha1()
    for part in (kernel, ramdisk, second):
        sha.update(part)
        sha.update(struct.pack("<I", len(part)))
    header = HEADER.pack(MAGIC,
                         len(kernel), (base + kernel_offset) & 0xFFFFFFFF,
                         len(ramdisk), (base + ramdisk_offset) & 0xFFFFFFFF,
                         len(second), (base + second_offset) & 0xFFFFFFFF,
                         (base + tags_offset) & 0xFFFFFFFF, page, 0, 0,
                         name, cmd, sha.digest())
    out = padded(header, page) + padded(kernel, page)
    if ramdisk:
        out += padded(ramdisk, page)
    if second:
        out += padded(second, page)
    return out


def parse(img):
    if len(img) < HEADER.size or not img.startswith(MAGIC):
        raise ValueError("not an Android boot image (no ANDROID! magic)")
    (_, ksize, kaddr, rsize, raddr, ssize, saddr, tags, page, _, _,
     name, cmdline, ident) = HEADER.unpack_from(img)
    parts = {}
    offset = page
    for key, size in (("kernel", ksize), ("ramdisk", rsize), ("second", ssize)):
        parts[key] = img[offset:offset + size]
        offset += size + (-size % page)
    sha = hashlib.sha1()
    for key in ("kernel", "ramdisk", "second"):
        sha.update(parts[key])
        sha.update(struct.pack("<I", len(parts[key])))
    return {
        "kernel_addr": kaddr, "ramdisk_addr": raddr, "second_addr": saddr, "tags_addr": tags,
        "page": page, "name": name.split(b"\0")[0].decode(errors="replace"),
        "cmdline": cmdline.split(b"\0")[0].decode(errors="replace"),
        "id_ok": ident[:20] == sha.digest(), "end": offset, **parts,
    }


def info(path):
    with open(path, "rb") as f:
        img = f.read()
    h = parse(img)
    print(f"{path}: {len(img)} bytes, page size {h['page']}")
    for key in ("kernel", "ramdisk", "second"):
        print(f"  {key:8} {len(h[key]):9} bytes at 0x{h[key + '_addr']:08x}")
    print(f"  tags     at 0x{h['tags_addr']:08x}")
    print(f"  name     {h['name']!r}")
    print(f"  cmdline  {h['cmdline']!r}")
    print(f"  id       {'matches' if h['id_ok'] else 'does NOT match'} the contents")
    if h["end"] > len(img):
        print("  truncated: the parts run past the end of the file")
    return h


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--kernel", help="the kernel (here: the application .bin)")
    p.add_argument("--ramdisk", help="ramdisk (NONE or left out: none)")
    p.add_argument("--second", help="second-stage image (none by default)")
    p.add_argument("--cmdline", default=DEFAULT_CMDLINE, help=f"default: {DEFAULT_CMDLINE!r}")
    p.add_argument("--board", default="", help="board name (header field, up to 15 bytes)")
    num = lambda v: int(v, 0)
    p.add_argument("--base", type=num, default=0x10000000)
    p.add_argument("--kernel_offset", type=num, default=0x00008000)
    p.add_argument("--ramdisk_offset", type=num, default=0x01000000)
    p.add_argument("--second_offset", type=num, default=0x00F00000)
    p.add_argument("--tags_offset", type=num, default=0x00000100)
    p.add_argument("--pagesize", type=num, default=4096)
    p.add_argument("-o", "--output", help="the boot image to write")
    p.add_argument("--info", metavar="IMG", help="print an existing image's header and check its id")
    p.add_argument("--unpack", metavar="IMG", help="take the kernel out of an image (with --kernel-out)")
    p.add_argument("--kernel-out", metavar="FILE", help="with --unpack: where to write the kernel")
    a = p.parse_args()

    try:
        if a.info:
            info(a.info)
            return
        if a.unpack:
            h = info(a.unpack)
            if a.kernel_out:
                with open(a.kernel_out, "wb") as f:
                    f.write(h["kernel"])
                print(f"kernel -> {a.kernel_out}")
            return
        if not a.kernel or not a.output:
            p.error("--kernel and -o are needed (or --info / --unpack)")
        img = build(read(a.kernel), read(a.ramdisk), read(a.second), a.cmdline, a.board, a.base,
                    a.kernel_offset, a.ramdisk_offset, a.second_offset, a.tags_offset, a.pagesize)
    except (OSError, ValueError) as e:
        sys.exit(f"mkbootimg: {e}")
    with open(a.output, "wb") as f:
        f.write(img)
    print(f"mkbootimg: {a.output}, {len(img)} bytes")


if __name__ == "__main__":
    main()
