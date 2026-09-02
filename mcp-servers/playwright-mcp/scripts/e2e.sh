#!/usr/bin/env bash
# End-to-end tests for playwright-mcp. Framework checks (protocol, spec
# enforcement, discovery route, skills over MCP, robustness, Host guard) come
# from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases live
# below.
#
# Hermetic: scripts/playwright_fixture.py impersonates `@playwright/mcp`
# 0.0.80 in streamable-HTTP mode on 127.0.0.1:FIXTURE_PORT (sessions, SSE
# frames, the upstream's status codes and result sections) and
# PLAYWRIGHT_BASE_URL points every instance at it. Five wasmtime instances run:
#   :PORT         token + short outbound timeout             — most cases
#   :GUARD_PORT   no token (missing-secret path), default gates, Host guard
#   :UNSAFE_PORT  PLAYWRIGHT_ALLOW_UNSAFE=true, PLAYWRIGHT_AUTO_SNAPSHOT=false
#   :DEAD_PORT    PLAYWRIGHT_BASE_URL at the Desktop loopback sentinel, which
#                 does not resolve here — the "upstream unreachable" path
#   :STATE_PORT   same env as :PORT but only ever called sequentially — wasmtime
#                 serve (p3) grows a pool of instances under the harness's
#                 8-way concurrency test and reuses them afterwards, so
#                 per-instance state (session id, initialize count, tool cache)
#                 is only deterministic on an instance that never saw
#                 concurrency. Desktop runs poolSize 1.
#
# Usage: scripts/e2e.sh [--no-build]
#   E2E_LIVE=1 also launches the real `npx -y @playwright/mcp@0.0.80` (headless
#   chromium) plus a local test page and drives navigate -> snapshot -> type ->
#   click -> wait_for -> screenshot -> close through a fifth instance.
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9238}
GUARD_PORT=${GUARD_PORT:-9239}
FIXTURE_PORT=${FIXTURE_PORT:-9240}
UNSAFE_PORT=${UNSAFE_PORT:-9241}
DEAD_PORT=${DEAD_PORT:-9242}
STATE_PORT=${STATE_PORT:-9246}
LIVE_PORT=${LIVE_PORT:-9243}
LIVE_PAGE_PORT=${LIVE_PAGE_PORT:-9244}
LIVE_INST_PORT=${LIVE_INST_PORT:-9245}
WASM=${WASM:-target/wasm32-wasip2/release/playwright_mcp.wasm}
SKILL_NAME=playwright-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIXTURE_URL="http://127.0.0.1:${FIXTURE_PORT}"
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"
UNSAFE_BASE="http://127.0.0.1:${UNSAFE_PORT}/"
DEAD_BASE="http://127.0.0.1:${DEAD_PORT}/"
STATE_BASE="http://127.0.0.1:${STATE_PORT}/"
LIVE_BASE="http://127.0.0.1:${LIVE_INST_PORT}/"
UNSAFE_PID=""
DEAD_PID=""
STATE_PID=""
LIVE_PID=""
LIVE_PAGE_PID=""
LIVE_INST_PID=""

cleanup_all() {
  for pid in "$UNSAFE_PID" "$DEAD_PID" "$STATE_PID" "$LIVE_INST_PID" "$LIVE_PAGE_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
  done
  if [ -n "$LIVE_PID" ]; then
    # npx spawns node as a child; take the whole process group down.
    kill -- -"$LIVE_PID" 2>/dev/null || kill "$LIVE_PID" 2>/dev/null
  fi
  mcp_harness_cleanup
}
trap cleanup_all EXIT

# assert_stats <name> <python-expr> — evaluates <expr> against the fixture's
# /__fixture/stats with `stats`, `calls` (tools/call records, oldest first),
# `last`/`prev` (newest records), `reqs` (raw POST/DELETE records) and
# `expect` (the JSON in $E2E_TMP/expect.json, if present) bound.
assert_stats() {
  local name="$1" expr="$2"
  if curl -sS "${FIXTURE_URL}/__fixture/stats" | EXPECT_FILE="$E2E_TMP/expect.json" python3 -c '
import json, os, sys
stats = json.load(sys.stdin)
calls = stats["calls"]; reqs = stats["requests"]
last = calls[-1] if calls else {}
prev = calls[-2] if len(calls) > 1 else {}
expect = json.load(open(os.environ["EXPECT_FILE"])) if os.path.exists(os.environ["EXPECT_FILE"]) else None
sys.exit(0 if eval(sys.argv[1]) else 1)
' "$expr" 2>/dev/null; then
    pass "$name"
  else
    fail "$name" "expression [$expr] false; last call: $(curl -sS "${FIXTURE_URL}/__fixture/stats" | python3 -c 'import json,sys; s=json.load(sys.stdin); c=s["calls"]; print(json.dumps(c[-1], ensure_ascii=False)[:400] if c else "none")')"
  fi
}

# stat <python-expr> — prints one value from the fixture stats (`s` bound).
stat() {
  curl -sS "${FIXTURE_URL}/__fixture/stats" | python3 -c "import json,sys; s=json.load(sys.stdin); print($1)"
}

