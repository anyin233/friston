#!/usr/bin/env bash
# Regenerates crates/mm-app/assets/Friston.icns and the sidebar logo PNG from the SVG source.
#
# Why commit the .icns: bundling (dev-app.sh, package-app.sh) then needs no SVG rasteriser.
# Run this after editing friston-icon.svg. Needs rsvg-convert (`brew install librsvg`) and iconutil.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SVG="$ROOT/crates/mm-app/assets/friston-icon.svg"
OUT="$ROOT/crates/mm-app/assets/Friston.icns"
command -v rsvg-convert >/dev/null || { echo "error: rsvg-convert not found; brew install librsvg" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
set_dir="$work/Friston.iconset"
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
  rsvg-convert -w "$size" -h "$size" "$SVG" -o "$set_dir/icon_${size}x${size}.png"
  rsvg-convert -w $((size * 2)) -h $((size * 2)) "$SVG" -o "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set_dir" -o "$OUT"
# GPUI's img() leaves this SVG blank, so the sidebar embeds a PNG (2x of its 20pt slot, rounded up).
rsvg-convert -w 64 -h 64 "$SVG" -o "$ROOT/crates/mm-app/assets/friston-logo-64.png"
# README renders a high-DPI PNG reliably on GitHub without CSP SVG restrictions.
rsvg-convert -w 512 -h 512 "$SVG" -o "$ROOT/crates/mm-app/assets/friston-logo.png"
echo "$OUT"
