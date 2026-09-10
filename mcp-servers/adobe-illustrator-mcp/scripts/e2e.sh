#!/usr/bin/env bash
# End-to-end tests for illustrator-mcp. See scripts/mcp_e2e_lib.sh for the
# shared framework harness (protocol, spec enforcement, robustness, guard).
#
# The live-control tools need a wasi:keyvalue host, which `wasmtime serve` does
# not provide, so the harness composes testing/kv-stub into the component with
# `wac plug` and drives the bridge endpoints itself — curl plays the part of
# the Illustrator bridge. No copy of Illustrator is involved.
set -u

cd "$(dirname "$0")/.."

PORT=8189
GUARD_PORT=8188
WASM=target/wasm32-wasip2/release/illustrator_mcp.wasm
PLUGGED=target/wasm32-wasip2/release/illustrator_mcp_plugged.wasm
STUB=testing/kv-stub/target/wasm32-wasip2/release/kv_stub.wasm

# shellcheck source=mcp_e2e_lib.sh
source "$(dirname "$0")/mcp_e2e_lib.sh"

# Used by the framework's concurrency test.
FIRST_TOOL_NAME=hex_to_rgb
FIRST_TOOL_ARGS='{"hex":"#ff8800"}'
FIRST_TOOL_EXPECT='255,136,0'

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

# --- bridge simulation ------------------------------------------------------

# Identifies the simulated bridge. The server serves only the highest client
# id it has seen, so any test that registers a newer bridge must raise this.
PANEL_CLIENT=1000

# panel_poll [client] — one bridge poll, as an up-to-date bridge would make it.
panel_poll() { curl -s "${BASE}/bridge/command?v=2&client=${1:-$PANEL_CLIENT}"; }

# panel_answer <id> <result-json> — post a result for a dispatched command.
panel_answer() {
  curl -s -o /dev/null -X POST "${BASE}/bridge/result?id=$1" \
    -H 'Content-Type: application/json' -d "$2"
}

# json_field <json> <python-expression over `d`>
json_field() { printf '%s' "$1" | python3 -c "import sys,json;d=json.load(sys.stdin);print($2)"; }

# panel_serve <result-json> — claim the next command and answer it. Echoes the
# command the bridge received so callers can assert on the arguments.
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

framework_tests hex_to_rgb rgb_to_hex convert_units bridge_status get_results \
  get_help run_script run_jsx get_document_info list_documents list_page_items \
  list_text_frames list_artboards list_layers list_swatches list_fonts \
  new_document open_document save_document close_document export_document \
  place_image add_artboard set_active_artboard add_layer set_layer \
  delete_layer draw_rectangle draw_ellipse draw_line draw_polygon draw_star \
  add_text set_text_frame select_all deselect_all select_by_name \
  get_selection move_selection scale_selection rotate_selection \
  duplicate_selection delete_selection set_fill set_stroke set_opacity \
  group_selection ungroup_selection bring_to_front send_to_back undo redo \
  run_batch

# The default route and Skills over MCP, both from the upstream template.
discovery_tests illustrator-mcp
skills_tests illustrator-mcp references/TOOLS.md references/HANDOFF.md

echo "== hex_to_rgb / rgb_to_hex =="
OUT=$(mcp_call hex_to_rgb '{"hex":"#ff8800"}')
assert_contains "ff8800 -> 255,136,0" '"rgba_255":[255,136,0,255]' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"f80"}')
assert_contains "shorthand f80 expands" '[255,136,0,255]' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"#11223344"}')
assert_contains "8-digit hex keeps alpha" '[17,34,51,68]' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"nothex"}')
assert_contains "non-hex rejected" '-32602' "$OUT"
OUT=$(mcp_call hex_to_rgb '{"hex":"#12345"}')
assert_contains "5-digit hex rejected" '3, 6, or 8' "$OUT"
OUT=$(mcp_call rgb_to_hex '{"r":255,"g":136,"b":0}')
assert_contains "255,136,0 -> #ff8800" '"hex":"#ff8800"' "$OUT"
OUT=$(mcp_call rgb_to_hex '{"r":300,"g":0,"b":0}')
assert_contains "out-of-range component rejected" '-32602' "$OUT"

