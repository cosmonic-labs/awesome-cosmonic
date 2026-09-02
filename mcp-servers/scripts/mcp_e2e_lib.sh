#!/usr/bin/env bash
# Shared end-to-end test harness for the MCP servers in this directory.
#
# A per-server scripts/e2e.sh sources this file, then:
#   - sets WASM, PORT, GUARD_PORT (and optional FIXTURE_PORT) before sourcing,
#   - calls mcp_harness_start [--env K=V ...] / mcp_harness_start_guard,
#   - runs framework_tests <tool...> (protocol + spec enforcement + robustness),
#     discovery_tests (the GET / and GET /health default route),
#     skills_tests <skill> [<supporting-file>] (Skills over MCP resources),
#   - adds its own tool cases with mcp_call / assert_contains / assert_json,
#   - calls guard_tests and finally mcp_harness_report.
#
# Everything runs under `wasmtime serve -Sp3,cli,http` — purely as test
# infrastructure; deployment is Cosmonic Desktop. Requires: cargo with the
# wasm32-wasip2 target, wasm-tools, curl, python3, and wasmtime 46.x or 47.x
# (set WASMTIME=/path/to/wasmtime to pick one; 48.0.1 traps on every request
# with "waitable cannot be used synchronously while added to a waitable set"
# against the template's wit-bindgen 0.57 runtime — Desktop 0.5.27 runs 47).
#
# Derived from cosmonic-labs/mcp-examples scripts/mcp_e2e_lib.sh (Apache-2.0),
# extended with the discovery-route and skills sections the template's own
# e2e.sh carries.
set -u

WASMTIME="${WASMTIME:-wasmtime}"
# Base URL of the instance under test. Default: the wasmtime instance on
# $PORT. A Desktop-only suite (a server importing a host capability wasmtime
# cannot provide) sets MCP_BASE=http://<name>.localhost:8200/ and skips the
# wasmtime/guard sections.
MCP_BASE="${MCP_BASE:-http://127.0.0.1:${PORT:-0}/}"
META='"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}'
ACCEPT='Accept: application/json, text/event-stream'
CT='Content-Type: application/json'
PV='MCP-Protocol-Version: 2026-07-28'

PASS=0
FAIL=0
FAILED_NAMES=()
SERVER_PID=""
GUARD_PID=""
FIXTURE_PID=""
# Scratch files carry the port so concurrent suites never clobber each other.
E2E_TMP="${TMPDIR:-/tmp}/mcp-e2e-${PORT:-0}"
mkdir -p "$E2E_TMP"

pass() { PASS=$((PASS + 1)); echo "  ok   - $1"; }
fail() {
  FAIL=$((FAIL + 1))
  FAILED_NAMES+=("$1")
  echo "  FAIL - $1"
  echo "         ${2:-}" | head -c 600
  echo
}

# assert_contains <name> <needle> <haystack>
assert_contains() {
  case "$3" in
    *"$2"*) pass "$1" ;;
    *) fail "$1" "expected to contain [$2], got: $3" ;;
  esac
}

# assert_not_contains <name> <needle> <haystack>
assert_not_contains() {
  case "$3" in
    *"$2"*) fail "$1" "expected NOT to contain [$2], got: $3" ;;
    *) pass "$1" ;;
  esac
}

# assert_json <name> <python-expr> <sse-or-json-text>
# Extracts the first JSON-RPC message from an SSE (`data:`) or plain JSON
# body and evaluates <python-expr> with `r` bound to the parsed object
# (e.g. 'r["result"]["structuredContent"]["count"] == 3').
assert_json() {
  local name="$1" expr="$2" body="$3"
  if printf '%s' "$body" | python3 -c '
import json, sys
raw = sys.stdin.read()
msg = None
for line in raw.splitlines():
    if line.startswith("data:"):
        msg = json.loads(line[5:].strip()); break
if msg is None:
    msg = json.loads(raw)
r = msg
sys.exit(0 if eval(sys.argv[1]) else 1)
' "$expr" 2>/dev/null; then
    pass "$name"
  else
    fail "$name" "expression [$expr] false for: $body"
  fi
}

# mcp_post <base-url> <extra curl args...> — POST stdin with MCP headers.
mcp_post() {
  local base="$1"; shift
  curl -sS --max-time 30 -X POST "$base" -H "$CT" -H "$ACCEPT" -H "$PV" "$@" --data-binary @-
}

