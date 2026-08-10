#!/usr/bin/env python3
"""Build BoaVoice's app icon from `packaging/boa-source.png`, then an `.icns`.

No third-party imaging library is needed: the PNG codec here is the ~120 lines of zlib +
struct the format actually requires, so the icon rebuilds from a plain checkout with
nothing but a Python interpreter. It is a close relative of RedPython's script, which is
where the approach comes from.

The source is white line art on nothing — `boa-source.svg` rasterises to RGBA with a
transparent background, so the alpha channel already *is* the coverage mask, exactly as the
drawing program laid it down. Antialiased edges and round stroke caps all survive without a
threshold anywhere.

What is left to do is composite: area-average the mask down to each size macOS asks for,
paint the glyph white onto a green superellipse tile, and hand the set to `iconutil`.

The one number that differs from the red sibling is the fill fraction. That artwork is a
wide, flat head; this one is a boa's head seen from above, half as wide as it is tall. The
fit is limited by height, so the same fraction would leave a third of the tile empty on
either side — these are larger to compensate.

Usage:
    python3 scripts/make-icon.py [out.icns]
"""

import argparse
import os
import struct
import subprocess
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PACKAGING = os.path.join(ROOT, "packaging")

# Rasterised from boa-source.svg by scripts/raster-svg.sh and committed, which
# keeps this script free of any SVG dependency.
SOURCE = os.path.join(PACKAGING, "boa-source.png")

# macOS wants these (size, scale) pairs in an .iconset.
VARIANTS = [
    (16, 1), (16, 2), (32, 1), (32, 2), (128, 1), (128, 2),
    (256, 1), (256, 2), (512, 1), (512, 2),
]

# The tile. A vertical ramp rather than a flat fill so the icon has the same top-lit shading
# as the rest of macOS's dock. A bright, saturated green rather than Catppuccin's soft
# `#a6e3a1`: the palette colour is chosen to sit *behind text* in a dark interface, and at
# 32 pixels in a dock full of saturated icons it reads as grey-green. The accent inside the
# app stays the palette's.
TILE_TOP = (52, 214, 92)
TILE_BOTTOM = (8, 138, 56)
INK = (255, 255, 255)

EXP = 4.6      # superellipse exponent ≈ the macOS "squircle"
EXTENT = 0.92  # tile size within the canvas, leaving the usual icon padding
SS = 3         # supersampling factor for the tile edge


def glyph_fill(size):
    """Fraction of the tile width the drawing spans.

    Larger when pixels are scarce: at 16–32 px the padding is worth more as silhouette than
    as breathing room. And larger throughout than the wide-headed sibling this came from,
    because a glyph twice as tall as it is wide is fitted by its *height* — the same fraction
    would leave the tile visibly empty at the sides.
    """
    if size <= 32:
        return 0.96
    if size <= 64:
        return 0.92
    return 0.86


def stroke_gain(size):
    """Coverage multiplier keeping thin strokes alive once downscaled.

    Area-averaging a 22-unit stroke into a 16 px icon leaves it at maybe a third
    coverage, which renders as pink haze rather than a line. Scaling coverage up
    (and clamping) restores the line's weight without touching its position.
    """
    if size <= 16:
        return 2.2
    if size <= 32:
        return 1.5
    if size <= 64:
        return 1.2
    return 1.0


# --------------------------------------------------------------------------- #
# PNG
# --------------------------------------------------------------------------- #