echo "== convert_units =="
OUT=$(mcp_call convert_units '{"value":1,"from":"in","to":"pt"}')
assert_contains "1 in = 72 pt" '"value":72' "$OUT"
OUT=$(mcp_call convert_units '{"value":25.4,"from":"mm","to":"in"}')
assert_contains "25.4 mm = 1 in" '"value":1' "$OUT"
OUT=$(mcp_call convert_units '{"value":100,"from":"px","to":"pt"}')
assert_contains "px == pt at 72 ppi" '"value":100' "$OUT"
OUT=$(mcp_call convert_units '{"value":1e308,"from":"in","to":"pt"}')
assert_contains "overflow to non-finite rejected" '-32602' "$OUT"

echo "== run_jsx gate =="
# The primary server starts WITHOUT MCP_ALLOW_RAW_JSX: the escape hatch must
# be off by default, on every path that can reach it.
OUT=$(mcp_call run_jsx '{"script":"app.documents.length"}')
assert_contains "run_jsx disabled by default" 'raw-jsx-disabled' "$OUT"
assert_contains "run_jsx denial is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call run_script '{"script":"runJsx","parameters":{"script":"1+1"}}')
assert_contains "run_script cannot smuggle runJsx" 'raw-jsx-disabled' "$OUT"
OUT=$(mcp_call run_batch '{"commands":[{"command":"runJsx","args":{"script":"1+1"}}]}')
assert_contains "run_batch cannot smuggle runJsx" 'raw-jsx-disabled' "$OUT"
OUT=$(mcp_call run_script '{"script":"rm -rf /","parameters":{}}')
assert_contains "unknown script rejected" 'unknown variant' "$OUT"
assert_contains "unknown script is a tool error" '"isError":true' "$OUT"

echo "== bridge: no bridge connected =="
OUT=$(mcp_call bridge_status '{}')
assert_contains "bridge_status reports no bridge" '"panelConnected":false' "$OUT"
assert_contains "bridge_status says how to connect" 'install-bridge.sh' "$OUT"
OUT=$(mcp_call get_results '{}')
assert_contains "get_results with nothing queued" 'no-results' "$OUT"
# A command with no bridge is queued but NOT executed, and says so as a tool
# error — a success here would claim work that never happened.
OUT=$(mcp_call list_layers '{}')
assert_contains "unserviced command is a tool error" '"isError":true' "$OUT"
assert_contains "unserviced command reports it was only queued" 'queued-not-executed' "$OUT"

echo "== bridge: poll protocol =="
OUT=$(panel_poll)
assert_contains "bridge claims the queued command" '"command":"listLayers"' "$OUT"
assert_contains "claimed command is marked dispatched" '"status":"dispatched"' "$OUT"
OUT=$(panel_poll)
assert_contains "a claimed command is not handed out twice" '"command":null' "$OUT"
OUT=$(mcp_call bridge_status '{}')
assert_contains "bridge_status sees the poll" '"panelConnected":true' "$OUT"
# A bridge that does not declare v=2 mishandles our responses, so it is
# refused rather than allowed to consume and drop commands.
OUT=$(curl -s "${BASE}/bridge/command?client=1000")
assert_contains "bridge without v=2 is refused" '"command":null' "$OUT"
assert_contains "refused bridge is told to reinstall" 'outdated or has been superseded' "$OUT"
# Highest client id wins, so reloading the panel supersedes the old instance.
curl -s -o /dev/null "${BASE}/bridge/command?v=2&client=2000"
OUT=$(curl -s "${BASE}/bridge/command?v=2&client=1000")
assert_contains "superseded bridge is refused" 'superseded' "$OUT"
PANEL_CLIENT=2000

