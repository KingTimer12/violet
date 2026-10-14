#!/usr/bin/env bash
# Builds Violet for this Linux machine's architecture and packs it:
#
#   scripts/package-linux.sh <version> <out-dir>
#
# writes <out-dir>/violet-<version>-linux-<arch>.tar.gz and its .sha256.
# The archive holds the binary, a desktop entry and its icon, and
# install.sh, which puts them under ~/.local (or a prefix it is given).
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
name="violet-$version-linux-$arch"
dir="$work/$name"
mkdir -p "$dir"
cp target/release/violet "$dir/"
cp assets/app/dev.violet.Violet.desktop assets/app/violet.png README.md "$dir/"
cat > "$dir/install.sh" <<'INSTALL'
#!/bin/sh
# Installs Violet for this user (or under the prefix given, e.g. /usr/local).
set -eu
prefix=${1:-$HOME/.local}
here=$(cd "$(dirname "$0")" && pwd)
install -Dm755 "$here/violet" "$prefix/bin/violet"
install -Dm644 "$here/dev.violet.Violet.desktop" "$prefix/share/applications/dev.violet.Violet.desktop"
install -Dm644 "$here/violet.png" "$prefix/share/icons/hicolor/512x512/apps/violet.png"
echo "Installed to $prefix; make sure $prefix/bin is on your PATH."
INSTALL
chmod +x "$dir/install.sh"
tar -C "$work" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