def read_png(path):
    """Decode `path` to (width, height, rows-of-RGBA-bytes).

    Handles the subset rsvg-convert emits: 8-bit non-interlaced RGB or RGBA.
    """
    with open(path, "rb") as fh:
        data = fh.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG")

    pos = 8
    width = height = channels = None
    idat = bytearray()
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length  # length + type + body + crc

        if kind == b"IHDR":
            width, height, depth, colour, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if depth != 8 or interlace != 0 or colour not in (2, 6):
                raise SystemExit(
                    f"{path}: need an 8-bit non-interlaced RGB/RGBA PNG "
                    f"(got depth={depth} colour={colour} interlace={interlace})"
                )
            channels = 3 if colour == 2 else 4
        elif kind == b"IDAT":
            idat += body
        elif kind == b"IEND":
            break

    raw = zlib.decompress(bytes(idat))
    stride = width * channels
    rows = []
    prev = bytearray(stride)
    pos = 0
    for _ in range(height):
        filt = raw[pos]
        line = bytearray(raw[pos + 1:pos + 1 + stride])
        pos += 1 + stride
        _unfilter(filt, line, prev, channels)
        rows.append(line)
        prev = line

    if channels == 3:  # promote to RGBA so callers see one layout
        rows = [_add_alpha(r) for r in rows]
    return width, height, rows


def _unfilter(filt, line, prev, bpp):
    """Undo one PNG scanline filter in place (spec 9.2)."""
    if filt == 0:
        return
    if filt == 1:
        for i in range(bpp, len(line)):
            line[i] = (line[i] + line[i - bpp]) & 0xFF
    elif filt == 2:
        for i in range(len(line)):
            line[i] = (line[i] + prev[i]) & 0xFF
    elif filt == 3:
        for i in range(len(line)):
            left = line[i - bpp] if i >= bpp else 0
            line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
    elif filt == 4:
        for i in range(len(line)):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
            line[i] = (line[i] + pred) & 0xFF
    else:
        raise SystemExit(f"unknown PNG filter {filt}")


def _add_alpha(row):
    out = bytearray(len(row) // 3 * 4)
    for i in range(len(row) // 3):
        out[4 * i:4 * i + 3] = row[3 * i:3 * i + 3]
        out[4 * i + 3] = 255
    return out


def write_png(path, width, height, pixels):
    """Encode RGBA `pixels` (a flat bytearray) as a PNG."""
    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)  # filter: none — zlib does the work, and icons are tiny
        raw += pixels[y * stride:(y + 1) * stride]

    def chunk(kind, body):
        return (struct.pack(">I", len(body)) + kind + body
                + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF))

    with open(path, "wb") as fh:
        fh.write(b"\x89PNG\r\n\x1a\n")
        fh.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)))
        fh.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        fh.write(chunk(b"IEND", b""))


# --------------------------------------------------------------------------- #
# compositing
# --------------------------------------------------------------------------- #

def alpha_mask(width, height, rows):
    """Coverage in 0…1, one float per source pixel, row-major."""
    mask = [0.0] * (width * height)
    for y in range(height):
        row = rows[y]
        base = y * width
        for x in range(width):
            mask[base + x] = row[4 * x + 3] / 255.0
    return mask


def box_downscale(mask, sw, sh, dw, dh):
    """Area-average `mask` to dw×dh.

    Every destination pixel integrates the full rectangle of source pixels behind
    it, fractional edges included. Sampling instead would make thin strokes
    flicker in and out between sizes.
    """
    out = [0.0] * (dw * dh)
    fx, fy = sw / dw, sh / dh
    for dy in range(dh):
        y0, y1 = dy * fy, (dy + 1) * fy
        iy0, iy1 = int(y0), min(int(y1 - 1e-9) + 1, sh)
        for dx in range(dw):
            x0, x1 = dx * fx, (dx + 1) * fx
            ix0, ix1 = int(x0), min(int(x1 - 1e-9) + 1, sw)
            total = weight = 0.0
            for sy in range(iy0, iy1):
                wy = min(sy + 1, y1) - max(sy, y0)
                if wy <= 0:
                    continue
                base = sy * sw
                for sx in range(ix0, ix1):
                    wx = min(sx + 1, x1) - max(sx, x0)
                    if wx <= 0:
                        continue
                    a = wx * wy
                    total += mask[base + sx] * a
                    weight += a
            out[dy * dw + dx] = total / weight if weight else 0.0
    return out