# assert_failed <name> <out> — a tool-level error (isError) or a JSON-RPC error.
assert_failed() {
  case "$2" in
    *'"isError":true'* | *'"error":{'*) pass "$1" ;;
    *) fail "$1" "expected a failure, got: $2" ;;
  esac
}

fx_post() { curl -sS -X POST "${FIXTURE_URL}$1" -H 'Content-Type: application/json' -d "${2:-}" >/dev/null; }
tools_list_on() {
  printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{$META}}" | mcp_post "$1" -H 'Mcp-Method: tools/list'
}

# The concurrency test in framework_tests fires this tool 8x in parallel.
FIRST_TOOL_NAME=playwright_status
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"status":"ok"'

mcp_build_if_needed "${1:-}"

echo "starting fixture on :${FIXTURE_PORT}..."
python3 scripts/playwright_fixture.py "$FIXTURE_PORT" >"$E2E_TMP/fixture.log" 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "${FIXTURE_URL}/" && break
  sleep 0.2
done

# The short outbound timeout keeps the slow-upstream case fast (fixture sleeps 5 s).
COMMON=(--env "PLAYWRIGHT_BASE_URL=${FIXTURE_URL}/mcp" --env RUST_LOG=info --env MCP_OUTBOUND_TIMEOUT_MS=3000)
mcp_harness_start "${COMMON[@]}" --env PLAYWRIGHT_BEARER_TOKEN=test-token
# Guard instance: no PLAYWRIGHT_BEARER_TOKEN (missing-secret path), default gates.
mcp_harness_start_guard "${COMMON[@]}"
echo "starting unsafe/no-auto-snapshot instance on :${UNSAFE_PORT}..."
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env PLAYWRIGHT_ALLOW_UNSAFE=true \
  --env PLAYWRIGHT_AUTO_SNAPSHOT=false --addr "127.0.0.1:${UNSAFE_PORT}" "$WASM" \
  >"$E2E_TMP/unsafe.log" 2>&1 &
UNSAFE_PID=$!
echo "starting dead-upstream instance on :${DEAD_PORT}..."
"$WASMTIME" serve -Sp3,cli,http --env RUST_LOG=info --env MCP_OUTBOUND_TIMEOUT_MS=3000 \
  --addr "127.0.0.1:${DEAD_PORT}" "$WASM" >"$E2E_TMP/dead.log" 2>&1 &
DEAD_PID=$!
echo "starting sequential-only (stateful) instance on :${STATE_PORT}..."
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env PLAYWRIGHT_BEARER_TOKEN=test-token \
  --addr "127.0.0.1:${STATE_PORT}" "$WASM" >"$E2E_TMP/state.log" 2>&1 &
STATE_PID=$!
mcp_wait_ready "$UNSAFE_PORT"
mcp_wait_ready "$DEAD_PORT"
mcp_wait_ready "$STATE_PORT"

ALL_TOOLS=(playwright_status playwright_reset_session browser_navigate browser_snapshot
  browser_click browser_type browser_take_screenshot browser_wait_for browser_console_messages
  browser_resize browser_tabs browser_handle_dialog browser_close)

framework_tests "${ALL_TOOLS[@]}"
discovery_tests playwright_status
skills_tests "$SKILL_NAME" references/TOOLS.md references/SETUP.md

echo "== discovery: credentials block =="
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / lists the optional playwright-mcp-token credential" '"ref": "playwright-mcp-token"' "$ROOT"
assert_contains "GET / names the env var" '"env": "PLAYWRIGHT_BEARER_TOKEN"' "$ROOT"
assert_contains "GET / marks the credential optional" '"required": false' "$ROOT"
assert_contains "GET / reports the token as configured (presence only)" '"status": "configured"' "$ROOT"
assert_contains "GET / names playwright_status as the validator" '"validate": "playwright_status"' "$ROOT"
assert_not_contains "GET / never leaks the token value" 'test-token' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$STATE_BASE")
assert_not_contains "GET / before any upstream contact lists local tools only" '"browser_navigate"' "$ROOT"
tools_list_on "$STATE_BASE" >/dev/null
ROOT=$(curl -sS --max-time 20 "$STATE_BASE")
assert_contains "GET / lists cached upstream tools once the upstream answered" '"browser_navigate"' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD_BASE")
assert_contains "GET / on the guard instance reports the token missing" '"status": "missing"' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$DEAD_BASE")
assert_contains "GET / on the dead-upstream instance still lists playwright_status" '"playwright_status"' "$ROOT"
assert_not_contains "GET / on the dead-upstream instance lists no browser tools" '"browser_navigate"' "$ROOT"

