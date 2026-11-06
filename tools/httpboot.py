#!/usr/bin/env python3
"""Sends an application image to `uart_boot` on the board over WiFi, by HTTP.

`uart_boot` built with WiFi (see uart_boot/build.rs) joins the network after its 3 s on the UART,
gets an address by DHCP, prints it, and takes images for 5 s before it boots the application
saved on CACHE; with volume up held at power-on, or without a saved application, it waits for
good. The file is a `make bin` output, `<app>.bin`; a `boot.img` is taken too, its Android header
skipped. The image goes with its CRC-32 in `X-CRC32`, which the board checks.

    httpboot.py --host 192.168.1.50 IMAGE           run it from RAM
    httpboot.py --host 192.168.1.50 --save IMAGE    save it to CACHE, then run it
    httpboot.py --wait 30 IMAGE                     retry for up to 30 s: start the script,
                                                    then power on or reset the board
    httpboot.py --stay [IMAGE]                      test mode on first: FERRITE/FERRITE.BIN no
                                                    longer boots by itself, across restarts
    httpboot.py --autoboot                          test mode off, from the next power-on
    httpboot.py --flash FLASH.BIN [IMAGE]           save FILE as it is as FERRITE/FLASH.BIN,
                                                    the application's data; then IMAGE if given

Any connection during the 5 s also holds the saved application for that power-on, so with
--wait the image is never beaten by it. Keys on the console do the same as --stay and
--autoboot: `s` and `a`.

The host defaults to $EXYNOS_HOST, else `exynos`, the name uart_boot gives DHCP (many routers
resolve it, often as `exynos.lan` or `exynos.local`). Plain curl works too, without the CRC:

    curl --data-binary @IMAGE http://192.168.1.50/boot
"""

import argparse
import http.client
import os
import socket
import struct
import sys
import time
import zlib

ANDROID_MAGIC = b"ANDROID!"
ZIMAGE_MAGIC_OFFSET = 0x24
ZIMAGE_MAGIC = 0x016F2818
PORT = 80
CONNECT_TIMEOUT_S = 2.0
# Saving a large image to the eMMC comes before the answer.
ANSWER_TIMEOUT_S = 60.0
RETRY_S = 0.5


def log(text):
    sys.stderr.write(text + "\n")
    sys.stderr.flush()


def load(path):
    with open(path, "rb") as file:
        data = file.read()
    if data.startswith(ANDROID_MAGIC):
        # boot_img_hdr: kernel_size at 8, page_size at 36; the kernel starts one page in.
        kernel_size, = struct.unpack_from("<I", data, 8)
        page_size, = struct.unpack_from("<I", data, 36)
        data = data[page_size:page_size + kernel_size]
        log("httpboot: boot.img, kernel of %d bytes" % len(data))
    if len(data) < ZIMAGE_MAGIC_OFFSET + 4 or struct.unpack_from("<I", data, ZIMAGE_MAGIC_OFFSET)[0] != ZIMAGE_MAGIC:
        raise SystemExit("httpboot: %s is not an image of ours (no zImage magic)" % path)
    return data


def connect(host, wait_s):
    """A connection to the board, retried until `wait_s` has passed."""
    deadline = time.monotonic() + wait_s
    tries = 0
    while True:
        tries += 1
        try:
            return socket.create_connection((host, PORT), timeout=CONNECT_TIMEOUT_S)
        except OSError as error:
            if time.monotonic() >= deadline:
                hint = "" if wait_s else " (is uart_boot waiting? --wait retries)"
                raise SystemExit("httpboot: %s:%d: %s%s" % (host, PORT, error, hint))
            if tries == 1:
                log("httpboot: waiting for %s:%d ..." % (host, PORT))
            time.sleep(RETRY_S)


def post(host, path, wait_s):
    """A POST without a body, such as /stay; True if the board said 200."""
    connection = http.client.HTTPConnection(host, PORT, timeout=ANSWER_TIMEOUT_S)
    connection.sock = connect(host, wait_s)
    connection.sock.settimeout(ANSWER_TIMEOUT_S)
    connection.request("POST", path, body=b"", headers={"Content-Length": "0"})
    response = connection.getresponse()
    answer = response.read().decode("utf-8", "replace").strip()
    log("httpboot: %s: %d %s: %s" % (path, response.status, response.reason, answer))
    return response.status == 200


def load_flash(path):
    with open(path, "rb") as file:
        data = file.read()
    if not data:
        raise SystemExit("httpboot: %s is empty" % path)
    return data


def send(host, path, data, wait_s):
    crc = zlib.crc32(data) & 0xFFFFFFFF
    connection = http.client.HTTPConnection(host, PORT, timeout=ANSWER_TIMEOUT_S)
    connection.sock = connect(host, wait_s)
    connection.sock.settimeout(ANSWER_TIMEOUT_S)
    log("httpboot: %d bytes, CRC %08x, to http://%s%s" % (len(data), crc, host, path))
    begin = time.monotonic()
    connection.request("POST", path, body=data, headers={
        "Content-Type": "application/octet-stream",
        "X-CRC32": "%08x" % crc,
    })
    response = connection.getresponse()
    answer = response.read().decode("utf-8", "replace").strip()
    seconds = max(time.monotonic() - begin, 1e-3)
    log("httpboot: %d %s in %.1f s (%.0f KiB/s): %s" % (
        response.status, response.reason, seconds, len(data) / 1024 / seconds, answer))
    return response.status == 200


def main():
    parser = argparse.ArgumentParser(description="Send an image to uart_boot over HTTP.")
    parser.add_argument("image", nargs="?", help="a make bin output, <app>.bin, or a boot.img")
    parser.add_argument("--host", default=os.environ.get("EXYNOS_HOST", "exynos"),
                        help="the board's address or name (default $EXYNOS_HOST, else exynos)")
    parser.add_argument("--save", action="store_true", help="save it to CACHE as the application to boot, then run it")
    parser.add_argument("--wait", type=float, default=0.0, metavar="SECONDS",
                        help="retry connecting for this long, while the board starts")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--stay", action="store_true", help="test mode on before sending: nothing boots by itself")
    mode.add_argument("--autoboot", action="store_true", help="test mode off, from the next power-on")
    parser.add_argument("--flash", metavar="FILE",
                        help="save FILE, as it is, as the application's data FERRITE/FLASH.BIN first")
    args = parser.parse_args()
    if args.image is None and not (args.stay or args.autoboot or args.flash):
        parser.error("give an IMAGE, --flash FILE, --stay or --autoboot")
    data = load(args.image) if args.image else None
    flash = load_flash(args.flash) if args.flash else None
    ok = True
    wait = args.wait
    if args.stay or args.autoboot:
        ok = post(args.host, "/stay" if args.stay else "/autoboot", wait)
        wait = 0.0
    if ok and flash is not None:
        ok = send(args.host, "/flash", flash, wait)
        wait = 0.0
    if ok and data is not None:
        ok = send(args.host, "/save" if args.save else "/boot", data, wait)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
