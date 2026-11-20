#!/usr/bin/env bash
# End-to-end smoke test for the deployed NATS MCP server.
#
# Exercises all 25 tools plus the grant boundary against a real Cosmonic
# Desktop deployment. Requires: curl, python3, and the `nats` CLI (to
# provision the demo resources and run a responder).
#
# This replaces the template's `wasmtime serve` harness, which cannot host
# this component any more: it imports `wasmcloud:nats@0.1.0`, and only a
# wasmCloud/Cosmonic host implements that. There is no standalone runtime to
# test against — the host binding IS the dependency under test.
#
# Usage: scripts/smoke.sh [--provision]
#   --provision  create the DEMO stream, `worker` consumer, and demo-kv
#                bucket the deployed grants name, then run the tests.
set -u

HOST="${MCP_HOST:-nats-mcp-server-v1.localhost.cosmonic.sh}"
BASE="${MCP_BASE:-http://127.0.0.1:8200/}"
export NATS_URL="${NATS_URL:-nats://127.0.0.1:4222}"

META='"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}'

PASS=0; FAIL=0; FAILED=()
pass() { PASS=$((PASS + 1)); echo "  ok   - $1"; }
fail() { FAIL=$((FAIL + 1)); FAILED+=("$1"); echo "  FAIL - $1"; [ -n "${2:-}" ] && echo "         $2"; }

# rpc <method> <inner-params> [curl args...] — one JSON-RPC call, SSE unwrapped.
# The stateless 2026-07-28 transport needs the Mcp-Method header and the
# client _meta on every request: there is no session holding them.
rpc() {
  local method="$1" inner="$2"; shift 2
  [ -n "$inner" ] && inner="$inner,"
  printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":{%s%s}}' "$method" "$inner" "$META" \
  | curl -sS --max-time 25 -X POST "$BASE" \
      -H "Host: $HOST" -H 'Content-Type: application/json' \
      -H 'Accept: application/json, text/event-stream' \
      -H 'MCP-Protocol-Version: 2026-07-28' -H "Mcp-Method: $method" \
      "$@" --data-binary @- \
  | unwrap_sse
}

# The transport answers a successful call as SSE (`data: {...}`) but a
# protocol-level error as a plain JSON body. Stripping only `data:` lines threw
# those errors away, so a refusal looked identical to no response at all.
# Emit the SSE payload when there is one, and the raw body otherwise.
unwrap_sse() {
  awk '/^data: /{sub(/^data: /, ""); print; seen = 1; next}
       {body = body $0 "\n"}
       END {if (!seen) printf "%s", body}'
}

# tool <name> <json-args> — call one tool, print its result as compact JSON
# (structured content on success, "TOOL_ERROR <text>" on a tool-level error).
tool() {
  rpc tools/call "\"name\":\"$1\",\"arguments\":$2" -H "Mcp-Name: $1" | python3 -c '
import sys, json
raw = sys.stdin.read().strip()
if not raw: print("NO_RESPONSE"); raise SystemExit
d = json.loads(raw)
if "error" in d: print("PROTOCOL_ERROR", json.dumps(d["error"])); raise SystemExit
r = d["result"]
if r.get("isError"):
    print("TOOL_ERROR", " ".join(c.get("text", "") for c in r.get("content", []))); raise SystemExit
# Compact separators: the assertions below match on `"key":"value"`.
print(json.dumps(r.get("structuredContent", r.get("content")), separators=(",", ":")))
'
}

# expect_either <label> <needle-a> <needle-b> <actual> — passes on either.
# The server-wide tools answer with data when MCP_NATS_MONITOR_URL is set and
# with the enablement recipe when it is not; both are correct behavior, and the
# smoke test must not require an operator to have opted in.
expect_either() {
  case "$4" in
    *"$2"*|*"$3"*) pass "$1" ;;
    *)             fail "$1" "expected '$2' or '$3', got: $4" ;;
  esac
}

# expect <label> <needle> <actual> — substring assertion.
expect() {
  case "$3" in
    *"$2"*) pass "$1" ;;
    *)      fail "$1" "expected to contain '$2', got: $3" ;;
  esac
}