echo "== tools/list passthrough =="
OUT=$(tools_list_on "$MCP_BASE")
assert_contains "upstream tools are listed" '"name":"browser_navigate"' "$OUT"
assert_not_contains "browser_run_code_unsafe is hidden by default" '"name":"browser_run_code_unsafe"' "$OUT"
assert_not_contains "browser_evaluate is hidden by default" '"name":"browser_evaluate"' "$OUT"
assert_not_contains "filename is stripped from every schema" '"filename":' "$OUT"
assert_contains "upstream annotations pass through" '"destructiveHint":true' "$OUT"
assert_contains "screenshot description carries the proxy note" 'defaults to jpeg' "$OUT"
assert_contains "local tools carry annotations" '"title":"Playwright upstream status"' "$OUT"
OUT=$(tools_list_on "$UNSAFE_BASE")
assert_contains "PLAYWRIGHT_ALLOW_UNSAFE=true lists browser_run_code_unsafe" '"name":"browser_run_code_unsafe"' "$OUT"
assert_contains "PLAYWRIGHT_ALLOW_UNSAFE=true lists browser_evaluate" '"name":"browser_evaluate"' "$OUT"
assert_not_contains "filename is stripped on the unsafe instance too" '"filename":' "$OUT"
OUT=$(tools_list_on "$DEAD_BASE")
assert_contains "unreachable upstream still lists playwright_status" '"name":"playwright_status"' "$OUT"
assert_contains "unreachable upstream still lists playwright_reset_session" '"name":"playwright_reset_session"' "$OUT"
assert_not_contains "unreachable upstream lists no browser tools" '"name":"browser_navigate"' "$OUT"

echo "== playwright_status =="
OUT=$(mcp_call playwright_status '{}')
assert_json "status ok + reachable" 'r["result"]["structuredContent"]["status"] == "ok" and r["result"]["structuredContent"]["reachable"] is True' "$OUT"
assert_json "status reports upstream identity + protocol" 'r["result"]["structuredContent"]["upstream"] == {"name": "Playwright", "version": "fixture", "protocolVersion": "2025-11-25"}' "$OUT"
assert_json "status reports the cached session and tool list" 'r["result"]["structuredContent"]["session"]["cached"] is True and r["result"]["structuredContent"]["tools"]["cached"] >= 10' "$OUT"
assert_json "status reports the token and the credential ref" 'r["result"]["structuredContent"]["token_configured"] is True and r["result"]["structuredContent"]["credential"]["ref"] == "playwright-mcp-token"' "$OUT"
assert_json "status carries the launch command" '"--allowed-hosts host.wasmcloud.internal:" in r["result"]["structuredContent"]["launch_command"]' "$OUT"
assert_contains "status has a readable text block" 'Playwright MCP upstream OK' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" playwright_status '{}')
assert_json "guard status ok without a token (upstream needs none)" 'r["result"]["structuredContent"]["status"] == "ok" and r["result"]["structuredContent"]["token_configured"] is False' "$OUT"
OUT=$(mcp_call_on "$UNSAFE_BASE" playwright_status '{}')
assert_json "status hints name the unsafe gate and auto-snapshot off" 'any("PLAYWRIGHT_ALLOW_UNSAFE=true" in h for h in r["result"]["structuredContent"]["hints"]) and any("PLAYWRIGHT_AUTO_SNAPSHOT=false" in h for h in r["result"]["structuredContent"]["hints"])' "$OUT"

echo "== session lifecycle (sequential-only instance) =="
mcp_call_on "$STATE_BASE" browser_snapshot '{}' >/dev/null
INIT0=$(stat 's["initialize_count"]')
for _ in 1 2 3 4 5; do mcp_call_on "$STATE_BASE" browser_snapshot '{}' >/dev/null; done
INIT1=$(stat 's["initialize_count"]')
if [ "$INIT0" = "$INIT1" ]; then pass "one upstream session serves many calls (no re-initialize)"; else fail "one upstream session serves many calls (no re-initialize)" "initialize_count $INIT0 -> $INIT1"; fi
assert_stats "every call carries the same Mcp-Session-Id" 'len({c["session"] for c in calls[-5:]}) == 1 and all(c["session"].startswith("fx-") for c in calls[-5:])'
assert_stats "every call carries the negotiated MCP-Protocol-Version" 'all(c["protocol_version"] == "2025-11-25" for c in calls[-5:])'
assert_stats "tools/call params carry only name + arguments (no client _meta forwarded)" 'all(c["params_keys"] == ["arguments", "name"] for c in calls[-5:])'
assert_stats "every POST accepts both application/json and text/event-stream" 'all("application/json" in r["accept"] and "text/event-stream" in r["accept"] for r in reqs if r["method"] == "POST")'
assert_stats "every POST is application/json with a proxy User-Agent" 'all(r["content_type"] == "application/json" and r["user_agent"].startswith("playwright-mcp-proxy/") for r in reqs if r["method"] == "POST")'
assert_stats "Authorization: Bearer is sent on the primary instance" 'last["authorization"] == "Bearer test-token"'
fx_post /__fixture/expire
OUT=$(mcp_call_on "$STATE_BASE" browser_snapshot '{}')
assert_contains "lost upstream session: call still succeeds (transparent re-initialize)" '[ref=e' "$OUT"
INIT2=$(stat 's["initialize_count"]')
if [ "$INIT2" = "$((INIT1 + 1))" ]; then pass "lost session triggered exactly one re-initialize"; else fail "lost session triggered exactly one re-initialize" "initialize_count $INIT1 -> $INIT2"; fi
mcp_call_on "$STATE_BASE" browser_snapshot '{}' >/dev/null
INIT3=$(stat 's["initialize_count"]')
if [ "$INIT3" = "$INIT2" ]; then pass "re-initialized session is cached again"; else fail "re-initialized session is cached again" "initialize_count $INIT2 -> $INIT3"; fi

