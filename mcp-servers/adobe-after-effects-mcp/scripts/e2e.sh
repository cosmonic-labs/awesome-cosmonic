#!/usr/bin/env bash
# End-to-end tests for after-effects-mcp. See scripts/mcp_e2e_lib.sh for the
# shared framework harness (protocol, spec enforcement, robustness, discovery,
# skills, guard) — the same file the illustrator-mcp suite uses.
#
# The tools need a wasi:keyvalue host, which `wasmtime serve` does not provide,
# so the harness composes testing/kv-stub into the component with `wac plug`
# and drives the bridge endpoints itself — curl plays the part of the After
# Effects panel. No copy of After Effects is involved.
set -u

cd "$(dirname "$0")/.."

PORT=8187
GUARD_PORT=8186
WASM=target/wasm32-wasip2/release/after_effects_mcp.wasm
PLUGGED=target/wasm32-wasip2/release/after_effects_mcp_plugged.wasm
STUB=testing/kv-stub/target/wasm32-wasip2/release/kv_stub.wasm

# shellcheck source=mcp_e2e_lib.sh
source "$(dirname "$0")/mcp_e2e_lib.sh"

# Used by the framework's concurrency test. `get-help` is the only tool that
# answers without the panel, which is what makes it usable here.
FIRST_TOOL_NAME=get-help
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='After Effects MCP bridge'

mcp_build_if_needed "${1:-}"

# Satisfy the wasi:keyvalue import with the test stub, then serve the composed
# component instead of the raw one.
if [ "${1:-}" != "--no-build" ] || [ ! -f "$STUB" ]; then
  echo "building keyvalue stub..."
  (cd testing/kv-stub && cargo build --release --target wasm32-wasip2) || exit 1
fi
[ -f "$STUB" ] || { echo "missing $STUB"; exit 1; }
echo "composing keyvalue stub into the component..."
wac plug "$WASM" --plug "$STUB" -o "$PLUGGED" || exit 1
if wasm-tools component wit "$PLUGGED" 2>/dev/null | grep -q 'import wasi:keyvalue'; then
  fail "keyvalue import satisfied by the stub" "import still present"
else
  pass "keyvalue import satisfied by the stub"
fi
WASM="$PLUGGED"

# A previous run's server can outlive its cleanup (its kv dir gone, every
# instantiation failing) and squat the port, poisoning this run from the
# first request. These ports belong to this suite; clear them.
for p in "$PORT" "$GUARD_PORT"; do
  STALE=$(lsof -tiTCP:"$p" -sTCP:LISTEN 2>/dev/null || true)
  [ -n "$STALE" ] && { echo "killing stale listener on :$p ($STALE)"; kill $STALE 2>/dev/null; }
done

KV_DIR=$(mktemp -d)
trap 'rm -rf "$KV_DIR" "${KV_DIR}-guard"' EXIT

mkdir -p "${KV_DIR}-guard"
mcp_harness_start --dir "${KV_DIR}::/kv"
mcp_harness_start_guard --dir "${KV_DIR}-guard::/kv"

BASE="http://127.0.0.1:${PORT}"

# --- panel simulation -------------------------------------------------------

# Identifies the simulated panel. The server serves only the highest client id
# it has seen, so any test that registers a newer panel must raise this.
PANEL_CLIENT=1000

# panel_poll [client] — one panel poll, as an up-to-date panel would make it.
panel_poll() { curl -s "${BASE}/bridge/command?v=2&client=${1:-$PANEL_CLIENT}"; }

# panel_answer <id> <result-json> — post a result for a dispatched command.
panel_answer() {
  curl -s -o /dev/null -X POST "${BASE}/bridge/result?id=$1" \
    -H 'Content-Type: application/json' -d "$2"
}

# json_field <json> <python-expression over `d`>
json_field() { printf '%s' "$1" | python3 -c "import sys,json;d=json.load(sys.stdin);print($2)"; }

