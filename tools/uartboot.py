#!/usr/bin/env python3
"""Sends an application image to `uart_boot` on the board.

Meant to run as a minicom file transfer protocol (Ctrl-A S), which connects stdin and stdout
to the serial port. The file is a `make bin` output, `<app>.bin`; a `boot.img` is taken too,
its Android header skipped. The wire format is in uart_boot/src/main.rs. Progress and the
board's answers go to stderr, which minicom shows.

minicom setup (Ctrl-A O, File transfer protocols), or in ~/.minirc.dfl:

    pu pname10          YUNYNuartboot
    pu pprog10          python3 /path/to/tools/uartboot.py

Usage outside minicom: uartboot.py IMAGE < /dev/ttyUSB3 > /dev/ttyUSB3 (port at 115200, raw).
"""

import os
import select
import struct
import sys
import time
import zlib

MAGIC = b"EXYNBOOT"
ANDROID_MAGIC = b"ANDROID!"
ZIMAGE_MAGIC_OFFSET = 0x24
ZIMAGE_MAGIC = 0x016F2818
CHUNK = 1024
ANSWER_TIMEOUT_S = 5.0


def log(text):
    sys.stderr.write(text + "\r\n")
    sys.stderr.flush()


def load(path):
    with open(path, "rb") as file:
        data = file.read()
    if data.startswith(ANDROID_MAGIC):
        # boot_img_hdr: kernel_size at 8, page_size at 36; the kernel starts one page in.
        kernel_size, = struct.unpack_from("<I", data, 8)
        page_size, = struct.unpack_from("<I", data, 36)
        data = data[page_size:page_size + kernel_size]
        log("uartboot: boot.img, kernel of %d bytes" % len(data))
    if len(data) < ZIMAGE_MAGIC_OFFSET + 4 or struct.unpack_from("<I", data, ZIMAGE_MAGIC_OFFSET)[0] != ZIMAGE_MAGIC:
        raise SystemExit("uartboot: %s is not an image of ours (no zImage magic)" % path)
    return data


def write_all(data):
    view = memoryview(data)
    while view:
        written = os.write(1, view)
        view = view[written:]


def answer(wanted, timeout):
    """Reads the board's lines until one holds a word of `wanted`; returns it, or None."""
    end = time.monotonic() + timeout
    pending = b""
    while time.monotonic() < end:
        ready, _, _ = select.select([0], [], [], max(0.0, end - time.monotonic()))
        if not ready:
            break
        chunk = os.read(0, 256)
        if not chunk:
            break
        pending += chunk
        while b"\n" in pending:
            line, pending = pending.split(b"\n", 1)
            text = line.decode("ascii", "replace").strip()
            if text:
                log(text)
            for word in wanted:
                if word in text:
                    return word
    return None


def main():
    if len(sys.argv) < 2:
        raise SystemExit("usage: uartboot.py IMAGE")
    path = sys.argv[-1]
    data = load(path)
    crc = zlib.crc32(data) & 0xFFFFFFFF
    log("uartboot: %s, %d bytes, CRC %08x" % (os.path.basename(path), len(data), crc))
    write_all(MAGIC + struct.pack("<II", len(data), crc))
    if answer(["receiving", "ERROR"], ANSWER_TIMEOUT_S) != "receiving":
        raise SystemExit("uartboot: no answer from uart_boot; is it waiting?")
    begin = time.monotonic()
    for offset in range(0, len(data), CHUNK):
        write_all(data[offset:offset + CHUNK])
        if offset // CHUNK % 16 == 0:
            sys.stderr.write("\ruartboot: %d of %d bytes" % (offset, len(data)))
            sys.stderr.flush()
    # The port's output queue drains at the line's rate: wait for the board's verdict.
    seconds = len(data) * 10 / 115200
    result = answer(["CRC OK", "ERROR"], seconds + ANSWER_TIMEOUT_S)
    log("\nuartboot: sent in %.1f s" % (time.monotonic() - begin))
    if result != "CRC OK":
        raise SystemExit("uartboot: the board did not take the image")


if __name__ == "__main__":
    main()