echo "== auto-snapshot (PLAYWRIGHT_AUTO_SNAPSHOT) =="
OUT=$(mcp_call browser_navigate '{"url":"http://127.0.0.1:9351/"}')
assert_contains "navigate result carries the inline yaml snapshot" '```yaml' "$OUT"
assert_contains "inline snapshot carries element refs" '[ref=e' "$OUT"
assert_not_contains "the file-only snapshot link is gone" '[Snapshot](' "$OUT"
assert_contains "the ### Page section is preserved" '### Page' "$OUT"
assert_contains "the ### Events section is preserved" '### Events' "$OUT"
assert_contains "the ### Ran Playwright code section is preserved" 'await page.goto' "$OUT"
assert_stats "navigate was followed by exactly one browser_snapshot" 'prev["name"] == "browser_navigate" and last["name"] == "browser_snapshot" and last["arguments"] == {}'
OUT=$(mcp_call browser_click '{"target":"e-dialog"}')
assert_contains "### Modal state survives the merge" '### Modal state' "$OUT"
assert_contains "modal result still gets the inline snapshot" '```yaml' "$OUT"
OUT=$(mcp_call browser_type '{"target":"e3","text":"Liam"}')
assert_stats "a result without a snapshot link triggers no extra round-trip" 'last["name"] == "browser_type"'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "browser_snapshot itself returns the yaml" '```yaml' "$OUT"
assert_stats "browser_snapshot is never doubled" 'last["name"] == "browser_snapshot" and prev["name"] == "browser_type"'
OUT=$(mcp_call_on "$UNSAFE_BASE" browser_navigate '{"url":"http://127.0.0.1:9351/"}')
assert_contains "PLAYWRIGHT_AUTO_SNAPSHOT=false keeps the file link" '[Snapshot](' "$OUT"
assert_not_contains "PLAYWRIGHT_AUTO_SNAPSHOT=false inlines nothing" '```yaml' "$OUT"
fx_post /__fixture/mode '{"snapshot_error":true}'
OUT=$(mcp_call browser_navigate '{"url":"http://127.0.0.1:9351/"}')
assert_contains "a failing follow-up snapshot keeps the action result" 'await page.goto' "$OUT"
assert_contains "a failing follow-up snapshot keeps the link" '[Snapshot](' "$OUT"
assert_contains "a failing follow-up snapshot is reported in the proxy notes" 'browser_snapshot after the action failed' "$OUT"
assert_contains "a failing follow-up snapshot does not fail the action" '"isError":false' "$OUT"
fx_post /__fixture/mode '{"snapshot_error":false}'

echo "== browser_take_screenshot =="
OUT=$(mcp_call browser_take_screenshot '{}')
assert_stats "screenshot type defaults to jpeg" 'last["name"] == "browser_take_screenshot" and last["arguments"].get("type") == "jpeg"'
assert_contains "screenshot returns an image content block" '"type":"image"' "$OUT"
assert_contains "screenshot image is jpeg" '"mimeType":"image/jpeg"' "$OUT"
OUT=$(mcp_call browser_take_screenshot '{"type":"png","scale":"device"}')
assert_stats "explicit png type is kept" 'last["arguments"] == {"type": "png", "scale": "device"}'
assert_contains "explicit png comes back as image/png" '"mimeType":"image/png"' "$OUT"
OUT=$(mcp_call browser_take_screenshot '{"fullPage":true,"filename":"shot.png"}')
assert_stats "filename is stripped before forwarding" '"filename" not in last["arguments"] and last["arguments"]["fullPage"] is True'
assert_contains "stripped filename is reported in the proxy notes" '`filename` was dropped' "$OUT"
assert_contains "fullPage warns about the reply cap" 'MCP_OUTBOUND_MAX_BYTES' "$OUT"

echo "== browser_wait_for =="
OUT=$(mcp_call browser_wait_for '{"time":900}')
assert_stats "wait time 900 is clamped to 60" 'prev["name"] == "browser_wait_for" and prev["arguments"] == {"time": 60.0}'
assert_contains "clamp is reported in the proxy notes" '`time` clamped from 900 to 60' "$OUT"
assert_contains "wait_for result gets the inline snapshot too" '```yaml' "$OUT"
OUT=$(mcp_call browser_wait_for '{"time":-5}')
assert_stats "negative wait time is clamped to 0" 'prev["arguments"] == {"time": 0.0}'
OUT=$(mcp_call browser_wait_for '{"time":2.5}')
assert_stats "in-range wait time is untouched" 'prev["arguments"] == {"time": 2.5}'
assert_not_contains "no proxy notes when nothing changed" 'Proxy notes' "$OUT"
OUT=$(mcp_call browser_wait_for '{}')
assert_contains "empty wait_for error passes through verbatim" 'Either time, text or textGone must be provided' "$OUT"
assert_contains "empty wait_for is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call browser_wait_for '{"text":"clicked"}')
assert_contains "wait_for text passes through" 'Waited for clicked' "$OUT"
OUT=$(mcp_call browser_wait_for '{"time":"abc"}')
assert_stats "non-numeric time is forwarded for the upstream to validate" 'prev["arguments"] == {"time": "abc"}'

