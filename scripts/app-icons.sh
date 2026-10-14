#!/usr/bin/env bash
# Renders assets/app/violet.svg into the icons the packages carry:
# violet.icns (macOS) and violet.png (Linux). Needs resvg
# (`cargo install resvg`) and, for the .icns, macOS's iconutil.
set -euo pipefail
cd "$(dirname "$0")/../assets/app"
resvg -w 512 -h 512 violet.svg violet.png
set=$(mktemp -d)/violet.iconset
mkdir -p "$set"
for size in 16 32 128 256 512; do
    resvg -w "$size" -h "$size" violet.svg "$set/icon_${size}x${size}.png"
    resvg -w $((size * 2)) -h $((size * 2)) violet.svg "$set/icon_${size}x${size}@2x.png"
done
iconutil -c icns -o violet.icns "$set"
rm -rf "$(dirname "$set")"