echo "== bridge: round trip =="
# The tool call blocks on the bridge, so the bridge has to run concurrently —
# which is the point: /bridge/* is served without the MCP request lock.
mcp_call draw_rectangle '{"x":10,"y":20,"width":100,"height":50,"fill":"#ff0000","name":"Box"}' >/tmp/ilst-e2e-tool.json &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success","item":{"type":"PathItem","name":"Box","x":10,"y":20,"width":100,"height":50}}')
wait $TOOL_PID
OUT=$(cat /tmp/ilst-e2e-tool.json)
assert_contains "bridge receives the camelCase command name" '"command":"drawRectangle"' "$CMD"
assert_contains "bridge receives the tool arguments" '"width":100' "$CMD"
assert_contains "bridge receives the fill color" '"fill":"#ff0000"' "$CMD"
assert_contains "tool returns the bridge's result" '"name":"Box"' "$OUT"
assert_contains "successful result is not an error" '"isError":false' "$OUT"
assert_contains "result is machine-readable" '"structuredContent"' "$OUT"
# Absent optional fields must not be sent: the bridge distinguishes "omitted"
# (use the default) from any concrete value.
assert_not_contains "omitted optional params are not sent" 'cornerRadius' "$CMD"

echo "== bridge: bridge-reported failure =="
mcp_call delete_layer '{"name":"NoSuchLayer"}' >/tmp/ilst-e2e-tool.json &
TOOL_PID=$!
panel_serve '{"status":"error","message":"no layer named NoSuchLayer"}' >/dev/null
wait $TOOL_PID
OUT=$(cat /tmp/ilst-e2e-tool.json)
assert_contains "bridge error surfaces as a tool error" '"isError":true' "$OUT"
assert_contains "bridge error message is preserved" 'no layer named' "$OUT"

echo "== bridge: get_results =="
OUT=$(mcp_call get_results '{}')
assert_contains "get_results returns the last result" 'no layer named' "$OUT"

echo "== bridge: batch and argument mapping =="
mcp_call run_batch '{"commands":[{"command":"addLayer","args":{"name":"Art"}},{"command":"drawEllipse","args":{"x":0,"y":0,"width":50,"height":50}}]}' >/dev/null &
TOOL_PID=$!
CMD=$(panel_serve '{"status":"success","requested":2,"completed":2,"failed":0}')
wait $TOOL_PID
assert_contains "batch entries name bridge scripts" '"command":"addLayer"' "$CMD"
assert_contains "batch keeps entry arguments" '"name":"Art"' "$CMD"

echo "== bridge: sources are served =="
OUT=$(curl -s "${BASE}/bridge/pump.jsx")
assert_contains "pump.jsx is served" 'pumpRun' "$OUT"
assert_contains "pump.jsx carries the command library" 'executeCommand' "$OUT"
assert_contains "pump polls with v=2" '/bridge/command?v=2' "$OUT"
OUT=$(curl -s "${BASE}/bridge/commands.jsx")
assert_contains "commands.jsx is served" 'ilstMcpExecuteWire' "$OUT"
OUT=$(curl -s "${BASE}/bridge/shuttle.sh")
assert_contains "shuttle.sh is served" 'ilstMcpExecuteWire' "$OUT"
OUT=$(curl -s "${BASE}/bridge/cep/manifest.xml")
assert_contains "CEP manifest is served" 'com.cosmonic.illustrator-mcp' "$OUT"
OUT=$(curl -s "${BASE}/bridge/cep/main.js")
assert_contains "CEP main.js is served" 'ilstMcpExecuteWire' "$OUT"
OUT=$(curl -s -D - -o /dev/null "${BASE}/bridge/command?v=2&client=1")
assert_contains "bridge responses carry CORS headers" 'access-control-allow-origin' "$OUT"
OUT=$(curl -s "${BASE}/healthz")
assert_contains "healthz responds" 'ok' "$OUT"

guard_tests
mcp_harness_report