echo "== browser_click / stale refs / argument names =="
OUT=$(mcp_call browser_click '{"target":"e4"}')
assert_contains "click by ref succeeds with an inline snapshot" '```yaml' "$OUT"
OUT=$(mcp_call browser_click '{"target":"e99"}')
assert_contains "stale ref error passes through verbatim" 'Ref e99 not found in the current page snapshot. Try capturing new snapshot.' "$OUT"
assert_contains "stale ref is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call browser_click '{"ref":"e4"}')
assert_contains "ref-instead-of-target zod error passes through" 'at target' "$OUT"
assert_contains "zod error is a tool error" '"isError":true' "$OUT"

echo "== encoding / adversarial inputs =="
python3 -c 'import json; print(json.dumps({"url": "http://例え.jp/?q=<script>alert(1)</script>&x=\"'"'"'\r\n \\end"}))' > "$E2E_TMP/args.json"
python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["url"]))' "$E2E_TMP/args.json" > "$E2E_TMP/expect.json"
OUT=$(mcp_call browser_navigate "$(cat "$E2E_TMP/args.json")")
assert_contains "unicode + injection-shaped url is forwarded (fixture navigated)" 'await page.goto' "$OUT"
assert_stats "unicode + injection-shaped url round-trips byte-exact" 'prev["name"] == "browser_navigate" and prev["arguments"]["url"] == expect'
python3 -c 'import json; print(json.dumps({"target": "e3", "text": "é" * 50000 + "x" * 50000}))' > "$E2E_TMP/args.json"
python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["text"]))' "$E2E_TMP/args.json" > "$E2E_TMP/expect.json"
OUT=$(mcp_call browser_type "$(cat "$E2E_TMP/args.json")")
assert_contains "100k-char multibyte argument is accepted" '"isError":false' "$OUT"
assert_stats "100k-char multibyte argument round-trips byte-exact" 'last["name"] == "browser_type" and last["arguments"]["text"] == expect and len(last["arguments"]["text"]) == 100000'
rm -f "$E2E_TMP/expect.json"
OUT=$(mcp_call browser_navigate '{"url":"http://127.0.0.1:1/unsafe-port"}')
assert_contains "browser-side net:: errors pass through" 'net::ERR_UNSAFE_PORT' "$OUT"
assert_not_contains "ANSI escape codes are stripped from upstream errors" '\u001b' "$OUT"
assert_contains "the Playwright call log survives ANSI stripping" 'navigating to' "$OUT"
assert_contains "net:: error is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call browser_navigate '{"url":"file:///etc/passwd"}')
assert_contains "file: navigation error passes through" 'protocol is blocked' "$OUT"
OUT=$(mcp_call browser_snapshot '{"depth":1000}')
assert_stats "snapshot depth 1000 is clamped to 64" 'last["arguments"] == {"depth": 64.0}'
OUT=$(mcp_call browser_snapshot '{"depth":0,"filename":"../../etc/x.yml"}')
assert_stats "snapshot depth 0 is clamped to 1 and the traversal-shaped filename is dropped" 'last["arguments"] == {"depth": 1.0}'
OUT=$(mcp_call browser_resize '{"width":99999,"height":-1}')
assert_stats "resize is clamped to 1..8192" 'last["arguments"] == {"width": 8192.0, "height": 1.0}'
OUT=$(mcp_call browser_tabs '{"action":"select","index":-5}')
assert_stats "negative tab index is clamped to 0" 'last["arguments"] == {"action": "select", "index": 0.0}'
OUT=$(mcp_call browser_console_messages '{"level":"error","filename":"/tmp/console.log"}')
assert_stats "console_messages filename is stripped" 'last["arguments"] == {"level": "error"}'
assert_contains "console_messages echo shows no filename" '{\"arguments\": {\"level\": \"error\"}' "$OUT"
OUT=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/call\",\"params\":{\"name\":\"browser_click\",\"arguments\":\"e4\",$META}}" | mcp_post "$MCP_BASE" -H 'Mcp-Method: tools/call' -H 'Mcp-Name: browser_click')
assert_contains "string arguments are a clean JSON-RPC error" '"error":{' "$OUT"
OUT=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/call\",\"params\":{\"name\":\"browser_click\",\"arguments\":[],$META}}" | mcp_post "$MCP_BASE" -H 'Mcp-Method: tools/call' -H 'Mcp-Name: browser_click')
assert_contains "array arguments are a clean JSON-RPC error" '"error":{' "$OUT"
OUT=$(mcp_call browser_close '{}')
assert_contains "browser_close passes through" 'No open tabs. Navigate to a URL to create one.' "$OUT"
OUT=$(mcp_call browser_handle_dialog '{"accept":true,"promptText":"x"}')
assert_contains "handle_dialog is forwarded and snapshotted" '```yaml' "$OUT"
OUT=$(mcp_call playwright_status '{"unexpected":"argument"}')
assert_json "local tools ignore unexpected arguments" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"

