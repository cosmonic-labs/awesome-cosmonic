#!/usr/bin/env bash
# Shared end-to-end test harness for MCP server examples.
#
# A per-server scripts/e2e.sh sources this file, then:
#   - sets WASM, PORT, GUARD_PORT (and optional FIXTURE_PORT) before sourcing,
#   - calls mcp_harness_start / mcp_harness_start_guard,
#   - runs framework_tests (protocol + spec + robustness, tool-agnostic),
#   - adds its own tool cases with mcp_call / assert_contains / assert_json,
#   - calls mcp_harness_report at the end.
#
# Requires: cargo (wasm32-wasip2 target) to build, wasmtime >= 46, curl,
# python3. Everything here is intentionally dependency-light.
set -u

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

# mcp_post <base-url> <extra curl args...> — POST stdin with MCP headers.
mcp_post() {
  local base="$1"; shift
  curl -sS --max-time 20 -X POST "$base" -H "$CT" -H "$ACCEPT" -H "$PV" "$@" --data-binary @-
}

# mcp_call <tool> <json-arguments> — call a tool on the primary server,
# echoing the SSE `data:` payload. Arguments must be a JSON object literal.
mcp_call() {
  local tool="$1" args="$2"
  printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args},${META}}}" \
    | mcp_post "http://127.0.0.1:${PORT}/" -H 'Mcp-Method: tools/call' -H "Mcp-Name: ${tool}"
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
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "http://127.0.0.1:${1}/" && return 0
    sleep 0.2
  done
}

# mcp_harness_start [extra --env KEY=VAL ...] — start the primary server.
mcp_harness_start() {
  echo "starting wasmtime serve on :${PORT}..."
  wasmtime serve -Sp3,cli,http "$@" --addr "127.0.0.1:${PORT}" "$WASM" \
    >/tmp/mcp-e2e-server.log 2>&1 &
  SERVER_PID=$!
  mcp_wait_ready "$PORT"
}

# mcp_harness_start_guard — start a second instance with MCP_ALLOWED_HOSTS set
# to its own port, to test the Host-header guard.
mcp_harness_start_guard() {
  echo "starting guard instance on :${GUARD_PORT}..."
  wasmtime serve -Sp3,cli,http --env "MCP_ALLOWED_HOSTS=127.0.0.1:${GUARD_PORT}" \
    "$@" --addr "127.0.0.1:${GUARD_PORT}" "$WASM" >/tmp/mcp-e2e-guard.log 2>&1 &
  GUARD_PID=$!
  mcp_wait_ready "$GUARD_PORT"
}

# framework_tests <tool1> <tool2> ... — tool-agnostic protocol, spec, and
# robustness checks. Pass the server's tool names to assert they are listed.
framework_tests() {
  local base="http://127.0.0.1:${PORT}/"

  echo "== protocol =="
  local out hdrs
  out=$(mcp_initialize "$base")
  assert_contains "initialize negotiates 2026-07-28" '"protocolVersion":"2026-07-28"' "$out"

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

  echo "== robustness =="
  local code
  code=$(python3 -c "import sys; sys.stdout.write('{\"padding\":\"' + 'x' * (5 * 1024 * 1024) + '\"}')" \
    | mcp_post "$base" -H 'Mcp-Method: tools/list' -o /dev/null -w '%{http_code}')
  case "$code" in
    413 | 400) pass "oversized body (5 MiB) rejected with $code" ;;
    *) fail "oversized body (5 MiB) rejected" "http status: $code" ;;
  esac

  local conc_fails=0 pids=() i
  for i in $(seq 1 8); do
    ( mcp_call "${FIRST_TOOL_NAME}" "${FIRST_TOOL_ARGS}" > "/tmp/mcp-e2e-conc-$i.json" ) &
    pids+=($!)
  done
  wait "${pids[@]}"
  for i in $(seq 1 8); do
    grep -q "${FIRST_TOOL_EXPECT}" "/tmp/mcp-e2e-conc-$i.json" || conc_fails=$((conc_fails + 1))
  done
  if [ "$conc_fails" -eq 0 ]; then
    pass "8 concurrent tool calls all succeed"
  else
    fail "8 concurrent tool calls all succeed" "$conc_fails of 8 failed"
  fi
}

