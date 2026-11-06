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

With --port the script opens the port itself through pyserial (`pip install pyserial`). That is
the way on Windows, where there is no minicom and select() does not work on pipes:

    uartboot.py --port COM5 IMAGE              send, then exit (frees the port for other tools)
    uartboot.py --port COM5 --monitor IMAGE    send, then print what the board says (Ctrl-C ends)
    uartboot.py --port /dev/ttyUSB3 IMAGE      the same on Linux

uart_boot boots the application saved on CACHE (FERRITE/FERRITE.BIN) when no image arrives
within 3 s of power-on (hold volume up to stay in uart_boot instead). To replace that one:

    uartboot.py --port COM5 --save IMAGE       save it to CACHE after the CRC check, then run it
    uartboot.py --port COM5 --reset IMAGE      the ferrite application is running: restart it
                                               (its restart message), then catch uart_boot
    uartboot.py --port COM5 --wait 30 IMAGE    wait up to 30 s for uart_boot: start the script,
                                               then power on or reset the board

The application's data file, FERRITE/FLASH.BIN on CACHE, is written as it is sent (any bytes,
CRC-checked); nothing runs after it, so an image can follow in the same call:

    uartboot.py --port COM5 --flash FLASH.BIN            the data alone; uart_boot waits again
    uartboot.py --port COM5 --flash FLASH.BIN --save IMAGE   the data, then the application

