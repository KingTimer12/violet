#!/bin/sh
# Sets the workspace version and refreshes every Cargo.lock that records it:
# the root one and each provider's. Providers are outside the workspace and
# depend on violet-sdk by path, so a stale lock fails `cargo build --locked`
# in the release build.
#
#   scripts/bump-version.sh 0.1.10
#
# Then commit the changes and tag the release (git tag v0.1.10).
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <version>   (e.g. 0.1.10)" >&2
    exit 1
fi
version=$1
case "$version" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "not a version: $version" >&2; exit 1 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# The first `version =` is [workspace.package]'s.
awk -v v="$version" '!done && /^version = "/ { print "version = \"" v "\""; done = 1; next } { print }'     Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml

cargo update --workspace
for manifest in providers/*/Cargo.toml; do
    (cd "$(dirname "$manifest")" && cargo update --workspace)
done

# Fail here, not in CI.
cargo metadata --locked --format-version 1 >/dev/null
for manifest in providers/*/Cargo.toml; do
    (cd "$(dirname "$manifest")" && cargo metadata --locked --format-version 1 >/dev/null)
done

echo "bumped to $version" >&2
