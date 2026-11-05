#!/usr/bin/env python3
"""Writes a small H.264 test stream whose decoded frames are known exactly, for `mfc_test`.

Baseline profile, CAVLC, one slice per picture, no deblocking. Every macroblock that changes is
an I_PCM macroblock, its samples sent as they are; every other one is P_Skip, a copy of the
previous picture (all motion vectors are zero, so the prediction of a skip is zero too). The
decoder's output is therefore exactly the pictures drawn here, which this script also writes:

    OUT.h264    the stream, Annex B (start codes)
    OUT.crc     one line per frame: the CRC-32 (zlib) of its luma plane, then of its
                interleaved chroma plane (NV12, CbCr), both as visible, row after row
    OUT.ref0    frame 0 as NV12, for finding where tiles went if the CRCs do not match

The pictures: a diagonal gradient, bars that move from picture to picture, and a 64x64 square
that walks across; only the macroblocks the moving parts touch are sent again. Pictures 0 and
IDR_PERIOD, 2 * IDR_PERIOD, ... are IDR pictures, all I_PCM.

Usage: h264_pcm.py [--width 320] [--height 240] [--frames 30] OUT
"""

import argparse
import zlib

IDR_PERIOD = 15
SQUARE = 64


class Bits:
    def __init__(self):
        self.bits = []

    def u(self, count, value):
        for shift in range(count - 1, -1, -1):
            self.bits.append((value >> shift) & 1)

    def ue(self, value):
        value += 1
        length = value.bit_length()
        self.u(length - 1, 0)
        self.u(length, value)

    def se(self, value):
        self.ue(2 * value - 1 if value > 0 else -2 * value)

    def align_zero(self):
        while len(self.bits) % 8:
            self.bits.append(0)

    def trailing(self):
        self.bits.append(1)
        self.align_zero()

    def raw(self, data):
        assert len(self.bits) % 8 == 0
        for byte in data:
            self.u(8, byte)

    def bytes(self):
        assert len(self.bits) % 8 == 0
        out = bytearray()
        for i in range(0, len(self.bits), 8):
            byte = 0
            for bit in self.bits[i:i + 8]:
                byte = byte << 1 | bit
            out.append(byte)
        return bytes(out)


def nal(ref_idc, kind, rbsp):
    """Start code, header, and the RBSP with emulation prevention bytes."""
    out = bytearray(b"\x00\x00\x00\x01")
    out.append(ref_idc << 5 | kind)
    zeros = 0
    for byte in rbsp:
        if zeros >= 2 and byte <= 3:
            out.append(3)
            zeros = 0
        out.append(byte)
        zeros = zeros + 1 if byte == 0 else 0
    return bytes(out)