# panel_serve <result-json> — claim the next command and answer it. Echoes the
# command the panel received so callers can assert on the arguments.
panel_serve() {
  local cmd id
  for _ in $(seq 1 40); do
    cmd=$(panel_poll)
    case "$cmd" in *'"command":null'*|'') sleep 0.2 ;; *) break ;; esac
  done
  id=$(json_field "$cmd" "d['id']")
  panel_answer "$id" "$1"
  printf '%s' "$cmd"
}

framework_tests get-help bridge-status get-results run-script get-project-info \
  list-compositions get-layer-info create-composition set-composition-properties \
  delete-composition create-text-layer create-shape-layer create-solid-layer \
  add-image-layer set-layer-properties set-layer-keyframe set-layer-expression \
  apply-effect apply-effect-template save-frame-png save-project run-batch

# The default route and Skills over MCP, both from the upstream template.
discovery_tests after-effects-mcp
skills_tests after-effects-mcp references/TOOLS.md references/HANDOFF.md

echo "== get-help (answers with no panel) =="
OUT=$(mcp_call get-help '{}')
assert_contains "get-help returns the setup guide" 'After Effects MCP bridge' "$OUT"
assert_contains "get-help lists the effect templates" 'cinematic-look' "$OUT"
assert_contains "get-help states the colour convention" '0..1' "$OUT"
assert_contains "get-help points at the handoff skill" 'HANDOFF.md' "$OUT"

echo "== argument validation =="
# Bad arguments are rejected in the SDK's deserialization step, before any tool
# body runs — so nothing is queued for the panel. rmcp reports that as a tool
# error carrying the reason, not as a JSON-RPC -32602.
OUT=$(mcp_call create-shape-layer '{"shapeType":"trapezoid"}')
assert_contains "unknown shapeType rejected" '"isError":true' "$OUT"
assert_contains "rejection names the allowed shapes" 'rectangle' "$OUT"
OUT=$(mcp_call run-script '{"script":"rmRfSlash"}')
assert_contains "script outside the allow-list rejected" '"isError":true' "$OUT"
assert_contains "rejected script is not queued" 'unknown variant' "$OUT"
OUT=$(mcp_call apply-effect-template '{"layerIndex":1,"templateName":"make-it-pop"}')
assert_contains "unknown effect template rejected" '"isError":true' "$OUT"
OUT=$(mcp_call create-composition '{}')
assert_contains "missing required argument rejected" '"isError":true' "$OUT"
assert_contains "rejection names the missing field" 'name' "$OUT"

echo "== bridge: no panel connected =="
OUT=$(mcp_call bridge-status '{}')
assert_contains "bridge-status reports no panel" '"panelConnected":false' "$OUT"
assert_contains "bridge-status says how to connect" 'install-bridge.sh' "$OUT"
OUT=$(mcp_call get-results '{}')
assert_contains "get-results with nothing queued" 'no-results' "$OUT"
OUT=$(mcp_call list-compositions '{}')
assert_contains "unserviced command is a tool error" '"isError":true' "$OUT"
assert_contains "unserviced command reports it was only queued" 'queued-not-executed' "$OUT"

echo "== bridge: poll protocol =="
CMD=$(panel_poll)
assert_contains "panel claims the queued command" '"command":"listCompositions"' "$CMD"
assert_contains "claimed command is marked dispatched" '"status":"dispatched"' "$CMD"
AGAIN=$(panel_poll)
assert_contains "a claimed command is not handed out twice" '"command":null' "$AGAIN"
OUT=$(mcp_call bridge-status '{}')
assert_contains "bridge-status sees the poll" '"panelConnected":true' "$OUT"
OUT=$(curl -s "${BASE}/bridge/command?client=${PANEL_CLIENT}")
assert_contains "panel without v=2 is refused" '"command":null' "$OUT"
assert_contains "refused panel is told to reinstall" 'install-bridge.sh' "$OUT"
OUT=$(panel_poll $((PANEL_CLIENT - 1)))
assert_contains "superseded panel is refused" 'superseded' "$OUT"

