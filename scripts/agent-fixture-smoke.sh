#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLI="${GPUI_AGENT_CLI:-gpui-agent}"
RECIPE="$ROOT/docs/agent/rqbit-fixture-recipe.json"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/rqbit-agent-smoke.XXXXXX")"
APP_PID=""

cleanup() {
    if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
        "$CLI" --connect rqbit-desktop-fixture shutdown >/dev/null 2>&1 || true
        kill "$APP_PID" 2>/dev/null || true
        wait "$APP_PID" 2>/dev/null || true
    fi
    printf 'agent smoke artifacts: %s\n' "$WORK"
}
trap cleanup EXIT INT TERM

export GPUI_AGENT=1
export RQBIT_AGENT_FIXTURE=1
export GPUI_AGENT_ADDR="${GPUI_AGENT_ADDR:-127.0.0.1:17421}"
export GPUI_AGENT_TOKEN="${GPUI_AGENT_TOKEN:-dev-secret}"
export GPUI_AGENT_CONNECT_ID=rqbit-desktop-fixture
export GPUI_AGENT_RUNTIME_DIR="$WORK/runtime"
export GPUI_AGENT_SCREENSHOT_DIR="$WORK/screenshots"
mkdir -p "$GPUI_AGENT_RUNTIME_DIR" "$GPUI_AGENT_SCREENSHOT_DIR"

cargo build --manifest-path "$ROOT/Cargo.toml" -p desktop-gpui --features agent-testing
"$ROOT/target/debug/desktop-gpui" >"$WORK/app.log" 2>&1 &
APP_PID=$!

ready=false
for _ in $(seq 1 60); do
    if "$CLI" --connect rqbit-desktop-fixture wait >/dev/null 2>&1; then
        ready=true
        break
    fi
    if ! kill -0 "$APP_PID" 2>/dev/null; then
        printf 'fixture exited before becoming ready; see %s\n' "$WORK/app.log" >&2
        exit 1
    fi
    sleep 0.25
done
if [[ "$ready" != true ]]; then
    printf 'fixture did not become ready; see %s\n' "$WORK/app.log" >&2
    exit 1
fi

"$CLI" --connect last hello
"$CLI" recipe validate "$RECIPE"
"$CLI" recipe plan "$RECIPE" --order-check >/dev/null

if [[ "${AGENT_SCREENSHOTS:-0}" == 1 ]]; then
    "$CLI" --connect last recipe run "$RECIPE" \
        --receipt-out "$WORK/receipt.json" \
        --screenshot-dir "$WORK/steps" --screenshot-flagged
else
    "$CLI" --connect last recipe run "$RECIPE" \
        --receipt-out "$WORK/receipt.json"
fi

python3 -I -c '
import json, sys
receipt = json.load(open(sys.argv[1]))
assert receipt["ok"] is True, receipt
assert receipt["session_reused"] is True, receipt
print("recipe ok: {} steps, one reused session".format(len(receipt["steps"])))
' "$WORK/receipt.json"

"$CLI" --connect last invoke fixture.close_window
"$CLI" --connect last hello | python3 -I -c '
import json, sys
response = json.load(sys.stdin)
assert response["ok"] is True, response
assert response["hello"]["ready"] is False, response
print("close-to-tray: ready=false")
'
if "$CLI" --connect last click theme >/dev/null 2>&1; then
    printf 'window action unexpectedly succeeded while closed to tray\n' >&2
    exit 1
fi
"$CLI" --connect last keybinding --id app.open --scope global
"$CLI" --connect last wait
"$CLI" --connect last assert --id torrent-2 --exists --visible

if "$CLI" --connect last keybinding --id app.quit --scope global >/dev/null 2>&1; then
    printf 'app.quit unexpectedly succeeded without confirm=true\n' >&2
    exit 1
fi
"$CLI" --connect last keybinding --id app.quit --scope global --confirm
wait "$APP_PID"
APP_PID=""