The header goes out when uart_boot prints its banner, not blindly: stray bytes in U-Boot's
countdown would stop its autoboot. --debug prints every byte received.
"""

import argparse
import os
import struct
import sys
import time
import zlib

MAGIC = b"EXYNBOOT"
MAGIC_SAVE = b"EXYNSAVE"
MAGIC_FLASH = b"EXYNFLSH"
ANDROID_MAGIC = b"ANDROID!"
ZIMAGE_MAGIC_OFFSET = 0x24
ZIMAGE_MAGIC = 0x016F2818
CHUNK = 1024
ANSWER_TIMEOUT_S = 5.0
# With --wait or --reset: how long the first header gets before waiting for uart_boot's banner.
FIRST_TRY_S = 1.2
# How long --reset waits for uart_boot after restarting the application, if --wait is shorter.
RESET_WAIT_S = 20.0
# uart_boot's lines that say it now listens for a header.
READY_LINES = ["send a .bin", "unless an image arrives", "waiting for an image", "waiting again"]
# A ferrite application's restart message (ferrite_cli.py TAG_RESTART).
FERRITE_RESTART = b""
# --debug: print every byte received.
DEBUG = False
# Writing the image to the eMMC, after the CRC check.
SAVE_TIMEOUT_S = 30.0


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


class StdioLink:
    """stdin/stdout connected to the port, as minicom does it. POSIX only."""

    def write(self, data):
        view = memoryview(data)
        while view:
            written = os.write(1, view)
            view = view[written:]

    def read(self, timeout):
        """Up to 256 bytes; b"" after `timeout` seconds without any; None at end of input."""
        import select

        ready, _, _ = select.select([0], [], [], timeout)
        if not ready:
            return b""
        return os.read(0, 256) or None

    def close(self):
        pass


class SerialLink:
    """A port opened through pyserial: COMx on Windows, /dev/tty* on Linux."""

    def __init__(self, port, baud):
        try:
            import serial
        except ImportError:
            raise SystemExit("uartboot: --port needs pyserial: pip install pyserial")
        self.port = serial.Serial()
        self.port.port = port
        self.port.baudrate = baud
        self.port.timeout = 0
        # Set before open, so the lines never pulse: some USB adapters wire DTR/RTS to reset.
        self.port.dtr = False
        self.port.rts = False
        try:
            self.port.open()
        except serial.SerialException as error:
            raise SystemExit("uartboot: cannot open %s: %s" % (port, error))
        self.port.reset_input_buffer()

    def write(self, data):
        self.port.write(data)

    def read(self, timeout):
        """Up to 256 bytes, or b"" after `timeout` seconds without any."""
        end = time.monotonic() + timeout
        while True:
            waiting = self.port.in_waiting
            if waiting:
                return self.port.read(min(waiting, 256))
            if time.monotonic() >= end:
                return b""
            time.sleep(0.005)

    def close(self):
        self.port.close()


def answer(link, wanted, timeout):
    """Reads the board's lines until one holds a word of `wanted`; returns it, or None.

    Bytes after that line (the next lines can come in the same read) stay on the link for the
    next call.
    """
    end = time.monotonic() + timeout
    pending = getattr(link, "pending", b"")
    try:
        while True:
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                text = line.decode("ascii", "replace").strip()
                if text:
                    log(text)
                for word in wanted:
                    if word in text:
                        return word
            if time.monotonic() >= end:
                return None
            chunk = link.read(max(0.0, end - time.monotonic()))
            if not chunk:
                return None
            link.heard = getattr(link, "heard", 0) + len(chunk)
            if DEBUG:
                log("uartboot: rx %r" % chunk)
            pending += chunk
    finally:
        link.pending = pending


def no_answer(link):
    heard = getattr(link, "heard", 0)
    if heard == 0:
        hint = ("nothing at all came from the board. uart_boot listens only for 3 s after a "
                "restart before it boots FERRITE/FERRITE.BIN, so it was probably running the "
                "application already. Use --reset (restarts a running ferrite application) or "
                "--wait 30 and then reset the board (hold volume up to stay in uart_boot)")
    else:
        last = getattr(link, "pending", b"").decode("ascii", "replace").strip()
        hint = "the board sent %d bytes, but not uart_boot's answer%s" % (
            heard, (": last %r" % last[-80:]) if last else "")
    raise SystemExit("uartboot: no answer from uart_boot; " + hint)


def load_flash(path):
    with open(path, "rb") as file:
        data = file.read()
    if not data:
        raise SystemExit("uartboot: %s is empty" % path)
    return data


def send(link, path, baud, save=False, wait=0.0, reset=False, flash=False):
    data = load_flash(path) if flash else load(path)
    crc = zlib.crc32(data) & 0xFFFFFFFF
    note = ", to be saved as FERRITE/FLASH.BIN" if flash else ", to be saved on CACHE" if save else ""
    log("uartboot: %s, %d bytes, CRC %08x%s" % (os.path.basename(path), len(data), crc, note))
    magic = MAGIC_FLASH if flash else MAGIC_SAVE if save else MAGIC
    header = magic + struct.pack("<II", len(data), crc)
    # uart_boot may be sitting there waiting already: it answers at once.
    link.write(header)
    quick = FIRST_TRY_S if (wait > 0 or reset) else ANSWER_TIMEOUT_S
    got = answer(link, ["receiving", "ERROR"], quick)
    if got != "receiving" and reset:
        # Over a second of silence has let a ferrite application's parser resync after the
        # header bytes; now its restart message.
        log("uartboot: no uart_boot; restarting the application")
        link.write(FERRITE_RESTART)
        wait = max(wait, RESET_WAIT_S)
    if got != "receiving" and wait > 0:
        # Send the header only when uart_boot announces itself: blind repeats could land in
        # U-Boot and stop its autoboot.
        log("uartboot: waiting up to %.0f s for uart_boot; power on or reset the board" % wait)
        end = time.monotonic() + wait
        while got != "receiving":
            remaining = end - time.monotonic()
            if remaining <= 0:
                break
            got = answer(link, ["receiving", "ERROR"] + READY_LINES, remaining)
            if got is None:
                break
            if got in READY_LINES:
                link.write(header)
                got = answer(link, ["receiving", "ERROR"], ANSWER_TIMEOUT_S)
    if got != "receiving":
        no_answer(link)
    begin = time.monotonic()
    for offset in range(0, len(data), CHUNK):
        link.write(data[offset:offset + CHUNK])
        if offset // CHUNK % 16 == 0:
            sys.stderr.write("\ruartboot: %d of %d bytes" % (offset, len(data)))
            sys.stderr.flush()
    # The port's output queue drains at the line's rate: wait for the board's verdict.
    seconds = len(data) * 10 / baud
    result = answer(link, ["CRC OK", "ERROR"], seconds + ANSWER_TIMEOUT_S)
    log("\nuartboot: sent in %.1f s" % (time.monotonic() - begin))
    if result != "CRC OK":
        raise SystemExit("uartboot: the board did not take the image")
    if flash:
        result = answer(link, ["saved", "save ERROR"], SAVE_TIMEOUT_S)
        if result != "saved":
            raise SystemExit("uartboot: FLASH.BIN was NOT saved on CACHE")
        # uart_boot listens again once it says so.
        answer(link, ["waiting again"], ANSWER_TIMEOUT_S)
        return
    if save:
        result = answer(link, ["saved", "save ERROR"], SAVE_TIMEOUT_S)
        if result != "saved":
            raise SystemExit("uartboot: the image runs, but was NOT saved on CACHE")


def monitor(link):
    """Copies what the board sends to stdout until Ctrl-C."""
    log("uartboot: monitoring, Ctrl-C to quit")
    out = sys.stdout.buffer
    try:
        while True:
            chunk = link.read(0.1)
            if chunk:
                out.write(chunk)
                out.flush()
    except KeyboardInterrupt:
        pass


def main():
    parser = argparse.ArgumentParser(description="Send an application image to uart_boot.")
    parser.add_argument("--port", "-p", help="serial port (COM5, /dev/ttyUSB3); default: stdin/stdout, as under minicom")
    parser.add_argument("--baud", "-b", type=int, default=115200, help="baud rate (default 115200, as U-Boot sets it)")
    parser.add_argument("--monitor", "-m", action="store_true", help="with --port: print the board's output afterwards")
    parser.add_argument("--save", "-s", action="store_true",
                        help="save the image on CACHE (FERRITE/FERRITE.BIN) as the application uart_boot boots")
    parser.add_argument("--wait", "-w", type=float, default=0.0, metavar="SECONDS",
                        help="wait this long for uart_boot to start (power on or reset the board meanwhile)")
    parser.add_argument("--reset", "-r", action="store_true",
                        help="if uart_boot does not answer, restart the running ferrite application first")
    parser.add_argument("--flash", "-F", metavar="FILE",
                        help="first save FILE, as it is, as the application's data FERRITE/FLASH.BIN")
    parser.add_argument("--debug", "-d", action="store_true", help="print every byte received")
    parser.add_argument("image", nargs="?", help="<app>.bin or boot.img")
    # minicom passes the file name last, possibly after options of its own.
    args, _ = parser.parse_known_args()
    if args.image is None and args.flash is None:
        parser.error("give an IMAGE, --flash FILE, or both")
    global DEBUG
    DEBUG = args.debug

    if args.port:
        link = SerialLink(args.port, args.baud)
    elif os.name == "nt":
        raise SystemExit("uartboot: on Windows give the port: uartboot.py --port COM5 IMAGE")
    else:
        link = StdioLink()
    try:
        if args.flash:
            send(link, args.flash, args.baud, wait=args.wait, reset=args.reset, flash=True)
        if args.image:
            # After FLASH.BIN, uart_boot is already listening: no waiting or restart.
            first = args.flash is None
            send(link, args.image, args.baud, save=args.save, wait=args.wait if first else 0.0,
                 reset=args.reset and first)
        if args.monitor and args.port:
            monitor(link)
    finally:
        link.close()


if __name__ == "__main__":
    main()