if [ "${1:-}" = "--provision" ]; then
  echo "provisioning demo resources..."
  nats stream add DEMO --subjects 'demo.>' --storage file --defaults >/dev/null 2>&1
  # The consumer's filter must sit INSIDE subject-allow: a consumer whose
  # filter is broader than the grant is invisible to the workload.
  nats consumer add DEMO worker --pull --deliver all --ack explicit \
    --filter 'demo.>' --defaults >/dev/null 2>&1
  nats kv add demo-kv --history=5 --storage file --defaults >/dev/null 2>&1
fi

echo "== handshake =="
OUT=$(rpc initialize '"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}')
expect "initialize reports serverInfo" '"name":"nats-mcp-server-v1"' "$OUT"

OUT=$(rpc tools/list "")
COUNT=$(printf '%s' "$OUT" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["result"]["tools"]))' 2>/dev/null || echo 0)
[ "$COUNT" = "25" ] && pass "tools/list returns 25 tools" || fail "tools/list returns 25 tools" "got $COUNT"

echo "== core =="
expect "nats_publish"            '"ok":true'        "$(tool nats_publish '{"subject":"demo.smoke","payload":"hello"}')"
expect "nats_request no-responders" 'no-responders' "$(tool nats_request '{"subject":"rpc.nobody","payload":"?","timeout_ms":1500}')"

# A live responder, so request/reply is tested against something that answers.
nats reply 'rpc.echo' --echo >/dev/null 2>&1 &
REPLIER=$!
trap 'kill $REPLIER 2>/dev/null' EXIT
sleep 1
expect "nats_request round-trip" 'ping' "$(tool nats_request '{"subject":"rpc.echo","payload":"ping","timeout_ms":3000}')"

echo "== jetstream =="
expect "jetstream_publish acked"   '"stream":"DEMO"'  "$(tool jetstream_publish '{"subject":"demo.orders","payload":"{\"o\":1}","msg_id":"smoke-1"}')"
expect "msg_id dedupes"            '"duplicate":true' "$(tool jetstream_publish '{"subject":"demo.orders","payload":"{\"o\":1}","msg_id":"smoke-1"}')"
expect "jetstream_stream_info"     '"name":"DEMO"'    "$(tool jetstream_stream_info '{"stream":"DEMO"}')"
expect "jetstream_list_subjects"   'demo.orders'      "$(tool jetstream_list_subjects '{"stream":"DEMO"}')"
expect "jetstream_scan"            '"messages"'       "$(tool jetstream_scan '{"stream":"DEMO","start_sequence":1,"max_count":3}')"
expect "jetstream_get_message"     '"sequence":1'     "$(tool jetstream_get_message '{"stream":"DEMO","sequence":1}')"
expect "jetstream_consumer_info"   '"name":"worker"'  "$(tool jetstream_consumer_info '{"stream":"DEMO","consumer":"worker"}')"
expect "fetch leaves msgs unsettled" '"settled":"unsettled"' "$(tool jetstream_fetch '{"stream":"DEMO","consumer":"worker","batch":1,"timeout_ms":3000}')"
expect "fetch settle=ack"          '"settled":"acked"' "$(tool jetstream_fetch '{"stream":"DEMO","consumer":"worker","batch":10,"timeout_ms":3000,"settle":"ack"}')"

echo "== binary round-trip =="
tool jetstream_publish '{"subject":"demo.bin","payload_base64":"AAH/"}' >/dev/null
LAST=$(tool jetstream_stream_info '{"stream":"DEMO"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["last_sequence"])')
# Build the args in a variable rather than inline: escaped quotes nested inside
# "$( ... )" get re-parsed by bash and reach the server as malformed JSON.
ARGS="{\"stream\":\"DEMO\",\"sequence\":$LAST}"
expect "non-UTF-8 body returns base64" '"base64":"AAH/"' "$(tool jetstream_get_message "$ARGS")"

