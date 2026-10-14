#!/usr/bin/env bash
# Builds Violet.app for Apple silicon and Intel in one (universal) binary
# and packs it into a disk image:
#
#   scripts/package-macos.sh <version> <out-dir>
#
# writes <out-dir>/violet-<version>-macos-universal.dmg and its .sha256.
# The app is signed ad hoc, not with a Developer ID, so the first launch
# needs right click > Open (or `xattr -dr com.apple.quarantine`).
set -euo pipefail

version=${1:?version}
out=${2:?output directory}
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}

cd "$root"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked --bin violet --target "$target"
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app="$work/Violet/Violet.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/violet" \
    target/aarch64-apple-darwin/release/violet \
    target/x86_64-apple-darwin/release/violet
cp assets/app/violet.icns "$app/Contents/Resources/violet.icns"

# The bundle version is numbers only: v0.1.0 -> 0.1.0, dev builds -> 0.0.0.
bundle_version=$(echo "$version" | sed -n 's/^v\{0,1\}\([0-9][0-9.]*\).*/\1/p')
bundle_version=${bundle_version:-0.0.0}
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Violet</string>
    <key>CFBundleDisplayName</key><string>Violet</string>
    <key>CFBundleIdentifier</key><string>dev.violet.Violet</string>
    <key>CFBundleExecutable</key><string>violet</string>
    <key>CFBundleIconFile</key><string>violet</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$bundle_version</string>
    <key>CFBundleVersion</key><string>$bundle_version</string>
    <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict "$app"

cp README.md "$work/Violet/"
ln -s /Applications "$work/Violet/Applications"
name="violet-$version-macos-universal.dmg"
rm -f "$out/$name"
hdiutil create -quiet -volname Violet -srcfolder "$work/Violet" -format UDZO -ov "$out/$name"
(cd "$out" && shasum -a 256 "$name" > "$name.sha256")
echo "$out/$name"
