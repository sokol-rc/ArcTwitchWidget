#!/usr/bin/env python3
"""Draws the ARC Live mark and writes every icon the app and installer need.

The mark is a rounded dark tile with the accent-green "A" of ARC Raiders, drawn
with 4x supersampling so the small sizes stay readable. Run it after changing
any constant here:

    python3 scripts/generate-icon.py

Outputs (all committed):
    assets/icon.ico        Windows resource: taskbar, Explorer, shortcuts
    assets/icon.png        256px preview for docs and the repository page
    assets/icon-128.rgba   raw RGBA the app loads for the window and the tray
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

SUPERSAMPLE = 4
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)

# Tile background: the dark teal of the app window, slightly lit at the top.
BACKGROUND_TOP = (23, 34, 42)
BACKGROUND_BOTTOM = (11, 16, 19)
# The accent green shared with the OBS widget and the status dots.
ACCENT = (92, 227, 166)
CORNER_RADIUS = 0.22

# The "A": an outer triangle minus its counter minus the gap between the legs.
APEX = (0.50, 0.12)
LEFT_FOOT = (0.12, 0.88)
RIGHT_FOOT = (0.88, 0.88)
COUNTER_APEX = (0.50, 0.34)
COUNTER_LEFT = (0.345, 0.60)
COUNTER_RIGHT = (0.655, 0.60)
GAP_LEFT = 0.335
GAP_RIGHT = 0.665
GAP_TOP = 0.70

# The live dot: the tile is an OBS source, so it carries the recording mark.
LIVE_COLOR = (255, 92, 92)
LIVE_CENTER = (0.775, 0.225)
LIVE_RADIUS = 0.105


def inside_triangle(x: float, y: float, a, b, c) -> bool:
    def side(p, q):
        return (q[0] - p[0]) * (y - p[1]) - (q[1] - p[1]) * (x - p[0])

    first = side(a, b)
    second = side(b, c)
    third = side(c, a)
    return (first >= 0 and second >= 0 and third >= 0) or (
        first <= 0 and second <= 0 and third <= 0
    )


def inside_tile(x: float, y: float) -> bool:
    radius = CORNER_RADIUS
    cx = min(max(x, radius), 1.0 - radius)
    cy = min(max(y, radius), 1.0 - radius)
    dx = x - cx
    dy = y - cy
    return dx * dx + dy * dy <= radius * radius


def inside_letter(x: float, y: float) -> bool:
    if not inside_triangle(x, y, APEX, LEFT_FOOT, RIGHT_FOOT):
        return False
    if inside_triangle(x, y, COUNTER_APEX, COUNTER_LEFT, COUNTER_RIGHT):
        return False
    return not (GAP_LEFT <= x <= GAP_RIGHT and y >= GAP_TOP)


def inside_live_dot(x: float, y: float) -> bool:
    dx = x - LIVE_CENTER[0]
    dy = y - LIVE_CENTER[1]
    return dx * dx + dy * dy <= LIVE_RADIUS * LIVE_RADIUS


def blend(base, top, amount: float):
    return tuple(
        round(channel + (other - channel) * amount) for channel, other in zip(base, top)
    )


def render(size: int) -> bytes:
    """Returns straight RGBA rows, top-down."""
    pixels = bytearray(size * size * 4)
    step = 1.0 / (size * SUPERSAMPLE)
    samples = SUPERSAMPLE * SUPERSAMPLE
    for row in range(size):
        for column in range(size):
            tile = 0
            letter = 0
            live = 0
            for sub_y in range(SUPERSAMPLE):
                y = (row * SUPERSAMPLE + sub_y + 0.5) * step
                for sub_x in range(SUPERSAMPLE):
                    x = (column * SUPERSAMPLE + sub_x + 0.5) * step
                    if inside_tile(x, y):
                        tile += 1
                        if inside_letter(x, y):
                            letter += 1
                        if inside_live_dot(x, y):
                            live += 1
            index = (row * size + column) * 4
            if tile == 0:
                continue
            vertical = (row + 0.5) / size
            background = tuple(
                round(top + (bottom - top) * vertical)
                for top, bottom in zip(BACKGROUND_TOP, BACKGROUND_BOTTOM)
            )
            tile_coverage = tile / samples
            # The letter and the dot are drawn over the tile, then the whole tile
            # is faded by its own coverage so the rounded corners stay smooth.
            color = blend(background, ACCENT, letter / tile)
            color = blend(color, LIVE_COLOR, live / tile)
            pixels[index] = color[0]
            pixels[index + 1] = color[1]
            pixels[index + 2] = color[2]
            pixels[index + 3] = round(255 * tile_coverage)
    return bytes(pixels)


def write_png(path: Path, size: int, rgba: bytes) -> None:
    raw = bytearray()
    stride = size * 4
    for row in range(size):
        raw.append(0)
        raw.extend(rgba[row * stride : (row + 1) * stride])

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + kind
            + payload
            + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def dib_entry(size: int, rgba: bytes) -> bytes:
    """Classic ICO entry: 32-bit bottom-up BGRA plus an empty AND mask."""
    body = bytearray()
    stride = size * 4
    for row in reversed(range(size)):
        line = rgba[row * stride : (row + 1) * stride]
        for offset in range(0, stride, 4):
            body.append(line[offset + 2])
            body.append(line[offset + 1])
            body.append(line[offset])
            body.append(line[offset + 3])
    mask_stride = ((size + 31) // 32) * 4
    body.extend(b"\x00" * (mask_stride * size))
    header = struct.pack(
        "<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(body), 0, 0, 0, 0
    )
    return header + bytes(body)


def write_ico(path: Path, images: dict[int, bytes]) -> None:
    entries = []
    for size in sorted(images):
        rgba = images[size]
        if size >= 128:
            temporary = path.with_suffix(f".{size}.png")
            write_png(temporary, size, rgba)
            payload = temporary.read_bytes()
            temporary.unlink()
        else:
            payload = dib_entry(size, rgba)
        entries.append((size, payload))

    offset = 6 + 16 * len(entries)
    directory = bytearray(struct.pack("<HHH", 0, 1, len(entries)))
    for size, payload in entries:
        directory.extend(
            struct.pack(
                "<BBBBHHII",
                0 if size >= 256 else size,
                0 if size >= 256 else size,
                0,
                0,
                1,
                32,
                len(payload),
                offset,
            )
        )
        offset += len(payload)
    path.write_bytes(bytes(directory) + b"".join(payload for _, payload in entries))


def main() -> None:
    ASSETS.mkdir(parents=True, exist_ok=True)
    images = {size: render(size) for size in ICO_SIZES}
    write_ico(ASSETS / "icon.ico", images)
    write_png(ASSETS / "icon.png", 256, images[256])
    (ASSETS / "icon-128.rgba").write_bytes(images[128])
    print(f"icon.ico: {(ASSETS / 'icon.ico').stat().st_size} bytes")


if __name__ == "__main__":
    main()
