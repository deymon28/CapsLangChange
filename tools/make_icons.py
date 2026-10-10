"""Generate the tray and application icons in assets/.

Standard library only. Run from the repository root:

    python tools/make_icons.py

The icons are a rounded square in a state colour with a white Caps Lock glyph.
"""

import struct
import zlib
from pathlib import Path

SIZES = (16, 20, 24, 32, 40, 48, 64, 256)
SUPERSAMPLE = 4

STATES = {
    "icon-active.ico": (0x25, 0x63, 0xEB),  # blue
    "icon-paused.ico": (0x6B, 0x72, 0x80),  # grey
    "icon-excluded.ico": (0xD9, 0x77, 0x06),  # amber
}

# Glyph geometry in unit coordinates (0..1, y grows downwards).
HEAD = ((0.50, 0.15), (0.84, 0.50), (0.16, 0.50))
STEM = (0.37, 0.49, 0.63, 0.68)  # x0, y0, x1, y1
BAR = (0.28, 0.75, 0.72, 0.86)
BG_INSET = 0.02
BG_RADIUS = 0.22


def in_rounded_rect(x, y):
    lo, hi = BG_INSET, 1.0 - BG_INSET
    if not (lo <= x <= hi and lo <= y <= hi):
        return False
    r = BG_RADIUS
    cx = min(max(x, lo + r), hi - r)
    cy = min(max(y, lo + r), hi - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def in_rect(x, y, rect):
    x0, y0, x1, y1 = rect
    return x0 <= x <= x1 and y0 <= y <= y1


def in_triangle(x, y, tri):
    (ax, ay), (bx, by), (cx, cy) = tri

    def side(px, py, qx, qy):
        return (x - qx) * (py - qy) - (px - qx) * (y - qy)

    d1 = side(ax, ay, bx, by)
    d2 = side(bx, by, cx, cy)
    d3 = side(cx, cy, ax, ay)
    neg = d1 < 0 or d2 < 0 or d3 < 0
    pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (neg and pos)


def in_glyph(x, y):
    return in_triangle(x, y, HEAD) or in_rect(x, y, STEM) or in_rect(x, y, BAR)


def render(size, colour):
    rows = []
    n = SUPERSAMPLE
    for py in range(size):
        row = bytearray()
        for px in range(size):
            bg = glyph = 0
            for sy in range(n):
                for sx in range(n):
                    x = (px + (sx + 0.5) / n) / size
                    y = (py + (sy + 0.5) / n) / size
                    if in_rounded_rect(x, y):
                        if in_glyph(x, y):
                            glyph += 1
                        else:
                            bg += 1
            total = n * n
            alpha = (bg + glyph) / total
            if alpha == 0:
                row += b"\x00\x00\x00\x00"
                continue
            # Blend glyph (white) over background colour, by coverage.
            g = glyph / (bg + glyph)
            r = round(colour[0] * (1 - g) + 255 * g)
            gr = round(colour[1] * (1 - g) + 255 * g)
            b = round(colour[2] * (1 - g) + 255 * g)
            row += bytes((r, gr, b, round(alpha * 255)))
        rows.append(bytes(row))
    return rows


def png(size, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + row for row in rows)
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico(colour):
    images = [png(s, render(s, colour)) for s in SIZES]
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for size, data in zip(SIZES, images):
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(images)


def main():
    assets = Path(__file__).resolve().parent.parent / "assets"
    assets.mkdir(exist_ok=True)
    for name, colour in STATES.items():
        (assets / name).write_bytes(ico(colour))
        print(f"wrote assets/{name}")


if __name__ == "__main__":
    main()
