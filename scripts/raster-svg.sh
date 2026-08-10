#!/bin/sh
# Rasterise packaging/boa-source.svg into the two masks make-icon.py consumes.
#
# Two, not one, because the drawing has two layers: the head is filled black, and the inner cross and
# the eyes sit on top of it in white. A single mask cannot express that — its alpha says only "the
# drawing is here", and the icon comes out as a featureless silhouette.
#
# So this renders the same SVG twice, hiding one layer each time, and make-icon.py composites the
# results in order. Same viewBox both times, so the two line up exactly.
#
# A separate, manual step: this needs librsvg (`brew install librsvg`, or `apt install librsvg2-bin`),
# while make-icon.py must stay dependency-free so the icon rebuilds from a plain checkout with nothing
# but a Python interpreter. The PNGs are committed, so run this only after editing the SVG.
#
# No `-b` flag: the background stays transparent on purpose. The alpha channel *is* the coverage mask,
# and any background baked in here would be read as ink.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
svg="$root/packaging/boa-source.svg"
width=${1:-1200}

command -v rsvg-convert >/dev/null || {
    echo "rsvg-convert not found — brew install librsvg" >&2
    exit 1
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Which path ids are which layer. Named here rather than inferred from the fill colour, because the
# colour in the SVG is documentation and this is the actual structure.
ink_ids="Path Path_Copy"
highlight_ids="Path-1 Path_Copy-1 Path-2 Path-3"

# `display:none` on the ids not wanted, injected as a stylesheet. rsvg-convert has no way to select
# layers, and editing the paths out with sed would depend on their order in the file.
hide() {
    out=$1
    shift
    css=""
    for id in "$@"; do
        css="$css#$id{display:none}"
    done
    python3 - "$svg" "$out" "$css" <<'PY'
import sys
svg, out, css = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(svg, encoding="utf-8").read()
# After the opening <svg …> tag, so it applies to everything inside it.
at = text.index(">", text.index("<svg")) + 1
open(out, "w", encoding="utf-8").write(
    text[:at] + f"<style>{css}</style>" + text[at:]
)
PY
}

hide "$work/ink.svg" $highlight_ids
hide "$work/highlight.svg" $ink_ids

rsvg-convert -w "$width" "$work/ink.svg" -o "$root/packaging/boa-ink.png"
rsvg-convert -w "$width" "$work/highlight.svg" -o "$root/packaging/boa-highlight.png"
echo "→ packaging/boa-ink.png and packaging/boa-highlight.png (${width}px wide)"