def sps(width, height):
    b = Bits()
    b.u(8, 66)          # profile_idc: Baseline
    b.u(8, 0xC0)        # constraint_set0 and set1
    b.u(8, 30)          # level_idc 3.0
    b.ue(0)             # seq_parameter_set_id
    b.ue(0)             # log2_max_frame_num_minus4: frame_num has 4 bits
    b.ue(2)             # pic_order_cnt_type 2: output order is decoding order
    b.ue(1)             # max_num_ref_frames
    b.u(1, 0)           # gaps_in_frame_num_value_allowed_flag
    b.ue(width // 16 - 1)
    b.ue(height // 16 - 1)
    b.u(1, 1)           # frame_mbs_only_flag
    b.u(1, 1)           # direct_8x8_inference_flag
    b.u(1, 0)           # frame_cropping_flag
    b.u(1, 1)           # vui_parameters_present_flag
    b.u(1, 0)           # aspect_ratio_info_present_flag
    b.u(1, 0)           # overscan_info_present_flag
    b.u(1, 0)           # video_signal_type_present_flag
    b.u(1, 0)           # chroma_loc_info_present_flag
    b.u(1, 0)           # timing_info_present_flag
    b.u(1, 0)           # nal_hrd_parameters_present_flag
    b.u(1, 0)           # vcl_hrd_parameters_present_flag
    b.u(1, 0)           # pic_struct_present_flag
    b.u(1, 1)           # bitstream_restriction_flag
    b.u(1, 1)           # motion_vectors_over_pic_boundaries_flag
    b.ue(0)             # max_bytes_per_pic_denom
    b.ue(0)             # max_bits_per_mb_denom
    b.ue(16)            # log2_max_mv_length_horizontal
    b.ue(16)            # log2_max_mv_length_vertical
    b.ue(0)             # max_num_reorder_frames: each picture can be shown at once
    b.ue(1)             # max_dec_frame_buffering
    b.trailing()
    return nal(3, 7, b.bytes())


def pps():
    b = Bits()
    b.ue(0)             # pic_parameter_set_id
    b.ue(0)             # seq_parameter_set_id
    b.u(1, 0)           # entropy_coding_mode_flag: CAVLC
    b.u(1, 0)           # bottom_field_pic_order_in_frame_present_flag
    b.ue(0)             # num_slice_groups_minus1
    b.ue(0)             # num_ref_idx_l0_default_active_minus1
    b.ue(0)             # num_ref_idx_l1_default_active_minus1
    b.u(1, 0)           # weighted_pred_flag
    b.u(2, 0)           # weighted_bipred_idc
    b.se(0)             # pic_init_qp_minus26
    b.se(0)             # pic_init_qs_minus26
    b.se(0)             # chroma_qp_index_offset
    b.u(1, 1)           # deblocking_filter_control_present_flag
    b.u(1, 0)           # constrained_intra_pred_flag
    b.u(1, 0)           # redundant_pic_cnt_present_flag
    b.trailing()
    return nal(3, 8, b.bytes())


def pcm(b, frame, width, mb_x, mb_y):
    """pcm_alignment_zero_bits, then the macroblock's 256 luma, 64 Cb and 64 Cr samples."""
    y_plane, cb_plane, cr_plane = frame
    b.align_zero()
    samples = bytearray()
    for row in range(16):
        start = (mb_y * 16 + row) * width + mb_x * 16
        samples += y_plane[start:start + 16]
    for plane in (cb_plane, cr_plane):
        for row in range(8):
            start = (mb_y * 8 + row) * (width // 2) + mb_x * 8
            samples += plane[start:start + 8]
    b.raw(samples)


def slice_nal(frame, width, height, number, idr, changed):
    b = Bits()
    b.ue(0)                                 # first_mb_in_slice
    b.ue(7 if idr else 5)                   # slice_type: I or P, all slices alike
    b.ue(0)                                 # pic_parameter_set_id
    b.u(4, number % IDR_PERIOD % 16)        # frame_num
    if idr:
        b.ue(number // IDR_PERIOD % 65536)  # idr_pic_id
    else:
        b.u(1, 0)                           # num_ref_idx_active_override_flag
        b.u(1, 0)                           # ref_pic_list_modification_flag_l0
    if idr:
        b.u(1, 0)                           # no_output_of_prior_pics_flag
        b.u(1, 0)                           # long_term_reference_flag
    else:
        b.u(1, 0)                           # adaptive_ref_pic_marking_mode_flag
    b.se(0)                                 # slice_qp_delta
    b.ue(1)                                 # disable_deblocking_filter_idc: off
    mbs_x, mbs_y = width // 16, height // 16
    skipped = 0
    for mb in range(mbs_x * mbs_y):
        mb_x, mb_y = mb % mbs_x, mb // mbs_x
        if idr:
            b.ue(25)                        # mb_type I_PCM
            pcm(b, frame, width, mb_x, mb_y)
        elif (mb_x, mb_y) in changed:
            b.ue(skipped)                   # mb_skip_run
            skipped = 0
            b.ue(5 + 25)                    # mb_type: I_PCM in a P slice
            pcm(b, frame, width, mb_x, mb_y)
        else:
            skipped += 1
    if skipped:
        b.ue(skipped)
    b.trailing()
    return nal(3 if idr else 2, 5 if idr else 1, b.bytes())


def clamp(value):
    # Kept away from 0: early editions of the standard do not allow a PCM sample of 0.
    return max(16, min(235, value))


def draw(width, height, number):
    """Picture `number` as Y, Cb, Cr planes (4:2:0)."""
    y_plane = bytearray(width * height)
    bar = (number * 8) % width
    sx = (number * 16) % (width - SQUARE)
    sy = (height - SQUARE) // 2
    for y in range(height):
        for x in range(width):
            value = (x + y) * 200 // (width + height) + 20
            if (x - bar) % width < 16:
                value = 230
            if sx <= x < sx + SQUARE and sy <= y < sy + SQUARE:
                value = 40 + ((x - sx) ^ (y - sy)) % 64 * 3
            y_plane[y * width + x] = clamp(value)
    cb_plane = bytearray(width * height // 4)
    cr_plane = bytearray(width * height // 4)
    for y in range(height // 2):
        for x in range(width // 2):
            cb_plane[y * (width // 2) + x] = clamp(64 + x * 128 // (width // 2))
            cr_plane[y * (width // 2) + x] = clamp(192 - y * 128 // (height // 2))
    return y_plane, cb_plane, cr_plane


def nv12(frame, width, height):
    y_plane, cb_plane, cr_plane = frame
    chroma = bytearray(width * height // 2)
    chroma[0::2] = cb_plane
    chroma[1::2] = cr_plane
    return bytes(y_plane), bytes(chroma)


def main():
    parser = argparse.ArgumentParser(description="Write an exactly decodable H.264 test stream.")
    parser.add_argument("--width", type=int, default=320)
    parser.add_argument("--height", type=int, default=240)
    parser.add_argument("--frames", type=int, default=30)
    parser.add_argument("out", help="prefix of the output files")
    args = parser.parse_args()
    width, height = args.width, args.height
    assert width % 16 == 0 and height % 16 == 0 and width >= SQUARE + 16

    stream = bytearray(sps(width, height) + pps())
    shown = None
    crcs = []
    for number in range(args.frames):
        drawn = draw(width, height, number)
        idr = number % IDR_PERIOD == 0
        changed = set()
        if not idr:
            for mb_y in range(height // 16):
                for mb_x in range(width // 16):
                    for plane, size, w in ((0, 16, width), (1, 8, width // 2), (2, 8, width // 2)):
                        rows = range(mb_y * size, mb_y * size + size)
                        if any(drawn[plane][r * w + mb_x * size:r * w + mb_x * size + size]
                               != shown[plane][r * w + mb_x * size:r * w + mb_x * size + size] for r in rows):
                            changed.add((mb_x, mb_y))
                            break
        # The decoded picture: drawn where sent, the previous one elsewhere.
        if idr:
            shown = tuple(bytearray(plane) for plane in drawn)
        else:
            for mb_x, mb_y in changed:
                for plane, size, w in ((0, 16, width), (1, 8, width // 2), (2, 8, width // 2)):
                    for r in range(mb_y * size, mb_y * size + size):
                        start = r * w + mb_x * size
                        shown[plane][start:start + size] = drawn[plane][start:start + size]
        stream += slice_nal(shown, width, height, number, idr, changed)
        luma, chroma = nv12(shown, width, height)
        crcs.append((zlib.crc32(luma), zlib.crc32(chroma)))
        if number == 0:
            with open(args.out + ".ref0", "wb") as file:
                file.write(luma + chroma)

    with open(args.out + ".h264", "wb") as file:
        file.write(stream)
    with open(args.out + ".crc", "w") as file:
        for luma, chroma in crcs:
            file.write("%08x %08x\n" % (luma, chroma))
    print("%s.h264: %dx%d, %d frames, %d bytes" % (args.out, width, height, args.frames, len(stream)))


if __name__ == "__main__":
    main()