# mcp_call <tool> <json-arguments> — call a tool on the primary server,
# echoing the SSE `data:` payload. Arguments must be a JSON object literal.
mcp_call() {
  local tool="$1" args="$2"
  printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args},${META}}}" \
    | mcp_post "$MCP_BASE" -H 'Mcp-Method: tools/call' -H "Mcp-Name: ${tool}"
}

# mcp_call_on <base-url> <tool> <json-arguments> — same, against another instance
# (e.g. the guard instance started without a credential).
mcp_call_on() {
  local base="$1" tool="$2" args="$3"
  printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args},${META}}}" \
    | mcp_post "$base" -H 'Mcp-Method: tools/call' -H "Mcp-Name: ${tool}"
}

# mcp_read_resource <uri> — resources/read on the primary server.
mcp_read_resource() {
  local uri="$1"
  printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"resources/read\",\"params\":{\"uri\":\"${uri}\",${META}}}" \
    | mcp_post "$MCP_BASE" -H 'Mcp-Method: resources/read' -H "Mcp-Name: ${uri}"
}

# mcp_initialize <base-url>
mcp_initialize() {
  printf '%s' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}' \
    | mcp_post "$1"
}

mcp_harness_cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  [ -n "$GUARD_PID" ] && kill "$GUARD_PID" 2>/dev/null
  [ -n "$FIXTURE_PID" ] && kill "$FIXTURE_PID" 2>/dev/null
  wait 2>/dev/null
}
trap mcp_harness_cleanup EXIT

# mcp_build_if_needed <arg1> — builds unless arg1 is --no-build.
mcp_build_if_needed() {
  if [ "${1:-}" != "--no-build" ]; then
    echo "building component..."
    cargo build --release || exit 1
  fi
  [ -f "$WASM" ] || { echo "missing $WASM"; exit 1; }
  echo "verifying component world..."
  if wasm-tools component wit "$WASM" 2>/dev/null | grep -q 'export wasi:http/handler@0.3.0'; then
    pass "component exports wasi:http/handler@0.3.0"
  else
    fail "component exports wasi:http/handler@0.3.0" "export missing"
  fi
}

# mcp_wait_ready <port>
mcp_wait_ready() {
  for _ in $(seq 1 75); do
    curl -s -o /dev/null "http://127.0.0.1:${1}/" && return 0
    sleep 0.2
  done
  echo "server on :$1 did not become ready" >&2
  return 1
}

# mcp_harness_start [wasmtime args: --env KEY=VAL, --dir host::guest ...] —
# start the primary server. Always binds 127.0.0.1.
mcp_harness_start() {
  echo "starting wasmtime serve on :${PORT}..."
  "$WASMTIME" serve -Sp3,cli,http "$@" --addr "127.0.0.1:${PORT}" "$WASM" \
    >"$E2E_TMP/server.log" 2>&1 &
  SERVER_PID=$!
  mcp_wait_ready "$PORT"
}

# mcp_harness_start_guard [wasmtime args...] — a second instance with
# MCP_ALLOWED_HOSTS pinned to its own port, to test the Host-header guard. Per
# convention it is ALSO the instance started without credentials, so the
# missing-secret path can be tested on it.
mcp_harness_start_guard() {
  echo "starting guard instance on :${GUARD_PORT}..."
  "$WASMTIME" serve -Sp3,cli,http --env "MCP_ALLOWED_HOSTS=127.0.0.1:${GUARD_PORT}" \
    "$@" --addr "127.0.0.1:${GUARD_PORT}" "$WASM" >"$E2E_TMP/guard.log" 2>&1 &
  GUARD_PID=$!
  mcp_wait_ready "$GUARD_PORT"
}

