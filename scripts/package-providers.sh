#!/bin/sh
# Builds every provider under providers/ for wasm32-wasip2 and packages them
# into dist/ with the providers.json catalogue Violet installs from.
#
#   scripts/package-providers.sh            # into ./dist
#   scripts/package-providers.sh out/dir
#
# Publish the folder's files as release assets (Violet reads
# releases/latest/download/providers.json by default), or point Violet at
# it while developing:  VIOLET_PROVIDER_INDEX=$PWD/dist/providers.json
set -eu

out=${1:-dist}
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

wasm=""
for manifest in providers/*/Cargo.toml; do
    dir=$(dirname "$manifest")
    echo "building $dir" >&2
    cargo build --release --locked --target wasm32-wasip2 --manifest-path "$manifest"
    for file in "$dir"/target/wasm32-wasip2/release/*.wasm; do
        wasm="$wasm $file"
    done
done

# The packaging tool only inspects components; a debug build (dependencies
# are optimised anyway) saves the release profile's long link.
# shellcheck disable=SC2086
cargo run --quiet --locked -p violet-provider-cli -- package --out "$out" $wasm