echo "== key/value =="
tool kv_purge '{"bucket":"demo-kv","key":"smoke.key"}' >/dev/null
expect "kv_put"      '"revision":'     "$(tool kv_put '{"bucket":"demo-kv","key":"smoke.key","value":"v1"}')"
expect "kv_get"      '"text":"v1"'     "$(tool kv_get '{"bucket":"demo-kv","key":"smoke.key"}')"
expect "kv_create refuses existing" 'TOOL_ERROR' "$(tool kv_create '{"bucket":"demo-kv","key":"smoke.key","value":"v2"}')"
expect "kv_update CAS reports revision" 'revision-mismatch' "$(tool kv_update '{"bucket":"demo-kv","key":"smoke.key","value":"v2","expected_revision":999999}')"
REV=$(tool kv_get '{"bucket":"demo-kv","key":"smoke.key"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["revision"])')
ARGS="{\"bucket\":\"demo-kv\",\"key\":\"smoke.key\",\"value\":\"v2\",\"expected_revision\":$REV}"
expect "kv_update CAS succeeds" '"revision":' "$(tool kv_update "$ARGS")"
expect "kv_keys"     'smoke.key'       "$(tool kv_keys '{"bucket":"demo-kv"}')"
expect "kv_history"  '"operation":"put"' "$(tool kv_history '{"bucket":"demo-kv","key":"smoke.key"}')"
expect "kv_status"   '"bucket":"demo-kv"' "$(tool kv_status '{"bucket":"demo-kv"}')"
expect "kv_delete"   '"ok":true'       "$(tool kv_delete '{"bucket":"demo-kv","key":"smoke.key"}')"
expect "deleted key is not found" 'key-not-found' "$(tool kv_get '{"bucket":"demo-kv","key":"smoke.key"}')"
expect "kv_purge"    '"ok":true'       "$(tool kv_purge '{"bucket":"demo-kv","key":"smoke.key"}')"

echo "== default route + skills over MCP =="
# The discovery route is outside the Host guard on purpose, so probe it with a
# bare GET rather than through rpc().
DISCO="$(curl -sS --max-time 10 -H "Host: $HOST" "$BASE")"
expect "GET / returns a discovery document" '"status": "ok"' "$DISCO"
expect "discovery names the MCP endpoint"   '"mcp"'          "$DISCO"
expect "discovery lists tools"              'nats_diagnose'  "$DISCO"
expect "discovery lists skills"             'skill://'       "$DISCO"
expect "GET /health also answers" '"status": "ok"' \
  "$(curl -sS --max-time 10 -H "Host: $HOST" "${BASE}health")"

RESOURCES="$(rpc resources/list '' -H 'Mcp-Name: resources/list')"
expect "resources/list includes the skill index" 'skill://index.json' "$RESOURCES"
expect "resources/list includes the playbook"    'SKILL.md'           "$RESOURCES"
expect "resource templates are advertised" 'skill://{skill}' \
  "$(rpc resources/templates/list '' -H 'Mcp-Name: resources/templates/list')"

INDEX="$(rpc resources/read '"uri":"skill://index.json"' -H 'Mcp-Name: skill://index.json')"
expect "skill index names this server"    'nats-mcp-server-v1' "$INDEX"
expect "skill index carries a trigger description" 'description' "$INDEX"
PLAYBOOK="$(rpc resources/read '"uri":"skill://nats-mcp-server-v1/SKILL.md"' -H 'Mcp-Name: skill://nats-mcp-server-v1/SKILL.md')"
expect "SKILL.md is served"                'Start with a diagnosis' "$PLAYBOOK"
expect "supporting file is served" 'jetstream_consumer_lag' \
  "$(rpc resources/read '"uri":"skill://nats-mcp-server-v1/references/TOOLS.md"' -H 'Mcp-Name: skill://nats-mcp-server-v1/references/TOOLS.md')"
expect "unknown skill URI is refused" 'no resource at' \
  "$(rpc resources/read '"uri":"skill://nope/SKILL.md"' -H 'Mcp-Name: skill://nope/SKILL.md')"

echo "== diagnostics =="
# Sampling tools take a real wall-clock window; keep it short here.
expect "stream_rate reports a measured rate" '"msgs_per_sec"' \
  "$(tool jetstream_stream_rate '{"stream":"DEMO","sample_ms":400}')"
expect "stream_rate reports the sampling window" '"sample_ms"' \
  "$(tool jetstream_stream_rate '{"stream":"DEMO","sample_ms":400}')"