echo "== upstream error mapping =="
fx_post /__fixture/mode '{"host_check":"localhost:8931"}'
OUT=$(mcp_call browser_navigate '{"url":"http://127.0.0.1:9351/"}')
assert_contains "403 Host allow-list maps to the --allowed-hosts hint" 'Access is only allowed at localhost:8931' "$OUT"
assert_contains "hint names the exact flag" "--allowed-hosts host.wasmcloud.internal:${FIXTURE_PORT},localhost:${FIXTURE_PORT}" "$OUT"
assert_contains "hint warns about the comma form" 'comma-separated' "$OUT"
assert_contains "host rejection is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call playwright_status '{}')
assert_json "status reports host_rejected" 'r["result"]["structuredContent"]["status"] == "host_rejected" and "--allowed-hosts" in r["result"]["structuredContent"]["remediation"]' "$OUT"
fx_post /__fixture/mode '{"host_check":null}'
fx_post /__fixture/mode '{"fail_status":429,"fail_body":"slow down","retry_after":"7"}'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "429 maps to the rate-limit hint" 'rate limiting' "$OUT"
assert_contains "429 keeps the upstream message" 'HTTP 429: slow down' "$OUT"
assert_contains "429 reports Retry-After" 'Retry-After: 7' "$OUT"
fx_post /__fixture/mode '{"fail_status":500,"fail_body":"boom"}'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "500 keeps the upstream message" 'HTTP 500: boom' "$OUT"
assert_contains "500 suggests one retry" 'retry once' "$OUT"
fx_post /__fixture/mode '{"fail_status":503}'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "503 is reported with its status" 'HTTP 503' "$OUT"
fx_post /__fixture/mode '{"fail_status":404,"fail_body":"Not Found"}'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "non-session 404 is a plain HTTP error" 'HTTP 404: Not Found' "$OUT"
fx_post /__fixture/mode '{"fail_status":406,"fail_body":"{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32000,\"message\":\"Not Acceptable: Client must accept both application/json and text/event-stream\"},\"id\":null}"}'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "406 names the Accept header bug" 'Client must accept both application/json and text/event-stream' "$OUT"
assert_contains "406 is flagged as a proxy bug" 'proxy bug' "$OUT"
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "server alive after mapped upstream errors" '```yaml' "$OUT"
OUT=$(mcp_call browser_navigate '{"url":"http://127.0.0.1:9351/slow"}')
assert_contains "black-holed upstream times out instead of wedging" 'timed out' "$OUT"
assert_contains "timeout is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call playwright_status '{}')
assert_json "server alive after the outbound timeout" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"

echo "== unreachable upstream (dead instance) =="
OUT=$(mcp_call_on "$DEAD_BASE" playwright_status '{}')
assert_json "dead instance reports unreachable" 'r["result"]["structuredContent"]["status"] == "unreachable" and r["result"]["structuredContent"]["reachable"] is False and r["result"]["structuredContent"]["via_loopback"] is True' "$OUT"
assert_contains "unreachable hint names allowedHosts" 'allowedHosts [\"host.wasmcloud.internal:8931\"]' "$OUT"
assert_contains "unreachable hint names allowedHostLoopbackPorts" 'allowedHostLoopbackPorts [\"8931\"]' "$OUT"
assert_contains "unreachable hint names the Settings -> Security door" 'allow host loopback' "$OUT"
assert_contains "unreachable hint gives the launch command with --host 127.0.0.1" 'npx @playwright/mcp@latest --port 8931 --host 127.0.0.1' "$OUT"
assert_contains "unreachable status is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$DEAD_BASE" browser_navigate '{"url":"http://example.com/"}')
assert_contains "browser tool on a dead upstream is an actionable tool error" 'Could not reach the Playwright MCP server at http://host.wasmcloud.internal:8931/mcp' "$OUT"
OUT=$(mcp_call_on "$DEAD_BASE" playwright_reset_session '{}')
assert_json "reset with nothing cached says so" 'r["result"]["structuredContent"]["status"] == "nothing_to_reset" and r["result"]["structuredContent"]["had_session"] is False' "$OUT"

