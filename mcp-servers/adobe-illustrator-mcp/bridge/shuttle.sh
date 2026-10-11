#!/usr/bin/env bash
# illustrator-mcp bridge shuttle (macOS) — zero-install bridge vehicle.
#
# Illustrator 2023+ removed the ExtendScript `Socket` class, so a pure
# in-app script can no longer speak HTTP (the CEP panel still can — its XHR
# runs in Chromium — but needs an install and an app restart). This shuttle
# is the sanctioned AppleScript alternative: it claims queued commands from
# the illustrator-mcp workload over HTTP, executes each inside Illustrator's
# persistent ExtendScript engine with `do javascript`, and posts the result
# back. Run it in a terminal for as long as you want the bridge live;
# Ctrl-C stops it. No daemon, nothing installed.
#
#   curl -fsS -H 'Host: illustrator-mcp.localhost' \
#     http://127.0.0.1:8200/bridge/shuttle.sh -o shuttle.sh && bash shuttle.sh
#
# Env: MCP_HOST, MCP_INGRESS, SHUTTLE_RUN_SECONDS (0 = until Ctrl-C).
set -u

HOST_HEADER="${MCP_HOST:-illustrator-mcp.localhost}"
INGRESS="${MCP_INGRESS:-http://127.0.0.1:8200}"
RUN_SECONDS="${SHUTTLE_RUN_SECONDS:-0}"
POLL_SECONDS="${SHUTTLE_POLL_SECONDS:-1}"
CLIENT="$(date +%s)$(printf '%03d' $((RANDOM % 1000)))"

LIB="$(mktemp -t ilst-mcp-commands).jsx"
trap 'rm -f "$LIB"' EXIT

echo "shuttle: fetching command library from $HOST_HEADER..."
curl -fsS -H "Host: $HOST_HEADER" "$INGRESS/bridge/commands.jsx" -o "$LIB" || {
    echo "shuttle: cannot reach the illustrator-mcp workload at $INGRESS" >&2
    exit 1
}

# Runs one command (already URI-encoded JSON) inside Illustrator and prints
# the result JSON. Loads the command library into the engine if this engine
# has not seen it yet (the main ExtendScript engine persists across calls).
run_in_illustrator() {
    local encoded="$1"
    osascript <<OSA
with timeout of 600 seconds
  tell application "Adobe Illustrator"
    do javascript "if (typeof ilstMcpExecuteWire === 'undefined') { \$.evalFile('$LIB'); } ilstMcpExecuteWire('$encoded')"
  end tell
end timeout
OSA
}

urlencode() {
    python3 -c 'import sys, urllib.parse; sys.stdout.write(urllib.parse.quote(sys.stdin.read(), safe=""))'
}

json_get() { # json_get <json> <key>
    printf '%s' "$1" | python3 -c "import sys, json
try:
    d = json.load(sys.stdin)
    v = d.get('$2')
    print('' if v is None else v)
except Exception:
    print('')"
}

echo "shuttle: bridging $HOST_HEADER <-> Adobe Illustrator (client $CLIENT). Ctrl-C to stop."
START=$(date +%s)
SERVED=0
while :; do
    if [ "$RUN_SECONDS" -gt 0 ] && [ $(( $(date +%s) - START )) -ge "$RUN_SECONDS" ]; then
        echo "shuttle: run window elapsed after $SERVED command(s); exiting."
        break
    fi
    CMD=$(curl -fsS -m 10 -H "Host: $HOST_HEADER" \
        "$INGRESS/bridge/command?v=2&client=$CLIENT" 2>/dev/null) || {
        echo "shuttle: server unreachable; retrying..."
        sleep 2
        continue
    }
    NAME=$(json_get "$CMD" command)
    if [ -z "$NAME" ]; then
        sleep "$POLL_SECONDS"
        continue
    fi
    ID=$(json_get "$CMD" id)
    echo "shuttle: executing $NAME (id $ID)"
    ENCODED=$(printf '%s' "$CMD" | urlencode)
    RESULT=$(run_in_illustrator "$ENCODED") || RESULT='{"status":"error","message":"osascript execution failed"}'
    for _ in 1 2 3; do
        if curl -fsS -m 10 -o /dev/null -X POST -H "Host: $HOST_HEADER" \
            -H 'Content-Type: application/json' \
            --data-binary "$RESULT" "$INGRESS/bridge/result?id=$ID"; then
            break
        fi
        echo "shuttle: result post failed; retrying"
        sleep 1
    done
    SERVED=$((SERVED + 1))
done