# discovery_tests <skill-name> — the default route (GET / and /health).
#
# Every server built from the Cosmonic MCP template answers its root path with
# a JSON discovery document instead of a 404, and that route must not shadow
# the protocol: POST still has to reach the transport on the same path.
discovery_tests() {
  local skill="$1"
  local base="http://127.0.0.1:${PORT}"
  echo "== default route (discovery) =="

  local root health hdrs
  root=$(curl -sS --max-time 20 "${base}/")
  assert_contains "GET / returns the discovery document" '"status": "ok"' "$root"
  assert_contains "GET / names the MCP protocol" 'Model Context Protocol' "$root"
  assert_contains "GET / reports the spec version" '2026-07-28' "$root"
  assert_contains "GET / lists the tool names" '"tools"' "$root"
  assert_contains "GET / points at the skill index" 'skill://index.json' "$root"
  assert_contains "GET / advertises the skills extension" 'io.modelcontextprotocol/skills' "$root"
  assert_contains "GET / lists this server's skill" "$skill" "$root"

  health=$(curl -sS --max-time 20 "${base}/health")
  assert_contains "GET /health returns the same document" '"status": "ok"' "$health"

  hdrs=$(curl -sS --max-time 20 -D - -o /dev/null "${base}/")
  assert_contains "discovery document is JSON" 'application/json' "$hdrs"
  assert_contains "discovery document is not cached" 'no-store' "$hdrs"
  # No CORS anywhere on this route: a browser page must not be able to read
  # the server's identity, tool list, or bridge state cross-origin.
  assert_not_contains "discovery document sends no CORS header" 'access-control-allow-origin' "$hdrs"

  # The discovery route must not shadow the protocol.
  local out
  out=$(mcp_initialize "${base}/")
  assert_contains "POST / still reaches the MCP transport" '"protocolVersion"' "$out"
}

# skills_tests <skill-name> <supporting-file-path>... — Skills over MCP.
#
# The skills extension (io.modelcontextprotocol/skills) rides on the resources
# primitive: a catalog at skill://index.json, a SKILL.md per skill, and any
# supporting files reachable at paths relative to the skill root.
skills_tests() {
  local skill="$1"; shift
  local base="http://127.0.0.1:${PORT}/"
  echo "== skills over MCP (resources) =="

  local out file
  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"resources/list\",\"params\":{$META}}" \
    | mcp_post "$base" -H 'Mcp-Method: resources/list')
  assert_contains "resources/list contains the skill catalog" 'skill://index.json' "$out"
  assert_contains "resources/list contains the SKILL.md" "skill://${skill}/SKILL.md" "$out"
  for file in "$@"; do
    assert_contains "resources/list contains ${file}" "skill://${skill}/${file}" "$out"
  done

  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"resources/templates/list\",\"params\":{$META}}" \
    | mcp_post "$base" -H 'Mcp-Method: resources/templates/list')
  assert_contains "resources/templates/list exposes the skill URI template" 'skill://{skill}/SKILL.md' "$out"

  # skill://index.json — the lightweight catalog a client reads at session start.
  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"resources/read\",\"params\":{\"uri\":\"skill://index.json\",$META}}" \
    | mcp_post "$base" -H 'Mcp-Method: resources/read' -H 'Mcp-Name: skill://index.json')
  assert_contains "skill index declares the skills extension" 'io.modelcontextprotocol/skills' "$out"
  assert_contains "skill index lists this server's skill" "$skill" "$out"
  # The catalog's description is the frontmatter trigger text, read at runtime
  # from the embedded SKILL.md — an empty one means the parse silently failed.
  assert_not_contains "skill index carries a trigger description" '"description": ""' "$out"

  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":23,\"method\":\"resources/read\",\"params\":{\"uri\":\"skill://${skill}/SKILL.md\",$META}}" \
    | mcp_post "$base" -H 'Mcp-Method: resources/read' -H "Mcp-Name: skill://${skill}/SKILL.md")
  assert_contains "SKILL.md is served" 'text/markdown' "$out"

  # Relative links inside a SKILL.md resolve against the skill root.
  for file in "$@"; do
    out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":24,\"method\":\"resources/read\",\"params\":{\"uri\":\"skill://${skill}/${file}\",$META}}" \
      | mcp_post "$base" -H 'Mcp-Method: resources/read' -H "Mcp-Name: skill://${skill}/${file}")
    assert_contains "${file} resolves under the skill root" 'text/markdown' "$out"
  done

  out=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":25,\"method\":\"resources/read\",\"params\":{\"uri\":\"skill://no-such-skill/SKILL.md\",$META}}" \
    | mcp_post "$base" -H 'Mcp-Method: resources/read' -H 'Mcp-Name: skill://no-such-skill/SKILL.md')
  assert_not_contains "unknown skill URI is not served" '"contents"' "$out"
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