echo "== bearer token: missing (guard) and invalid (rotated) =="
fx_post /__fixture/mode '{"require_token":"test-token"}'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "primary instance passes the token check" '```yaml' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" browser_navigate '{"url":"http://127.0.0.1:9351/"}')
assert_contains "missing PLAYWRIGHT_BEARER_TOKEN is an actionable tool error" 'PLAYWRIGHT_BEARER_TOKEN is not set' "$OUT"
assert_contains "missing-token error names the secret ref" 'playwright-mcp-token' "$OUT"
assert_contains "missing-token error says where the token comes from" 'hosted Playwright MCP endpoint behind an authenticating proxy' "$OUT"
assert_contains "missing-token error names cosmonic_set_secret" 'cosmonic_set_secret name=playwright-mcp-token' "$OUT"
assert_contains "missing-token error keeps the upstream message" 'HTTP 401' "$OUT"
assert_contains "missing-token error is a tool error (isError)" '"isError":true' "$OUT"
assert_stats "guard instance sends no Authorization header" 'reqs[-1]["authorization"] is None and reqs[-1]["method"] == "POST"'
OUT=$(mcp_call_on "$GUARD_BASE" playwright_status '{}')
assert_json "guard status reports missing with the remediation" 'r["result"]["structuredContent"]["status"] == "missing" and "playwright-mcp-token" in r["result"]["structuredContent"]["remediation"]' "$OUT"
fx_post /__fixture/mode '{"require_token":"rotated"}'
OUT=$(mcp_call browser_snapshot '{}')
assert_contains "401 with a token maps to the invalid-token hint" 'rejected the bearer token' "$OUT"
assert_contains "invalid-token error keeps the upstream message" 'HTTP 401' "$OUT"
assert_contains "invalid-token error names the secret ref" 'playwright-mcp-token' "$OUT"
assert_not_contains "invalid-token error never echoes the token" 'test-token' "$OUT"
OUT=$(mcp_call playwright_status '{}')
assert_json "status reports invalid on a rejected token" 'r["result"]["structuredContent"]["status"] == "invalid"' "$OUT"
fx_post /__fixture/mode '{"require_token":null}'

echo "== gated tools (PLAYWRIGHT_ALLOW_UNSAFE) =="
CALLS0=$(stat 'len(s["calls"])')
OUT=$(mcp_call_on "$GUARD_BASE" browser_evaluate '{"function":"() => document.title"}')
assert_contains "browser_evaluate is refused by default" 'browser_evaluate is disabled: set PLAYWRIGHT_ALLOW_UNSAFE=true' "$OUT"
assert_contains "gated refusal is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call browser_run_code_unsafe '{"code":"async (page) => 1"}')
assert_contains "browser_run_code_unsafe is refused by default" 'browser_run_code_unsafe is disabled' "$OUT"
OUT=$(mcp_call browser_file_upload '{"paths":["/etc/passwd"]}')
assert_contains "browser_file_upload is refused by default" 'browser_file_upload is disabled' "$OUT"
CALLS1=$(stat 'len(s["calls"])')
if [ "$CALLS0" = "$CALLS1" ]; then pass "gated refusals never reach the upstream"; else fail "gated refusals never reach the upstream" "calls $CALLS0 -> $CALLS1"; fi
OUT=$(mcp_call_on "$UNSAFE_BASE" browser_evaluate '{"function":"() => document.title"}')
assert_contains "PLAYWRIGHT_ALLOW_UNSAFE=true forwards browser_evaluate" '\"name\": \"browser_evaluate\"' "$OUT"
assert_contains "forwarded browser_evaluate succeeds" '"isError":false' "$OUT"

echo "== playwright_reset_session (sequential-only instance) =="
mcp_call_on "$STATE_BASE" browser_snapshot '{}' >/dev/null
DEL0=$(stat 's["delete_count"]')
INIT4=$(stat 's["initialize_count"]')
OUT=$(mcp_call_on "$STATE_BASE" playwright_reset_session '{}')
assert_json "reset closes the cached session" 'r["result"]["structuredContent"]["status"] == "reset" and r["result"]["structuredContent"]["had_session"] is True and r["result"]["structuredContent"]["upstream_status"] == 200' "$OUT"
DEL1=$(stat 's["delete_count"]')
if [ "$DEL1" = "$((DEL0 + 1))" ]; then pass "reset issued one upstream DELETE"; else fail "reset issued one upstream DELETE" "delete_count $DEL0 -> $DEL1"; fi
assert_stats "DELETE carried the session id" 'reqs[-1]["method"] == "DELETE" and reqs[-1]["session"].startswith("fx-")'
ROOT=$(curl -sS --max-time 20 "$STATE_BASE")
assert_not_contains "reset also drops the cached tool list (GET / shows local tools only)" '"browser_navigate"' "$ROOT"
OUT=$(mcp_call_on "$STATE_BASE" browser_snapshot '{}')
assert_contains "next call after reset succeeds" '```yaml' "$OUT"
INIT5=$(stat 's["initialize_count"]')
if [ "$INIT5" = "$((INIT4 + 1))" ]; then pass "next call after reset re-initializes once"; else fail "next call after reset re-initializes once" "initialize_count $INIT4 -> $INIT5"; fi
ROOT=$(curl -sS --max-time 20 "$STATE_BASE")
assert_contains "tool list is cached again after the first call" '"browser_navigate"' "$ROOT"

if [ "${E2E_LIVE:-0}" = "1" ]; then
  echo "== live: real @playwright/mcp 0.0.80 (E2E_LIVE=1) =="
  if ! command -v npx >/dev/null 2>&1; then
    echo "  skip - npx not installed"
  else
    mkdir -p "$E2E_TMP/page" "$E2E_TMP/out"
    cat > "$E2E_TMP/page/index.html" <<'HTML'
