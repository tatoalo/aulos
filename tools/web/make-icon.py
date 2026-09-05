#!/usr/bin/env python3
"""Render crates/aulos-api/web/icon-180.png — the same mark as icon.svg, rasterised.

Pillow is not a dependency of this repository, so the rasteriser is 80 lines of pure Python:
supersample 3x with signed-distance coverage for the rounded square, the three strokes and the
five tone holes, box-downsample, then write a PNG with zlib and the Sub filter (a diagonal
gradient's horizontal deltas are near-constant, which is what keeps the file a couple of KB).

    python3 tools/web/make-icon.py
"""

from __future__ import annotations

import binascii
import math
import pathlib
import struct
import zlib

SIZE = 180
SS = 3                      # supersampling factor
N = SIZE * SS
OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/aulos-api/web/icon-180.png"

TOP = (0xE0, 0x78, 0x50)    # --accent
BOTTOM = (0x8B, 0x3A, 0x4C)  # --accent-deep
WHITE = (0xFF, 0xFF, 0xFF)

# The 24-grid mark from icon.svg, placed with translate(40.5 40.5) scale(4.125).
SCALE = 4.125 * SS
OFF = 40.5 * SS
STROKE = 1.8 * SCALE / 2.0  # half-width, matching stroke-width="1.8"
DOT = 0.6 * SCALE

SEGMENTS = [((9, 3), (9, 21)), ((15, 3), (15, 21)), ((9, 3), (15, 3))]
DOTS = [(9, 9), (9, 13), (15, 9), (15, 13), (15, 17)]


def g(p):
    return (p[0] * SCALE + OFF, p[1] * SCALE + OFF)


SEGMENTS = [(g(a), g(b)) for a, b in SEGMENTS]
DOTS = [g(p) for p in DOTS]


def dist_to_segment(px, py, a, b):
    ax, ay = a
    bx, by = b
    dx, dy = bx - ax, by - ay
    span = dx * dx + dy * dy
    t = 0.0 if span == 0 else max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / span))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def render():
    # Accumulate RGB sums per output pixel, then divide by SS*SS.
    acc = [[0, 0, 0] for _ in range(SIZE * SIZE)]
    inv = 1.0 / (N - 1)
    for sy in range(N):
        row_out = (sy // SS) * SIZE
        for sx in range(N):
            # Full bleed on purpose: iOS masks an apple-touch-icon to its own rounded square, so a
            # rounded (or transparent) corner here would only ever show up as a black notch.
            t = (sx * inv + sy * inv) / 2.0                       # the 135deg gradient
            r = TOP[0] + (BOTTOM[0] - TOP[0]) * t
            gg = TOP[1] + (BOTTOM[1] - TOP[1]) * t
            b = TOP[2] + (BOTTOM[2] - TOP[2]) * t
            px, py = sx + 0.5, sy + 0.5
            on_mark = any(dist_to_segment(px, py, a, bb) <= STROKE for a, bb in SEGMENTS) or any(
                math.hypot(px - cx, py - cy) <= DOT for cx, cy in DOTS
            )
            if on_mark:
                r, gg, b = WHITE
            cell = acc[row_out + (sx // SS)]
            cell[0] += r
            cell[1] += gg
            cell[2] += b
    n = SS * SS
    return bytes(
        int(round(c / n)) & 0xFF
        for cell in acc
        for c in cell
    )


def png(pixels: bytes) -> bytes:
    raw = bytearray()
    stride = SIZE * 3
    for y in range(SIZE):
        row = pixels[y * stride:(y + 1) * stride]
        raw.append(1)  # filter: Sub
        for i, v in enumerate(row):
            left = row[i - 3] if i >= 3 else 0
            raw.append((v - left) & 0xFF)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", binascii.crc32(kind + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


if __name__ == "__main__":
    blob = png(render())
    OUT.write_bytes(blob)
    print(f"{OUT} — {len(blob)} bytes")
    if len(blob) > 10_000:
        raise SystemExit("icon-180.png must stay under 10 KB")
