#!/usr/bin/env python3
"""Test pictures for jpeg_test: one synthetic 320x240 picture, as baseline JPEG in 4:2:0, 4:2:2,
4:4:4 and grey, each with the RGB that libjpeg (through ImageMagick) decodes it to.

libjpeg decodes with its "fancy" chroma upsampling off: the codec repeats chroma rather than
interpolating it. On the board the 4:2:0 file came out at 29.8 dB against the interpolated
reference, 38.0 dB against this one. The board's decoder
is still not expected to match libjpeg bit for bit (another IDCT); jpeg_test compares within a
PSNR bound.

Needs ImageMagick's `convert`. Writes into the directory given (jpeg_test/data):
  pattern.ppm (not used by the board), test<kind>.jpg, test<kind>.rgb (24 bits a pixel).
"""

import math
import os
import subprocess
import sys

WIDTH, HEIGHT = 320, 240
KINDS = {"420": "2x2", "422": "2x1", "444": "1x1", "grey": None}
QUALITY = 90


def pixel(x, y):
    # Smooth background: hue across, brightness down.
    r = int(255 * x / (WIDTH - 1))
    g = int(255 * y / (HEIGHT - 1))
    b = int(255 * (1 - x / (WIDTH - 1)))
    # A white disc with a dark ring.
    d = math.hypot(x - 100, y - 120)
    if d < 40:
        r, g, b = 240, 240, 240
    elif d < 48:
        r, g, b = 20, 20, 60
    # Large checkerboard, sharp edges, in the right part.
    if 200 <= x < 300 and 40 <= y < 200:
        if ((x - 200) // 20 + (y - 40) // 20) % 2:
            r, g, b = 220, 30, 30
        else:
            r, g, b = 30, 200, 60
    return r, g, b


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "jpeg_test/data"
    os.makedirs(out, exist_ok=True)
    ppm = os.path.join(out, "pattern.ppm")
    with open(ppm, "wb") as f:
        f.write(b"P6 %d %d 255\n" % (WIDTH, HEIGHT))
        f.write(bytes(c for y in range(HEIGHT) for x in range(WIDTH) for c in pixel(x, y)))
    for kind, factor in KINDS.items():
        jpg = os.path.join(out, f"test{kind}.jpg")
        args = ["convert", ppm, "-strip", "-quality", str(QUALITY), "-interlace", "none"]
        args += ["-colorspace", "Gray", "-type", "Grayscale"] if factor is None else ["-sampling-factor", factor]
        subprocess.run(args + [jpg], check=True)
        rgb = os.path.join(out, f"test{kind}.rgb")
        # Chroma repeated, not interpolated: see above.
        subprocess.run(["convert", "-define", "jpeg:fancy-upsampling=off", jpg, "-depth", "8", f"rgb:{rgb}"], check=True)
        size = os.path.getsize(rgb)
        assert size == WIDTH * HEIGHT * 3, (rgb, size)
        print(f"{jpg}: {os.path.getsize(jpg)} bytes")
    os.remove(ppm)


if __name__ == "__main__":
    main()