# framework_tests <tool1> <tool2> ... — tool-agnostic protocol, spec, and
# robustness checks. Pass the server's tool names to assert they are listed.
# Set FIRST_TOOL_NAME / FIRST_TOOL_ARGS / FIRST_TOOL_EXPECT before calling:
# the concurrency test fires that tool 8x at once (point it at an outbound
# tool so concurrent outbound is exercised).
framework_tests() {
  local base="$MCP_BASE"

  echo "== protocol =="
  local out hdrs
  out=$(mcp_initialize "$base")
  assert_contains "initialize negotiates 2026-07-28" '"protocolVersion":"2026-07-28"' "$out"
  assert_contains "initialize advertises tools" '"tools"' "$out"
  assert_contains "initialize advertises resources (skills)" '"resources"' "$out"

  hdrs=$(printf '%s' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}' \
    | curl -sS --max-time 20 -X POST "$base" -H "$CT" -H "$ACCEPT" -H "$PV" -D - -o /dev/null --data-binary @-)
  assert_contains "initialize streams as SSE" 'text/event-stream' "$hdrs"

  out=$(printf '%s' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"old","version":"0"}}}' | mcp_post "$base")
  assert_contains "older protocol client still served (statelessly)" '"jsonrpc":"2.0"' "$out"

  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{$META}}" | mcp_post "$base" -H 'Mcp-Method: tools/list')
  local tool
  for tool in "$@"; do
    assert_contains "tools/list contains $tool" "\"name\":\"$tool\"" "$out"
  done
  # Every tool must carry a description and an input schema.
  assert_not_contains "every tool has a description" '"description":""' "$out"

  out=$(printf '%s' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}' \
    | curl -sS --max-time 20 -X POST "${base}mcp" -H "$CT" -H "$ACCEPT" -H "$PV" --data-binary @-)
  assert_contains "POST /mcp serves the protocol" '"protocolVersion":"2026-07-28"' "$out"

  echo "== spec enforcement =="
  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/list\",\"params\":{$META}}" | mcp_post "$base")
  assert_contains "missing Mcp-Method header rejected" 'Mcp-Method' "$out"

  out=$(printf '%s' '{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}' | mcp_post "$base" -H 'Mcp-Method: tools/list')
  assert_contains "missing _meta rejected" '-32602' "$out"

  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/list\",\"params\":{$META}}" | curl -sS --max-time 20 -X POST "$base" -H "$CT" -H 'Accept: application/json' -H "$PV" -H 'Mcp-Method: tools/list' --data-binary @-)
  assert_contains "Accept without text/event-stream rejected" 'must accept' "$out"

  out=$(printf '%s' 'not json at all{{' | mcp_post "$base" -H 'Mcp-Method: tools/list')
  assert_not_contains "malformed JSON gets an error, not a hang" '"result"' "$out"
  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/list\",\"params\":{$META}}" | mcp_post "$base" -H 'Mcp-Method: tools/list')
  assert_contains "server alive after malformed JSON" '"tools"' "$out"

  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"no_such_tool\",\"arguments\":{},$META}}" | mcp_post "$base" -H 'Mcp-Method: tools/call' -H 'Mcp-Name: no_such_tool')
  assert_contains "unknown tool is a clean error" '"error"' "$out"

  local code
  code=$(curl -sS --max-time 20 -o /dev/null -w '%{http_code}' -X DELETE "$base")
  assert_contains "DELETE is 405 (stateless: nothing to delete)" '405' "$code"

  echo "== robustness =="
  code=$(python3 -c "import sys; sys.stdout.write('{\"padding\":\"' + 'x' * (5 * 1024 * 1024) + '\"}')" \
    | mcp_post "$base" -H 'Mcp-Method: tools/list' -o /dev/null -w '%{http_code}')
  case "$code" in
    413 | 400) pass "oversized body (5 MiB) rejected with $code" ;;
    *) fail "oversized body (5 MiB) rejected" "http status: $code" ;;
  esac

  local conc_fails=0 pids=() i
  for i in $(seq 1 8); do
    ( mcp_call "${FIRST_TOOL_NAME}" "${FIRST_TOOL_ARGS}" > "$E2E_TMP/conc-$i.json" ) &
    pids+=($!)
  done
  wait "${pids[@]}"
  for i in $(seq 1 8); do
    grep -q "${FIRST_TOOL_EXPECT}" "$E2E_TMP/conc-$i.json" || conc_fails=$((conc_fails + 1))
  done
  if [ "$conc_fails" -eq 0 ]; then
    pass "8 concurrent ${FIRST_TOOL_NAME} calls all succeed"
  else
    fail "8 concurrent ${FIRST_TOOL_NAME} calls all succeed" "$conc_fails of 8 failed: $(head -c 300 "$E2E_TMP/conc-1.json")"
  fi
}

