# SPDX-License-Identifier: GPL-3.0-or-later
#
# Draws the four tray icons of FR-TRAY-01 into apps/drivemount/ui/tray/*.ico, at 16, 20, 24 and 32 px
# (100 % to 200 % scaling). Each size is drawn from the shapes below, not scaled from another, and
# stored as PNG inside the .ico. Standard library only. Run from anywhere:
#
#     python scripts/make-tray-icons.py
#
# DriveMount's app icon (ui/app-icon.svg), grey when nothing is mounted and in the accent otherwise,
# with a badge at the bottom right for the state: a tick (mounted), a ring (reconnecting) or "!" (error). Shapes, not only
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
METAL = (0x8A, 0x97, 0xA6)  # Stand and base, as in ui/app-icon.svg.
NODE = (0x44, 0x67, 0x8C)
WHITE = (0xFF, 0xFF, 0xFF)
GREEN = (0x2E, 0x9E, 0x4F)
AMBER = (0xE0, 0x9A, 0x1F)
RED = (0xD9, 0x44, 0x3B)

BADGE = (25.0, 25.0, 6.5)  # Centre and radius.
BADGE_GAP = 1.3  # Cleared around the badge, so it stands off the drive.


def lighter(colour, amount):
    return tuple(round(c + (255 - c) * amount) for c in colour)


def darker(colour, amount):
    return tuple(round(c * (1 - amount)) for c in colour)


def mix(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


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
    cx, cy = BADGE[0], BADGE[1]
    points = [(cx - 3.2, cy + 0.2), (cx - 1.0, cy + 2.4), (cx + 3.0, cy - 2.4)]
    return any(near_segment(x, y, points[i], points[i + 1], 2.1) for i in range(2))


def ring(x, y):
    distance = math.hypot(x - BADGE[0], y - BADGE[1])
    return abs(distance - 2.9) <= 0.85


def exclamation(x, y):
    cx, cy = BADGE[0], BADGE[1]
    return near_segment(x, y, (cx, cy - 4.0), (cx, cy + 0.4), 2.2) or math.hypot(x - cx, y - (cy + 3.1)) <= 1.25


def colour_at(x, y, body, badge):
    """The colour at one point, painted back to front, or None where it is clear.

    The shapes of ui/app-icon.svg (256 units) at 1/8: a drive, its stand and base, and a node."""
    colour = None
    if 14.25 <= x <= 17.75 and 18.75 <= y <= 25.75:
        colour = METAL
    if in_rounded_rect(x, y, 3.0, 24.25, 29.0, 27.25, 1.5):
        colour = METAL
    if in_rounded_rect(x, y, 3.0, 5.0, 29.0, 20.5, 3.25):
        colour = mix(lighter(body, 0.12), darker(body, 0.30), (y - 5.0) / 15.5)
        if y < 12.25:
            colour = lighter(colour, 0.14)
        if in_rounded_rect(x, y, 6.25, 14.75, 18.25, 16.75, 1.0):
            colour = lighter(colour, 0.55)
    distance = math.hypot(x - 16.0, y - 25.75)
    if distance <= 3.25:
        colour = WHITE if distance > 2.35 else NODE
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
