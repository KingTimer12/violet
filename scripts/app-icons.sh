#!/usr/bin/env bash
# Renders assets/app/violet.svg into the icons the packages carry:
# violet.icns (macOS), violet.png (Linux, and the window's icon) and
# violet.ico (the Windows executable, through build.rs). Needs resvg
# (`cargo install resvg`), ImageMagick for the .ico and, for the .icns,
# macOS's iconutil.
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

# Windows icons fill their square: the rounded square without the macOS
# grid's margin, at the sizes Explorer and the taskbar ask for.
ico=$(mktemp -d)
sed 's/viewBox="0 0 1024 1024"/viewBox="88 88 848 848"/' violet.svg > "$ico/violet.svg"
frames=()
for size in 16 20 24 32 40 48 64 128 256; do
    resvg -w "$size" -h "$size" "$ico/violet.svg" "$ico/$size.png"
    frames+=("$ico/$size.png")
done
magick "${frames[@]}" violet.ico
rm -rf "$ico"