def tile_coverage(size):
    """Superellipse coverage in 0…1 for a `size`×`size` canvas.

    Supersampled because the tile's shoulder is the one edge a viewer looks
    straight at; a hard test would show stair-steps at every icon size.
    """
    cov = [0.0] * (size * size)
    half = size * EXTENT / 2.0
    centre = size / 2.0
    step = 1.0 / SS
    per = 1.0 / (SS * SS)
    for y in range(size):
        for x in range(size):
            hits = 0.0
            for sy in range(SS):
                py = y + (sy + 0.5) * step - centre
                for sx in range(SS):
                    px = x + (sx + 0.5) * step - centre
                    if (abs(px) / half) ** EXP + (abs(py) / half) ** EXP <= 1.0:
                        hits += 1.0
            cov[y * size + x] = hits * per
    return cov


def render(size, mask, mw, mh):
    """Composite one icon: glyph over tile over transparency."""
    tile = tile_coverage(size)

    # Fit the drawing into the tile's inner box, preserving aspect.
    span = size * EXTENT * glyph_fill(size)
    scale = min(span / mw, span / mh)
    gw, gh = max(1, round(mw * scale)), max(1, round(mh * scale))
    glyph = box_downscale(mask, mw, mh, gw, gh)

    gain = stroke_gain(size)
    ox, oy = (size - gw) // 2, (size - gh) // 2

    px = bytearray(size * size * 4)
    for y in range(size):
        # Ramp the tile top-to-bottom.
        t = y / max(1, size - 1)
        tr = round(TILE_TOP[0] + (TILE_BOTTOM[0] - TILE_TOP[0]) * t)
        tg = round(TILE_TOP[1] + (TILE_BOTTOM[1] - TILE_TOP[1]) * t)
        tb = round(TILE_TOP[2] + (TILE_BOTTOM[2] - TILE_TOP[2]) * t)
        for x in range(size):
            i = y * size + x
            ta = tile[i]
            if ta <= 0.0:
                continue

            r, g, b = tr, tg, tb
            gx, gy = x - ox, y - oy
            if 0 <= gx < gw and 0 <= gy < gh:
                ink = min(1.0, glyph[gy * gw + gx] * gain)
                if ink > 0.0:
                    r = round(r + (INK[0] - r) * ink)
                    g = round(g + (INK[1] - g) * ink)
                    b = round(b + (INK[2] - b) * ink)

            o = i * 4
            px[o] = r
            px[o + 1] = g
            px[o + 2] = b
            px[o + 3] = round(ta * 255)
    return px


# --------------------------------------------------------------------------- #

def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("out", nargs="?", default=os.path.join(PACKAGING, "BoaVoice.icns"))
    args = ap.parse_args()

    if not os.path.exists(SOURCE):
        raise SystemExit(
            f"{SOURCE} missing — run scripts/raster-svg.sh first (needs librsvg)"
        )

    mw, mh, rows = read_png(SOURCE)
    mask = alpha_mask(mw, mh, rows)
    if not any(a > 0.0 for a in mask):
        raise SystemExit(
            f"{SOURCE} is fully opaque — it must be rasterised on a transparent "
            "background, since the alpha channel is the coverage mask"
        )

    iconset = os.path.join(PACKAGING, "BoaVoice.iconset")
    os.makedirs(iconset, exist_ok=True)

    for size, scale in VARIANTS:
        px = size * scale
        suffix = "" if scale == 1 else f"@{scale}x"
        name = f"icon_{size}x{size}{suffix}.png"
        write_png(os.path.join(iconset, name), px, px, render(px, mask, mw, mh))
        print(f"  {name} ({px}×{px})")

    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", args.out], check=True)
    print(f"→ {args.out}")

    # The app also needs a plain PNG for the runtime window icon.
    runtime = os.path.join(PACKAGING, "icon-512.png")
    write_png(runtime, 512, 512, render(512, mask, mw, mh))
    print(f"→ {runtime}")


if __name__ == "__main__":
    sys.exit(main())