expect "consumer_lag returns a verdict" '"verdict"' \
  "$(tool jetstream_consumer_lag '{"stream":"DEMO","consumer":"worker","sample_ms":400}')"
expect "consumer_lag explains the verdict" '"interpretation"' \
  "$(tool jetstream_consumer_lag '{"stream":"DEMO","consumer":"worker","sample_ms":400}')"

# The binding path works with no monitoring endpoint, and says what it skipped.
DIAG="$(tool nats_diagnose '{"streams":["DEMO"],"consumers":["DEMO/worker"],"sample_ms":400}')"
expect "diagnose (binding) names its source"      '"source":"binding"' "$DIAG"
expect "diagnose (binding) reports its limits"    'Retention configuration is not readable' "$DIAG"
expect "diagnose (binding) returns findings"      '"findings"' "$DIAG"
expect "diagnose reports its capability level"   '"capability"' "$DIAG"
expect "diagnose says retention rules were off"  '"retention_rules":false' "$DIAG"
expect "diagnose names what would unlock more"   '"unlocks"' "$DIAG"
# With no arguments the tool diagnoses the whole server when monitoring is on,
# and refuses (with the reason) when it is off. Both are correct.
# Three correct outcomes, depending on how the workload is configured:
# monitoring reports the whole server, hints scope it to the binding, and with
# neither the tool refuses and says why. All three carry a capability level.
expect_either "diagnose with no args: a scoped report or a clear refusal" \
  '"level"' 'nothing-to-diagnose' \
  "$(tool nats_diagnose '{"sample_ms":400}')"

ACCESS="$(tool nats_check_access '{"streams":["DEMO"],"buckets":["demo-kv"]}')"
expect "check_access finds the granted stream" '"access":"granted"' "$ACCESS"
expect "check_access probes the bucket too"    '"bucket":"demo-kv"' "$ACCESS"
expect "check_access flags an ungranted stream" '"access":"denied"' \
  "$(tool nats_check_access '{"streams":["definitely-not-granted"]}')"

echo "== server-wide telemetry (skipped cleanly when not configured) =="
expect_either "server_info: data or the enablement recipe" '"version"' 'monitor-not-configured' \
  "$(tool nats_server_info '{}')"
expect_either "server_streams: inventory or the recipe" '"streams"' 'monitor-not-configured' \
  "$(tool nats_server_streams '{"include_consumers":false}')"
expect_either "connections: clients or the recipe" '"num_connections"' 'monitor-not-configured' \
  "$(tool nats_connections '{"limit":5}')"

echo "== grant boundary (the security-relevant part) =="
expect "subject outside grant denied" 'subject-allow' "$(tool nats_publish '{"subject":"secrets.exfil","payload":"x"}')"
expect "bucket outside grant denied"  'bucket-allow'  "$(tool kv_get '{"bucket":"not-granted","key":"k"}')"
expect "wildcard publish refused"     'must be literal' "$(tool nats_publish '{"subject":"demo.>","payload":"x"}')"

echo "== concurrency (warm pool) =="
CONC_FAIL=0
# Collect these PIDs and wait on them specifically: a bare `wait` would also
# wait on the rpc.echo responder above, which never exits.
CONC_PIDS=()
for i in $(seq 1 8); do
  ( tool kv_put "{\"bucket\":\"demo-kv\",\"key\":\"smoke.conc.$i\",\"value\":\"v\"}" > "/tmp/smoke-conc-$i.json" ) &
  CONC_PIDS+=($!)
done
wait "${CONC_PIDS[@]}"
for i in $(seq 1 8); do grep -q '"revision"' "/tmp/smoke-conc-$i.json" || CONC_FAIL=$((CONC_FAIL + 1)); rm -f "/tmp/smoke-conc-$i.json"; done
[ "$CONC_FAIL" = "0" ] && pass "8 concurrent tool calls all succeed" || fail "8 concurrent tool calls all succeed" "$CONC_FAIL failed"
for i in $(seq 1 8); do tool kv_purge "{\"bucket\":\"demo-kv\",\"key\":\"smoke.conc.$i\"}" >/dev/null; done

echo
echo "passed: $PASS   failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '  - %s\n' "${FAILED[@]}"
  exit 1
fi