# discovery_tests [<tool-name-expected-in-document>] — the default route.
# GET / and GET /health must answer 200 with the JSON discovery document,
# without shadowing the protocol on POST.
discovery_tests() {
  local base="$MCP_BASE" expect_tool="${1:-}"
  echo "== default route (discovery) =="
  local code root hdrs
  code=$(curl -sS --max-time 20 -o "$E2E_TMP/root.json" -w '%{http_code}' "$base")
  assert_contains "GET / returns 200 (not a 404/405 dead end)" '200' "$code"
  root=$(cat "$E2E_TMP/root.json")
  assert_contains "GET / reports status ok" '"status": "ok"' "$root"
  assert_contains "GET / names the MCP spec version" '2026-07-28' "$root"
  assert_contains "GET / points at the skill index" 'skill://index.json' "$root"
  assert_contains "GET / names the server" "\"name\": \"$(basename "$PWD")\"" "$root"
  [ -n "$expect_tool" ] && assert_contains "GET / lists tool $expect_tool" "\"$expect_tool\"" "$root"
  hdrs=$(curl -sS --max-time 20 -D - -o /dev/null "$base")
  assert_contains "GET / is served as JSON" 'application/json' "$hdrs"
  assert_not_contains "GET / sets no CORS header" 'access-control-allow-origin' "$(printf '%s' "$hdrs" | tr 'A-Z' 'a-z')"
  code=$(curl -sS --max-time 20 -o /dev/null -w '%{http_code}' "${base}health")
  assert_contains "GET /health returns 200" '200' "$code"
  # The discovery route sits outside the Host guard: probes under any Host work.
  # (wasmtime instances only — on Desktop the ingress itself routes by Host.)
  if [ -n "$GUARD_PID" ]; then
    code=$(curl -sS --max-time 20 -o /dev/null -w '%{http_code}' -H 'Host: probe.example' "http://127.0.0.1:${GUARD_PORT}/health")
    assert_contains "GET /health answers under any Host (probe-friendly)" '200' "$code"
  fi
}

# skills_tests <skill-name> [<supporting-file-path> ...] — Skills over MCP.
skills_tests() {
  local skill="$1"; shift
  local base="$MCP_BASE"
  echo "== skills over MCP (resources) =="
  local out
  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"resources/list\",\"params\":{$META}}" | mcp_post "$base" -H 'Mcp-Method: resources/list')
  assert_contains "resources/list contains the skill catalog" 'skill://index.json' "$out"
  assert_contains "resources/list contains skill://$skill/SKILL.md" "skill://$skill/SKILL.md" "$out"
  local file
  for file in "$@"; do
    assert_contains "resources/list contains skill://$skill/$file" "skill://$skill/$file" "$out"
  done

  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"resources/templates/list\",\"params\":{$META}}" | mcp_post "$base" -H 'Mcp-Method: resources/templates/list')
  assert_contains "resources/templates/list exposes the skill URI template" 'skill://{skill}/SKILL.md' "$out"

  out=$(mcp_read_resource "skill://index.json")
  assert_contains "skill index declares the skills extension" 'io.modelcontextprotocol/skills' "$out"
  assert_contains "skill index lists $skill" "\\\"name\\\": \\\"$skill\\\"" "$out"
  assert_not_contains "skill index carries a non-empty trigger description" '"description": ""' "$out"

  out=$(mcp_read_resource "skill://$skill/SKILL.md")
  assert_contains "SKILL.md is served as markdown" '"mimeType":"text/markdown"' "$out"
  assert_contains "SKILL.md body has frontmatter name" "name: $skill" "$out"
  assert_not_contains "SKILL.md is not the unedited template boilerplate" 'THIS FILE IS SERVED TO CLIENTS' "$out"

  for file in "$@"; do
    out=$(mcp_read_resource "skill://$skill/$file")
    assert_contains "supporting file $file resolves under the skill root" '"text":"' "$out"
    assert_not_contains "supporting file $file is not a not-found error" 'no resource at' "$out"
  done

  out=$(mcp_read_resource "skill://no-such-skill/SKILL.md")
  assert_contains "unknown skill URI is a clean resource-not-found" '-32602' "$out"
  assert_contains "unknown skill URI points the client at the catalog" 'read skill://index.json' "$out"
}

# guard_tests — Host-header guard, run against the guard instance.
guard_tests() {
  echo "== Host-header guard (MCP_ALLOWED_HOSTS) =="
  local out
  out=$(mcp_initialize "http://127.0.0.1:${GUARD_PORT}/")
  assert_contains "allowed Host accepted" '"protocolVersion"' "$out"
  out=$(printf '%s' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}' \
    | curl -sS --max-time 20 -X POST "http://127.0.0.1:${GUARD_PORT}/" -H 'Host: evil.example' -H "$CT" -H "$ACCEPT" -H "$PV" --data-binary @-)
  assert_contains "disallowed Host rejected" 'Forbidden' "$out"
}

# mcp_harness_report — print totals and exit non-zero on any failure.
mcp_harness_report() {
  echo
  echo "== results: ${PASS} passed, ${FAIL} failed =="
  if [ "$FAIL" -gt 0 ]; then
    printf 'failed: %s\n' "${FAILED_NAMES[@]}"
    exit 1
  fi
}