<!doctype html><html><head><title>Live Page</title></head><body>
<h1>Hello Playwright</h1>
<input id="name" placeholder="Your name">
<button id="go" onclick="document.getElementById('out').textContent='clicked '+document.getElementById('name').value">Go</button>
<p id="out"></p>
</body></html>
HTML
    python3 -m http.server "$LIVE_PAGE_PORT" --bind 127.0.0.1 --directory "$E2E_TMP/page" >"$E2E_TMP/page.log" 2>&1 &
    LIVE_PAGE_PID=$!
    setsid npx -y @playwright/mcp@0.0.80 --port "$LIVE_PORT" --host 127.0.0.1 --headless --isolated \
      --allowed-hosts "127.0.0.1:${LIVE_PORT}" --output-dir "$E2E_TMP/out" >"$E2E_TMP/live.log" 2>&1 &
    LIVE_PID=$!
    LIVE_UP=0
    for _ in $(seq 1 120); do
      if curl -s -o /dev/null "http://127.0.0.1:${LIVE_PORT}/mcp"; then LIVE_UP=1; break; fi
      sleep 0.5
    done
    if [ "$LIVE_UP" != "1" ]; then
      echo "  skip - @playwright/mcp did not come up on :${LIVE_PORT} ($(tail -c 200 "$E2E_TMP/live.log"))"
    else
      "$WASMTIME" serve -Sp3,cli,http --env "PLAYWRIGHT_BASE_URL=http://127.0.0.1:${LIVE_PORT}/mcp" \
        --env RUST_LOG=info --env MCP_OUTBOUND_TIMEOUT_MS=90000 \
        --addr "127.0.0.1:${LIVE_INST_PORT}" "$WASM" >"$E2E_TMP/live-inst.log" 2>&1 &
      LIVE_INST_PID=$!
      mcp_wait_ready "$LIVE_INST_PORT"
      OUT=$(mcp_call_on "$LIVE_BASE" playwright_status '{}')
      assert_json "live: upstream reachable and identified" 'r["result"]["structuredContent"]["status"] == "ok" and r["result"]["structuredContent"]["upstream"]["name"] == "Playwright"' "$OUT"
      OUT=$(tools_list_on "$LIVE_BASE")
      assert_contains "live: browser_fill_form is listed" '"name":"browser_fill_form"' "$OUT"
      assert_not_contains "live: browser_evaluate is hidden" '"name":"browser_evaluate"' "$OUT"
      assert_not_contains "live: filename is stripped from real schemas" '"filename":' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" browser_navigate "{\"url\":\"http://127.0.0.1:${LIVE_PAGE_PORT}/\"}")
      assert_contains "live: navigate inlines the real snapshot" '[ref=e' "$OUT"
      assert_not_contains "live: no file link left" '[Snapshot](' "$OUT"
      BOX=$(printf '%s' "$OUT" | python3 -c 'import json,re,sys; raw=sys.stdin.read(); msg=[json.loads(l[5:]) for l in raw.splitlines() if l.startswith("data:")][0]; t=msg["result"]["content"][0]["text"]; m=re.search(r"textbox \"Your name\" \[ref=(e\d+)\]", t); print(m.group(1) if m else "")')
      BTN=$(printf '%s' "$OUT" | python3 -c 'import json,re,sys; raw=sys.stdin.read(); msg=[json.loads(l[5:]) for l in raw.splitlines() if l.startswith("data:")][0]; t=msg["result"]["content"][0]["text"]; m=re.search(r"button \"Go\" \[ref=(e\d+)\]", t); print(m.group(1) if m else "")')
      if [ -n "$BOX" ] && [ -n "$BTN" ]; then pass "live: refs for the textbox and button found in the inline snapshot ($BOX, $BTN)"; else fail "live: refs found in the inline snapshot" "$OUT"; fi
      OUT=$(mcp_call_on "$LIVE_BASE" browser_type "{\"target\":\"${BOX:-e3}\",\"text\":\"Liam\"}")
      assert_contains "live: type by ref" 'fill(' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" browser_click "{\"target\":\"${BTN:-e4}\"}")
      assert_contains "live: click by ref returns an inline snapshot" '```yaml' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" browser_wait_for '{"text":"clicked Liam"}')
      assert_contains "live: wait_for text sees the click result" 'Waited for clicked Liam' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" browser_take_screenshot '{}')
      assert_contains "live: screenshot returns a jpeg image block" '"mimeType":"image/jpeg"' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" browser_click '{"target":"e999"}')
      assert_contains "live: stale ref error passes through" 'not found in the current page snapshot' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" browser_evaluate '{"function":"() => 1"}')
      assert_contains "live: gated tool refused" 'PLAYWRIGHT_ALLOW_UNSAFE' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" browser_close '{}')
      assert_contains "live: close" 'No open tabs' "$OUT"
      OUT=$(mcp_call_on "$LIVE_BASE" playwright_reset_session '{}')
      assert_json "live: reset session DELETEs the real session" 'r["result"]["structuredContent"]["status"] == "reset" and r["result"]["structuredContent"]["upstream_status"] == 200' "$OUT"
    fi
  fi
fi

guard_tests
mcp_harness_report
