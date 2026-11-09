# SPDX-License-Identifier: GPL-3.0-or-later
#
# Draws the four tray icons of FR-TRAY-01 into apps/drivemount/ui/tray/*.ico, at 16, 20, 24 and 32 px
# (100 % to 200 % scaling). Each size is drawn from the shapes below, not scaled from another, and
# stored as PNG inside the .ico. Standard library only. Run from anywhere:
#
#     python scripts/make-tray-icons.py
#
# A drive, grey when nothing is mounted and in the accent otherwise, with a badge at the bottom
# right for the state: a tick (mounted), a ring (reconnecting) or "!" (error). Shapes, not only
# colours, tell the states apart.

import math
import os
import struct
import zlib

SIZES = (16, 20, 24, 32)
SUPERSAMPLE = 8
UNITS = 32.0  # The shapes are in a 32 x 32 box.

GREY = (0x8C, 0x93, 0x9D)
ACCENT = (0x5B, 0x83, 0xB0)
WHITE = (0xFF, 0xFF, 0xFF)
GREEN = (0x2E, 0x9E, 0x4F)
AMBER = (0xE0, 0x9A, 0x1F)
RED = (0xD9, 0x44, 0x3B)

BADGE = (24.0, 24.0, 7.5)  # Centre and radius.
BADGE_GAP = 1.5  # Cleared around the badge, so it stands off the drive.


def lighter(colour, amount):
    return tuple(round(c + (255 - c) * amount) for c in colour)


def in_rounded_rect(x, y, left, top, right, bottom, radius):
    cx = min(max(x, left + radius), right - radius)
    cy = min(max(y, top + radius), bottom - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius**2 and left <= x <= right and top <= y <= bottom


def near_segment(x, y, a, b, width):
    (ax, ay), (bx, by) = a, b
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)))
    px, py = ax + t * dx, ay + t * dy
    return (x - px) ** 2 + (y - py) ** 2 <= (width / 2) ** 2


def tick(x, y):
    points = [(20.4, 24.2), (23.0, 26.8), (27.6, 21.4)]
    return any(near_segment(x, y, points[i], points[i + 1], 2.3) for i in range(2))


def ring(x, y):
    distance = math.hypot(x - BADGE[0], y - BADGE[1])
    return abs(distance - 3.3) <= 0.95


def exclamation(x, y):
    return near_segment(x, y, (24.0, 19.9), (24.0, 24.9), 2.4) or math.hypot(x - 24.0, y - 28.0) <= 1.35


def colour_at(x, y, body, badge):
    """The colour at one point, painted back to front, or None where it is clear."""
    colour = None
    if in_rounded_rect(x, y, 2.0, 8.0, 28.0, 22.0, 3.5):
        colour = lighter(body, 0.18) if y < 14.6 else body
        if 14.6 <= y < 15.6:
            colour = lighter(body, 0.45)
        if math.hypot(x - 7.0, y - 18.6) <= 1.35 or math.hypot(x - 11.0, y - 18.6) <= 1.35:
            colour = WHITE
    if badge is not None:
        fill, glyph = badge
        distance = math.hypot(x - BADGE[0], y - BADGE[1])
        if distance <= BADGE[2]:
            colour = WHITE if glyph(x, y) else fill
        elif distance <= BADGE[2] + BADGE_GAP:
            colour = None
    return colour


def draw(size, body, badge):
    """RGBA rows, each pixel the average of SUPERSAMPLE² points."""
    scale = UNITS / size
    rows = []
    for py in range(size):
        row = bytearray()
        for px in range(size):
            r = g = b = a = 0
            for sy in range(SUPERSAMPLE):
                for sx in range(SUPERSAMPLE):
                    x = (px + (sx + 0.5) / SUPERSAMPLE) * scale
                    y = (py + (sy + 0.5) / SUPERSAMPLE) * scale
                    colour = colour_at(x, y, body, badge)
                    if colour is not None:
                        r, g, b, a = r + colour[0], g + colour[1], b + colour[2], a + 1
            if a == 0:
                row += bytes(4)
            else:
                row += bytes((r // a, g // a, b // a, round(255 * a / SUPERSAMPLE**2)))
        rows.append(bytes(row))
    return rows


def png(size, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    pixels = zlib.compress(b"".join(b"\x00" + row for row in rows), 9)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", pixels) + chunk(b"IEND", b"")


def ico(images):
    """An .ico with one PNG per size."""
    directory = struct.pack("<HHH", 0, 1, len(images))
    offset = len(directory) + 16 * len(images)
    entries, data = b"", b""
    for size, image in images:
        entries += struct.pack("<BBBBHHII", size, size, 0, 0, 1, 32, len(image), offset)
        data += image
        offset += len(image)
    return directory + entries + data


VARIANTS = {
    "idle": (GREY, None),
    "mounted": (ACCENT, (GREEN, tick)),
    "reconnecting": (ACCENT, (AMBER, ring)),
    "error": (ACCENT, (RED, exclamation)),
}


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    folder = os.path.join(root, "apps", "drivemount", "ui", "tray")
    os.makedirs(folder, exist_ok=True)
    for name, (body, badge) in VARIANTS.items():
        images = [(size, png(size, draw(size, body, badge))) for size in SIZES]
        path = os.path.join(folder, name + ".ico")
        with open(path, "wb") as file:
            file.write(ico(images))
        print(path)


if __name__ == "__main__":
    main()
