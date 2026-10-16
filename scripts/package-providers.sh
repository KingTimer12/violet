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
#
# Providers with C inside (SQLite) need the WASI SDK's clang: point
# WASI_SDK_PATH at it (https://github.com/WebAssembly/wasi-sdk/releases).
set -eu

out=${1:-dist}
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ -n "${WASI_SDK_PATH:-}" ]; then
    exe=""
    case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) exe=".exe" ;; esac
    export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang$exe"
    export AR_wasm32_wasip2="$WASI_SDK_PATH/bin/llvm-ar$exe"
    export CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK_PATH/share/wasi-sysroot"
elif [ -d providers/sqlite ]; then
    echo "WASI_SDK_PATH is not set: the SQLite provider needs the WASI SDK" >&2
    exit 1
fi

wasm=""
for manifest in providers/*/Cargo.toml; do
    dir=$(dirname "$manifest")
    echo "building $dir" >&2
    # From the provider's folder, so its .cargo/config.toml applies.
    (cd "$dir" && cargo build --release --locked --target wasm32-wasip2)
    for file in "$dir"/target/wasm32-wasip2/release/*.wasm; do
        wasm="$wasm $file"
    done
done

# The packaging tool only inspects components; a debug build (dependencies
# are optimised anyway) saves the release profile's long link.
# shellcheck disable=SC2086
cargo run --quiet --locked -p violet-provider-cli -- package --out "$out" $wasm
