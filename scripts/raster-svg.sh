#!/bin/sh
# Rasterise packaging/boa-source.svg to the PNG that make-icon.py consumes.
#
# A separate, manual step: this needs librsvg (`brew install librsvg`, or
# `apt install librsvg2-bin`), while make-icon.py must stay dependency-free so the
# icon rebuilds from a plain checkout with nothing but a Python interpreter. The
# PNG is committed, so run this only after editing the SVG.
#
# No `-b` flag: the background stays transparent on purpose. make-icon.py reads
# the alpha channel as its coverage mask and paints the green tile itself, so any
# background baked in here would be read as ink and the icon would come out as a
# white rectangle.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
svg="$root/packaging/boa-source.svg"
png="$root/packaging/boa-source.png"
width=${1:-1200}

command -v rsvg-convert >/dev/null || {
    echo "rsvg-convert not found — brew install librsvg" >&2
    exit 1
}

rsvg-convert -w "$width" "$svg" -o "$png"
echo "→ $png"
