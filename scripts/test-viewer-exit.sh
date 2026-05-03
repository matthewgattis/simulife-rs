#!/usr/bin/env bash
# Regression test: viewer must exit cleanly (no segfault / signal death).
# Reproduces the Linux-specific crash where wgpu Surface teardown races with
# the X11/Wayland display disconnect on quit.
#
# Usage: scripts/test-viewer-exit.sh [--runs N]
#   Requires a built release binary (cargo build --release).

set -euo pipefail

RUNS=3
while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        *) echo "unknown arg: $1"; exit 1 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SERVER="$REPO_ROOT/target/release/server"
VIEWER="$REPO_ROOT/target/release/viewer"

for bin in "$SERVER" "$VIEWER"; do
    if [[ ! -x "$bin" ]]; then
        echo "FAIL: $bin not found. Run: cargo build --release" >&2
        exit 1
    fi
done

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Start server
"$SERVER" &>/dev/null &
SERVER_PID=$!
sleep 2

# Verify server is alive
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL: server did not start" >&2
    exit 1
fi

FAILURES=0
for i in $(seq 1 "$RUNS"); do
    "$VIEWER" --profile-duration-secs 2 &>/dev/null
    code=$?
    if [[ $code -ne 0 ]]; then
        # Signals show up as 128+signum (e.g. 139 = SIGSEGV)
        echo "FAIL: run $i exited with code $code" >&2
        FAILURES=$((FAILURES + 1))
    fi
done

if [[ $FAILURES -ne 0 ]]; then
    echo "FAIL: $FAILURES/$RUNS runs crashed" >&2
    exit 1
fi

echo "PASS: $RUNS/$RUNS viewer exits were clean"
