#!/usr/bin/env bash
# Builds Violet for this Linux machine's architecture and packs it:
#
#   scripts/package-linux.sh <version> <out-dir>
#
# writes <out-dir>/violet-<version>-<arch>-unknown-linux-gnu.tar.gz and its
# .sha256, named as the updater (fastframe-update) looks for it. The archive
# holds the binary, the marker that lets it update itself, a desktop entry
# and its icon, and install.sh, which puts them under ~/.local (or a prefix
# it is given).
set -euo pipefail

version=${1:?version}
out=${2:?output directory}
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
arch=$(uname -m)

cd "$root"
cargo build --release --locked --bin violet

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
name="violet-$version-$arch-unknown-linux-gnu"
dir="$work/$name"
mkdir -p "$dir"
cp target/release/violet "$dir/"
# A copy from this archive replaces itself with newer releases.
printf 'violet-portable-v1\n' > "$dir/violet-portable.txt"
cp assets/app/dev.violet.Violet.desktop assets/app/violet.png README.md "$dir/"
cat > "$dir/install.sh" <<'INSTALL'
#!/bin/sh
# Installs Violet for this user (or under the prefix given, e.g. /usr/local).
set -eu
prefix=${1:-$HOME/.local}
here=$(cd "$(dirname "$0")" && pwd)
install -Dm755 "$here/violet" "$prefix/bin/violet"
# Beside the binary: Violet updates itself where it finds it.
install -Dm644 "$here/violet-portable.txt" "$prefix/bin/violet-portable.txt"
install -Dm644 "$here/dev.violet.Violet.desktop" "$prefix/share/applications/dev.violet.Violet.desktop"
install -Dm644 "$here/violet.png" "$prefix/share/icons/hicolor/512x512/apps/violet.png"
echo "Installed to $prefix; make sure $prefix/bin is on your PATH."
INSTALL
chmod +x "$dir/install.sh"
tar -C "$work" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
