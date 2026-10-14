#!/usr/bin/env bash
# Compares Violet with DBeaver on this Mac: size on disk, time until the
# main window shows, and memory and CPU once idle. macOS only.
#
#   scripts/bench-dbeaver.sh            # 3 runs each
#   RUNS=5 scripts/bench-dbeaver.sh
#
# Violet runs from target/release (built if missing) with an empty
# VIOLET_HOME, so your own connections are not touched. DBeaver runs as
# installed, with your workspace; it is quit, not killed, after each run.
set -euo pipefail

RUNS=${RUNS:-3}
IDLE_SECONDS=${IDLE_SECONDS:-15}
DBEAVER_APP=${DBEAVER_APP:-/Applications/DBeaver.app}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VIOLET_BIN=${VIOLET_BIN:-$ROOT/target/release/violet}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[[ $(uname) == Darwin ]] || { echo "macOS only" >&2; exit 1; }
[[ -d $DBEAVER_APP ]] || { echo "DBeaver not found at $DBEAVER_APP" >&2; exit 1; }
if [[ ! -x $VIOLET_BIN ]]; then
    (cd "$ROOT" && cargo build --release --locked --bin violet)
fi

# Waits for a window of at least 800x500 points owned by the process: the
# main window, not a splash screen. Prints the time it showed, in ms.
cat > "$WORK/window.js" <<'EOF'
ObjC.import('CoreGraphics');
function run(argv) {
    const pid = parseInt(argv[0]);
    const deadline = Date.now() + 120000;
    while (Date.now() < deadline) {
        const windows = ObjC.castRefToObject(
            $.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0));
        for (let i = 0; i < windows.count; i++) {
            const w = windows.objectAtIndex(i);
            if (w.objectForKey('kCGWindowOwnerPID').intValue !== pid) continue;
            if (w.objectForKey('kCGWindowLayer').intValue !== 0) continue;
            const b = w.objectForKey('kCGWindowBounds');
            if (b.objectForKey('Width').doubleValue >= 800 &&
                b.objectForKey('Height').doubleValue >= 500) {
                return String(Date.now());
            }
        }
        delay(0.02);
    }
    return 'timeout';
}
EOF

now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }

footprint_mb() {
    footprint "$1" 2>/dev/null | sed -n 's/.*Footprint: \([0-9.]*\) \([KMG]B\).*/\1 \2/p' |
        awk '{ v = $1; if ($2 == "KB") v /= 1024; if ($2 == "GB") v *= 1024; printf "%d\n", v }'
}

# CPU seconds used so far, from ps's [[dd-]hh:]mm:ss.cc
cpu_seconds() {
    ps -o time= -p "$1" | awk '{
        n = split($1, p, ":"); s = 0
        for (i = 1; i <= n; i++) s = s * 60 + p[i]
        print s }'
}

median() { sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'; }

# run <name> <command...>: one measured launch; appends "ms mb cpu%" to $WORK/<name>
run() {
    local name=$1; shift
    local start pid shown
    start=$(now_ms)
    "$@" >/dev/null 2>&1 &
    pid=$!
    shown=$(osascript -l JavaScript "$WORK/window.js" "$pid")
    if [[ $shown == timeout ]]; then
        echo "$name: no window within 120 s" >&2
        kill "$pid" 2>/dev/null || true
        return 1
    fi
    sleep "$IDLE_SECONDS"
    local mb cpu0 cpu1
    mb=$(footprint_mb "$pid")
    cpu0=$(cpu_seconds "$pid"); sleep 10; cpu1=$(cpu_seconds "$pid")
    echo "$((shown - start)) $mb $(awk -v a="$cpu0" -v b="$cpu1" 'BEGIN { printf "%.1f", (b - a) * 10 }')" >> "$WORK/$name"
    "quit_$name" "$pid"
}

quit_violet() { kill "$1"; wait "$1" 2>/dev/null || true; }
quit_dbeaver() {
    osascript -e 'tell application id "org.jkiss.dbeaver.core.product" to quit' >/dev/null 2>&1 || kill "$1"
    while kill -0 "$1" 2>/dev/null; do sleep 0.2; done
}

for i in $(seq "$RUNS"); do
    echo "run $i/$RUNS" >&2
    rm -rf "${WORK:?}/home"
    run violet env VIOLET_HOME="$WORK/home" "$VIOLET_BIN"
    sleep 2
    run dbeaver "$DBEAVER_APP/Contents/MacOS/dbeaver"
    sleep 2
done

med() { awk -v c="$2" '{ print $c }' "$WORK/$1" | median; }
violet_size=$(du -sk "$VIOLET_BIN" | awk '{ printf "%.1f", $1 / 1024 }')
provider=$(find "$ROOT/providers/postgres/target/wasm32-wasip2/release" -maxdepth 1 -name '*.wasm' 2>/dev/null | head -1)
provider_size=$([[ -n $provider ]] && du -sk "$provider" | awk '{ printf " + %.1f MB PostgreSQL provider", $1 / 1024 }')
dbeaver_size=$(du -sk "$DBEAVER_APP" | awk '{ printf "%.0f", $1 / 1024 }')
dbeaver_version=$(defaults read "$DBEAVER_APP/Contents/Info.plist" CFBundleShortVersionString)
violet_version=$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)

cat <<EOF
| | Violet $violet_version | DBeaver $dbeaver_version |
| --- | --- | --- |
| Size on disk | ${violet_size} MB${provider_size} | ${dbeaver_size} MB |
| Launch to main window | $(med violet 1) ms | $(med dbeaver 1) ms |
| Memory when idle (footprint) | $(med violet 2) MB | $(med dbeaver 2) MB |
| CPU when idle | $(med violet 3) % | $(med dbeaver 3) % |

$(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.memsize | awk '{ print $1 / 1073741824 }') GB, macOS $(sw_vers -productVersion); median of $RUNS runs.
EOF