echo "== bridge: round trip =="
# The tool call blocks on the panel, so the panel has to run concurrently —
# which is the point: /bridge/* is served without the MCP request lock. Wait on
# the tool's own PID, never a bare `wait`: the harness's wasmtime servers are
# background jobs too and never exit.
mcp_call create-composition '{"name":"Hero","width":1920,"height":1080,"frameRate":30}' >/tmp/ae-e2e-tool.json &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success","composition":{"name":"Hero","width":1920}}')
wait $TOOL_PID
OUT=$(cat /tmp/ae-e2e-tool.json)
assert_contains "panel receives the camelCase command name" '"command":"createComposition"' "$CMD"
assert_contains "panel receives the tool arguments" '"name":"Hero"' "$CMD"
assert_contains "panel receives the frame rate as camelCase" '"frameRate":30' "$CMD"
# Absent optional fields must not be sent: the panel distinguishes "omitted"
# (use the default) from any concrete value.
assert_not_contains "omitted optional params are not sent" 'pixelAspect' "$CMD"
assert_contains "tool returns the panel's result" '"name":"Hero"' "$OUT"
assert_contains "successful result is not an error" '"isError":false' "$OUT"
assert_contains "result is machine-readable" '"structuredContent"' "$OUT"

echo "== bridge: colour and position conventions reach the panel verbatim =="
mcp_call create-shape-layer \
  '{"shapeType":"rectangle","position":[960,540],"size":[400,200],"fillColor":[0.1,0.45,0.91],"name":"Card"}' \
  >/tmp/ae-e2e-tool.json &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success"}')
wait $TOOL_PID
assert_contains "shape command name is camelCase" '"command":"createShapeLayer"' "$CMD"
assert_contains "shapeType is passed through" '"shapeType":"rectangle"' "$CMD"
assert_contains "0..1 float colours survive the round trip" '0.45' "$CMD"
assert_contains "centre position is passed through" '"position":[960.0,540.0]' "$CMD"
assert_contains "layer name is passed through" '"Card"' "$CMD"

echo "== bridge: panel-reported failure =="
mcp_call get-layer-info '{}' >/tmp/ae-e2e-tool.json &
TOOL_PID=$!
panel_serve '{"status":"error","message":"No active composition"}' >/dev/null
wait $TOOL_PID
OUT=$(cat /tmp/ae-e2e-tool.json)
assert_contains "panel error surfaces as a tool error" '"isError":true' "$OUT"
assert_contains "panel error message is preserved" 'No active composition' "$OUT"

echo "== bridge: get-results =="
OUT=$(mcp_call get-results '{}')
assert_contains "get-results returns the last result" 'No active composition' "$OUT"

echo "== bridge: batch and argument mapping =="
mcp_call run-batch \
  '{"commands":[{"command":"createShapeLayer","args":{"shapeType":"ellipse"}},{"command":"createTextLayer","args":{"text":"Hi"}}],"undoGroup":"Build scene"}' \
  >/tmp/ae-e2e-tool.json &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success","results":[]}')
wait $TOOL_PID
assert_contains "batch entries name panel scripts" '"createShapeLayer"' "$CMD"
assert_contains "batch keeps entry arguments" '"text":"Hi"' "$CMD"
assert_contains "batch carries the undo group label" 'Build scene' "$CMD"

echo "== bridge: sources are served =="
OUT=$(curl -s "${BASE}/bridge/panel.jsx")
assert_contains "panel.jsx is served" 'mcp' "$OUT"
assert_contains "panel.jsx polls with v=2" 'v=2' "$OUT"
OUT=$(curl -s "${BASE}/healthz")
assert_contains "healthz responds" 'ok' "$OUT"
# An unhandled GET falls through to the MCP transport (405), rather than being
# answered with the discovery document: the default route is `/` and `/health`,
# not "any GET".
CODE=$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/no-such-route")
assert_contains "an unknown GET route is not the discovery document" '405' "$CODE"

guard_tests
mcp_harness_report
