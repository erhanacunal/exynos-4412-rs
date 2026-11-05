#!/usr/bin/env python3
"""Takes the H.264 video track out of an MP4 file as an Annex B stream, for `mfc_play`.

An MP4 file keeps the sequence and picture parameter sets in the track's `avcC` box and puts a
length before each NAL unit; the MFC wants start codes. This writes the parameter sets first,
then every sample's NAL units in decoding order, each after a start code. Audio and the other
tracks are left out. Standard library only.

Usage: mp4_annexb.py IN.mp4 OUT.h264
"""

import struct
import sys

START_CODE = b"\x00\x00\x00\x01"
CONTAINERS = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"dinf"}


def boxes(data, start, end):
    """(type, body start, body end) of each box in data[start:end]."""
    at = start
    while at + 8 <= end:
        size, kind = struct.unpack_from(">I4s", data, at)
        header = 8
        if size == 1:
            size, = struct.unpack_from(">Q", data, at + 8)
            header = 16
        elif size == 0:
            size = end - at
        if size < header or at + size > end:
            raise SystemExit("mp4_annexb: broken box %r at %d" % (kind, at))
        yield kind, at + header, at + size
        at += size


def find(data, start, end, path):
    """The bodies of the boxes at `path` (types), under data[start:end]."""
    for kind, body, stop in boxes(data, start, end):
        if kind == path[0]:
            if len(path) == 1:
                yield body, stop
            elif kind in CONTAINERS:
                yield from find(data, body, stop, path[1:])


def one(data, start, end, path):
    for found in find(data, start, end, path):
        return found
    raise SystemExit("mp4_annexb: no %s box" % b"/".join(path).decode())


def video_track(data):
    moov = one(data, 0, len(data), [b"moov"])
    for trak in find(data, *moov, [b"trak"]):
        hdlr = one(data, *trak, [b"mdia", b"hdlr"])
        # Full box: version and flags, pre_defined, then the handler type.
        if data[hdlr[0] + 8:hdlr[0] + 12] == b"vide":
            return trak
    raise SystemExit("mp4_annexb: no video track")


def avcc(data, stbl):
    stsd = one(data, *stbl, [b"stsd"])
    # Full box and entry count, then the sample entry: an avc1 (or avc3) box.
    entries = stsd[0] + 8
    for kind, body, stop in boxes(data, entries, stsd[1]):
        if kind not in (b"avc1", b"avc3"):
            raise SystemExit("mp4_annexb: the video is %s, not H.264" % kind.decode(errors="replace"))
        # The visual sample entry's 78 bytes, then its boxes.
        return one(data, body + 78, stop, [b"avcC"])
    raise SystemExit("mp4_annexb: empty stsd")


def parameter_sets(data, box):
    """The length size of the samples' NAL units, and the SPS and PPS units."""
    body = data[box[0]:box[1]]
    length_size = (body[4] & 3) + 1
    units = []
    at = 5
    for count_mask in (0x1F, 0xFF):
        count = body[at] & count_mask
        at += 1
        for _ in range(count):
            size, = struct.unpack_from(">H", body, at)
            units.append(body[at + 2:at + 2 + size])
            at += 2 + size
    return length_size, units


def samples(data, stbl):
    """(offset, size) of each sample, in order."""
    stsz = one(data, *stbl, [b"stsz"])
    uniform, count = struct.unpack_from(">II", data, stsz[0] + 4)
    sizes = [uniform] * count if uniform else list(struct.unpack_from(">%dI" % count, data, stsz[0] + 12))
    try:
        stco = one(data, *stbl, [b"stco"])
        n, = struct.unpack_from(">I", data, stco[0] + 4)
        chunks = list(struct.unpack_from(">%dI" % n, data, stco[0] + 8))
    except SystemExit:
        co64 = one(data, *stbl, [b"co64"])
        n, = struct.unpack_from(">I", data, co64[0] + 4)
        chunks = list(struct.unpack_from(">%dQ" % n, data, co64[0] + 8))
    stsc = one(data, *stbl, [b"stsc"])
    n, = struct.unpack_from(">I", data, stsc[0] + 4)
    runs = [struct.unpack_from(">III", data, stsc[0] + 8 + 12 * i) for i in range(n)]
    out = []
    sample = 0
    for i, (first, per_chunk, _) in enumerate(runs):
        last = runs[i + 1][0] - 1 if i + 1 < len(runs) else len(chunks)
        for chunk in range(first, last + 1):
            offset = chunks[chunk - 1]
            for _ in range(per_chunk):
                if sample == count:
                    return out
                out.append((offset, sizes[sample]))
                offset += sizes[sample]
                sample += 1
    return out


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__.strip())
    with open(sys.argv[1], "rb") as file:
        data = file.read()
    trak = video_track(data)
    stbl = one(data, *trak, [b"mdia", b"minf", b"stbl"])
    length_size, units = parameter_sets(data, avcc(data, stbl))
    out = bytearray()
    for unit in units:
        out += START_CODE + unit
    pictures = samples(data, stbl)
    for offset, size in pictures:
        at, end = offset, offset + size
        while at < end:
            length = int.from_bytes(data[at:at + length_size], "big")
            at += length_size
            out += START_CODE + data[at:at + length]
            at += length
    with open(sys.argv[2], "wb") as file:
        file.write(out)
    print("%s: %d parameter sets, %d pictures, %d bytes" % (sys.argv[2], len(units), len(pictures), len(out)))


if __name__ == "__main__":
    main()
