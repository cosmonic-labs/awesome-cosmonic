#!/usr/bin/env bash
# End-to-end tests for docker-mcp. Framework checks (protocol, spec
# enforcement, discovery route, skills over MCP, robustness, Host guard) come
# from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases live
# below.
#
# Hermetic: scripts/fixture.py impersonates a Docker Engine (API 1.47, with a
# few podman-shaped answers) on 127.0.0.1:FIXTURE_PORT and DOCKER_HOST points
# every instance at it. Four wasmtime instances run:
#   :PORT         v1.44, writes enabled, registry auth JSON      — most cases
#   :GUARD_PORT   defaults (read-only), DOCKER_HOST=127.0.0.1:1   — gates, transport error, Host guard
#   :NOAUTH_PORT  writes enabled, no registry auth, 5 s deadline  — missing secret, timeouts
#   :OLDAPI_PORT  DOCKER_API_VERSION=v1.43, placeholder secret    — version window, invalid secret
#   :LIVE_PORT    pre-encoded (standard base64, unpadded) secret  — header normalisation (killed before live)
#   :DNS_PORT     DOCKER_HOST on an unresolvable name (short-lived) — the DNS-failure hint
# The no-auth instance runs with RUST_LOG=debug so the log-leak checks cover
# the chattiest level.
#
# Usage: scripts/e2e.sh [--no-build]
#   E2E_LIVE=1 [DOCKER_LIVE_HOST=http://127.0.0.1:2375] adds live cases against
#   a real daemon (podman system service / dockerd TCP listener): pull, run,
#   logs, stats, kill, remove.
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9808}
GUARD_PORT=${GUARD_PORT:-9809}
FIXTURE_PORT=${FIXTURE_PORT:-9810}
NOAUTH_PORT=${NOAUTH_PORT:-9811}
OLDAPI_PORT=${OLDAPI_PORT:-9812}
LIVE_PORT=${LIVE_PORT:-9813}
DNS_PORT=${DNS_PORT:-9814}
WASM=${WASM:-target/wasm32-wasip2/release/docker_mcp.wasm}
SKILL_NAME=docker-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIXTURE_URL="http://127.0.0.1:${FIXTURE_PORT}"
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"
NOAUTH_BASE="http://127.0.0.1:${NOAUTH_PORT}/"
OLDAPI_BASE="http://127.0.0.1:${OLDAPI_PORT}/"
LIVE_BASE="http://127.0.0.1:${LIVE_PORT}/"
NOAUTH_PID=""
OLDAPI_PID=""
PREENC_PID=""
LIVE_PID=""
LIVEAUTH_PID=""
DNS_PID=""

cleanup_all() {
  [ -n "$DNS_PID" ] && kill "$DNS_PID" 2>/dev/null
  [ -n "$NOAUTH_PID" ] && kill "$NOAUTH_PID" 2>/dev/null
  [ -n "$OLDAPI_PID" ] && kill "$OLDAPI_PID" 2>/dev/null
  [ -n "$PREENC_PID" ] && kill "$PREENC_PID" 2>/dev/null
  [ -n "$LIVE_PID" ] && kill "$LIVE_PID" 2>/dev/null
  [ -n "$LIVEAUTH_PID" ] && kill "$LIVEAUTH_PID" 2>/dev/null
  mcp_harness_cleanup
}
trap cleanup_all EXIT

# assert_req <name> <python-expr> — evaluates <expr> with `reqs` bound to the
# fixture's recorded requests (oldest first), `last` to the newest and `prev`
# to the one before. Each request: method, path, raw_path, raw_query,
# query (dict of lists), headers, body. `b64`/`json`/`calendar` are importable.
assert_req() {
  local name="$1" expr="$2"
  if curl -sS "${FIXTURE_URL}/_log" | python3 -c '
import json, sys, base64, calendar
reqs = json.load(sys.stdin)
last = reqs[-1] if reqs else {}
prev = reqs[-2] if len(reqs) > 1 else {}
def b64(s):
    return json.loads(base64.urlsafe_b64decode(s + "=" * (-len(s) % 4)))
def b64strict(s):
    # Go base64.URLEncoding (what dockerd/podman use) requires padding.
    if len(s) % 4 != 0 or any(c in s for c in "+/"):
        raise ValueError("not padded url-safe base64")
    return json.loads(base64.urlsafe_b64decode(s))
sys.exit(0 if eval(sys.argv[1]) else 1)
' "$expr" 2>/dev/null; then
    pass "$name"
  else
    fail "$name" "expression [$expr] false; last request: $(curl -sS "${FIXTURE_URL}/_log" | python3 -c 'import json,sys; r=json.load(sys.stdin); print(json.dumps(r[-1]) if r else "none")' | head -c 500)"
  fi
}

# assert_failed <name> <out> — a tool-level error (isError) or a JSON-RPC error.
assert_failed() {
  case "$2" in
    *'"isError":true'* | *'"error":{'*) pass "$1" ;;
    *) fail "$1" "expected a failure, got: $2" ;;
  esac
}

fx_reset() { curl -sS -X POST "${FIXTURE_URL}/_reset" >/dev/null; }
fx_count() { curl -sS "${FIXTURE_URL}/_log" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'; }

# The concurrency test in framework_tests fires this tool 8x in parallel.
FIRST_TOOL_NAME=version
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"api_version_ok":true'

mcp_build_if_needed "${1:-}"

echo "starting fixture on :${FIXTURE_PORT}..."
python3 scripts/fixture.py "$FIXTURE_PORT" >"$E2E_TMP/fixture.log" 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "${FIXTURE_URL}/_log" && break
  sleep 0.2
done

REG_AUTH='{"username":"u","password":"p","serveraddress":"reg.test"}'
COMMON=(--env "DOCKER_HOST=${FIXTURE_URL}" --env RUST_LOG=info)
mcp_harness_start "${COMMON[@]}" --env DOCKER_READ_ONLY=false --env "DOCKER_REGISTRY_AUTH=${REG_AUTH}"
# Guard instance: no DOCKER_READ_ONLY (defaults to read-only), no registry
# auth, and a DOCKER_HOST nothing listens on (transport error path).
mcp_harness_start_guard --env DOCKER_HOST=http://127.0.0.1:1 --env RUST_LOG=info

echo "starting no-auth instance on :${NOAUTH_PORT} (RUST_LOG=debug)..."
"$WASMTIME" serve -Sp3,cli,http --env "DOCKER_HOST=${FIXTURE_URL}" --env RUST_LOG=debug \
  --env DOCKER_READ_ONLY=false --env MCP_OUTBOUND_TIMEOUT_MS=5000 \
  --addr "127.0.0.1:${NOAUTH_PORT}" "$WASM" >"$E2E_TMP/noauth.log" 2>&1 &
NOAUTH_PID=$!
mcp_wait_ready "$NOAUTH_PORT"

echo "starting old-api instance on :${OLDAPI_PORT}..."
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env DOCKER_READ_ONLY=false --env DOCKER_API_VERSION=v1.43 \
  --env DOCKER_REGISTRY_AUTH=REPLACE_ME \
  --addr "127.0.0.1:${OLDAPI_PORT}" "$WASM" >"$E2E_TMP/oldapi.log" 2>&1 &
OLDAPI_PID=$!
mcp_wait_ready "$OLDAPI_PORT"

# Pre-encoded credential: standard alphabet ('+' and '/'), padding stripped —
# the shape `echo -n '{...}' | base64 | tr -d =` produces. The header must
# still go out as padded url-safe base64 (Go's base64.URLEncoding).
PREENC_JSON='{"username":"u","password":"x>?a?","serveraddress":"reg.test"}'
PREENC_STD_NOPAD='eyJ1c2VybmFtZSI6InUiLCJwYXNzd29yZCI6Ing+P2E/Iiwic2VydmVyYWRkcmVzcyI6InJlZy50ZXN0In0'
PREENC_EXPECT='eyJ1c2VybmFtZSI6InUiLCJwYXNzd29yZCI6Ing-P2E_Iiwic2VydmVyYWRkcmVzcyI6InJlZy50ZXN0In0='
PREENC_BASE="$LIVE_BASE"
echo "starting pre-encoded-credential instance on :${LIVE_PORT}..."
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env DOCKER_READ_ONLY=false \
  --env "DOCKER_REGISTRY_AUTH=${PREENC_STD_NOPAD}" \
  --addr "127.0.0.1:${LIVE_PORT}" "$WASM" >"$E2E_TMP/preenc.log" 2>&1 &
PREENC_PID=$!
mcp_wait_ready "$LIVE_PORT"

ALL_TOOLS=(version info list_containers inspect_container container_logs container_stats
  run_container start_container stop_container restart_container kill_container remove_container
  list_images inspect_image pull_image remove_image list_networks list_volumes system_df)
WRITE_TOOLS=(run_container start_container stop_container restart_container kill_container remove_container pull_image remove_image)

framework_tests "${ALL_TOOLS[@]}"
discovery_tests list_containers
skills_tests "$SKILL_NAME" references/TOOLS.md references/ERRORS.md references/SETUP.md

echo "== discovery: credentials block =="
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / lists the optional docker-mcp-registry-auth credential" '"ref": "docker-mcp-registry-auth"' "$ROOT"
assert_contains "GET / names the env var" '"env": "DOCKER_REGISTRY_AUTH"' "$ROOT"
assert_contains "GET / marks the credential optional" '"required": false' "$ROOT"
assert_contains "GET / reports the credential as configured (presence only)" '"status": "configured"' "$ROOT"
assert_contains "GET / names version as the credential's validate tool" '"validate": "version"' "$ROOT"
assert_not_contains "GET / never leaks the credential value" 'reg.test' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD_BASE")
assert_contains "GET / on the guard instance reports the credential missing" '"status": "missing"' "$ROOT"

echo "== version =="
fx_reset
OUT=$(mcp_call version '{}')
assert_json "version reports engine docker and the daemon's window" 'r["result"]["structuredContent"]["engine"] == "docker" and r["result"]["structuredContent"]["api_version"] == "1.47" and r["result"]["structuredContent"]["min_api_version"] == "1.44"' "$OUT"
assert_json "version says v1.44 is inside the window" 'r["result"]["structuredContent"]["api_version_ok"] is True and r["result"]["structuredContent"]["configured_api_version"] == "v1.44"' "$OUT"
assert_json "version reports read_only=false and registry auth configured" 'r["result"]["structuredContent"]["read_only"] is False and r["result"]["structuredContent"]["registry_auth_configured"] is True' "$OUT"
assert_json "version validates the registry credential's shape (registry_auth_valid=true)" 'r["result"]["structuredContent"]["registry_auth_valid"] is True and "registry_auth_error" not in r["result"]["structuredContent"]' "$OUT"
assert_contains "version text reports the credential as well-formed" 'configured and well-formed' "$OUT"
assert_not_contains "version never echoes the credential" 'reg.test' "$OUT"
assert_json "version is not an error" 'r["result"].get("isError") is False' "$OUT"
assert_req "version dialled /v1.44/version with a User-Agent" 'last["raw_path"] == "/v1.44/version" and last["headers"].get("user-agent", "").startswith("docker-mcp/")'
fx_reset
OUT=$(mcp_call_on "$OLDAPI_BASE" version '{}')
assert_json "version on v1.43 falls back and reports the window as violated" 'r["result"]["structuredContent"]["api_version_ok"] is False and "too old" in r["result"]["structuredContent"]["error"] and r["result"]["structuredContent"]["api_version"] == "1.47"' "$OUT"
assert_json "version on v1.43 carries the DOCKER_API_VERSION hint" '"DOCKER_API_VERSION" in r["result"]["structuredContent"]["hint"] and "v1.47" in r["result"]["structuredContent"]["hint"]' "$OUT"
assert_json "version flags the placeholder credential as invalid without dialling a registry" 'r["result"]["structuredContent"]["registry_auth_configured"] is True and r["result"]["structuredContent"]["registry_auth_valid"] is False and "neither a JSON object" in r["result"]["structuredContent"]["registry_auth_error"] and "docker-mcp-registry-auth" in r["result"]["structuredContent"]["registry_auth_error"]' "$OUT"
assert_not_contains "version never echoes the placeholder value" 'REPLACE_ME' "$OUT"
assert_req "version fallback hit /v1.43/version, then /version and /_ping" '[q["raw_path"] for q in reqs] == ["/v1.43/version", "/version", "/_ping"]'
OUT=$(mcp_call_on "$OLDAPI_BASE" info '{}')
assert_contains "info on v1.43 maps the version-window 400" 'too old' "$OUT"
assert_contains "version-window error names DOCKER_API_VERSION" 'DOCKER_API_VERSION' "$OUT"
assert_contains "version-window error is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" version '{}')
assert_contains "unreachable daemon is an actionable tool error" 'could not reach the Docker daemon' "$OUT"
assert_contains "unreachable-daemon error quotes the allowedHosts grant" 'host.wasmcloud.internal:2375' "$OUT"
assert_contains "unreachable-daemon error quotes the loopback port grant" 'allowedHostLoopbackPorts' "$OUT"
assert_contains "unreachable-daemon error quotes the Security toggle" 'allow host loopback' "$OUT"
assert_contains "unreachable-daemon error is a tool error" '"isError":true' "$OUT"

echo "== info =="
OUT=$(mcp_call info '{}')
assert_json "info trims the daemon document" 'r["result"]["structuredContent"]["ServerVersion"] == "27.5.1" and r["result"]["structuredContent"]["Runtimes"] == ["io.containerd.runc.v2", "runc"] and r["result"]["structuredContent"]["Warnings"] == ["WARNING: No swap limit support"]' "$OUT"
assert_json "info renders MemTotal in human units and swarm state" 'r["result"]["structuredContent"]["MemTotalHuman"] == "16.0 GiB" and r["result"]["structuredContent"]["SwarmLocalNodeState"] == "inactive"' "$OUT"
assert_json "info trimmed view drops Plugins" '"Plugins" not in r["result"]["structuredContent"]' "$OUT"
OUT=$(mcp_call info '{"raw":true}')
assert_json "info raw=true returns the full document" 'r["result"]["structuredContent"]["Plugins"]["Log"] == ["json-file"]' "$OUT"

echo "== list_containers =="
fx_reset
OUT=$(mcp_call list_containers '{}')
assert_json "list_containers returns 12-char ids, stripped names and rendered ports" 'r["result"]["structuredContent"]["count"] == 4 and r["result"]["structuredContent"]["containers"][0]["id"] == "web123456789" and r["result"]["structuredContent"]["containers"][0]["names"] == ["web"] and r["result"]["structuredContent"]["containers"][0]["ports"] == ["127.0.0.1:8080->80/tcp"]' "$OUT"
assert_json "list_containers renders Created as RFC 3339 and counts labels" 'r["result"]["structuredContent"]["containers"][0]["created"] == "2026-09-05T09:20:00Z" and r["result"]["structuredContent"]["containers"][0]["labels"] == 2' "$OUT"
assert_json "list_containers passes podman's stopped state through" 'any(c["state"] == "stopped" for c in r["result"]["structuredContent"]["containers"])' "$OUT"
assert_req "list_containers defaults to all=true, limit=100, size=false, no filters" 'last["raw_path"] == "/v1.44/containers/json" and last["query"] == {"all": ["true"], "limit": ["100"], "size": ["false"]}'
OUT=$(mcp_call list_containers '{"all":false,"limit":999999,"size":true,"filters":{"status":["running"],"name":"web"}}')
assert_json "list_containers clamps limit to 500" 'r["result"]["structuredContent"]["limit"] == 500 and r["result"]["structuredContent"]["all"] is False' "$OUT"
assert_req "list_containers sends filters as percent-encoded JSON of arrays" 'last["query"]["limit"] == ["500"] and last["query"]["all"] == ["false"] and last["query"]["size"] == ["true"] and "filters=%7B%22name%22%3A%5B%22web%22%5D%2C%22status%22%3A%5B%22running%22%5D%7D" in last["raw_query"]'
OUT=$(mcp_call list_containers '{"limit":0}')
assert_json "list_containers clamps limit 0 -> 1 client-side too" 'r["result"]["structuredContent"]["limit"] == 1 and r["result"]["structuredContent"]["count"] == 1' "$OUT"
N=$(fx_count)
OUT=$(mcp_call list_containers '{"filters":{"colour":["red"]}}')
assert_contains "list_containers refuses an unknown filter key" 'unknown filter key' "$OUT"
assert_contains "unknown-filter error lists the allowed keys" 'allowed: ancestor' "$OUT"
OUT=$(mcp_call list_containers '{"filters":{"status":[{"x":1}]}}')
assert_contains "list_containers refuses non-string filter values" 'must be strings' "$OUT"
OUT=$(mcp_call list_containers "$(python3 -c 'import json; print(json.dumps({"filters": {"name": ["x" * 600]}}))')")
assert_contains "list_containers refuses a 600-char filter value" 'longer than 512' "$OUT"
[ "$(fx_count)" = "$N" ] && pass "rejected filters never reach the daemon" || fail "rejected filters never reach the daemon" "fixture saw extra requests"
OUT=$(mcp_call list_containers '{"filters":{"name":["\"];DROP TABLE x;--","☃ snow"],"label":"a=b"}}')
assert_json "list_containers passes injection-shaped/unicode filters safely" 'r["result"]["structuredContent"]["count"] >= 1' "$OUT"
assert_req "injection-shaped filter values arrive as JSON, percent-encoded" 'json.loads(last["query"]["filters"][0]) == {"label": ["a=b"], "name": ["\"];DROP TABLE x;--", "☃ snow"]} and ";" not in last["raw_query"]'

echo "== inspect_container =="
fx_reset
OUT=$(mcp_call inspect_container '{"id":"web"}')
assert_json "inspect_container trims state/config/host_config/network" 'r["result"]["structuredContent"]["name"] == "web" and r["result"]["structuredContent"]["state"]["Status"] == "running" and r["result"]["structuredContent"]["config"]["Image"] == "nginx:1.27" and r["result"]["structuredContent"]["host_config"]["PortBindings"]["80/tcp"][0]["HostPort"] == "8080" and r["result"]["structuredContent"]["network_settings"]["networks"]["bridge"]["ip_address"] == "172.17.0.2"' "$OUT"
assert_json "inspect_container reports no redaction when nothing looks secret" 'r["result"]["structuredContent"]["env_redacted"] is False and r["result"]["structuredContent"]["mounts"][0]["Destination"] == "/data"' "$OUT"
assert_req "inspect_container dialled /containers/web/json without size" 'last["raw_path"] == "/v1.44/containers/web/json" and last["raw_query"] == ""'
OUT=$(mcp_call inspect_container '{"id":"web","size":true}')
assert_req "inspect_container size=true is forwarded" 'last["query"] == {"size": ["true"]}'
OUT=$(mcp_call inspect_container '{"id":"envy"}')
assert_json "inspect_container redacts secret-looking Env values" 'r["result"]["structuredContent"]["config"]["Env"] == ["PASSWORD=***", "TOKEN=***", "API_KEY=***", "PLAIN=ok"] and r["result"]["structuredContent"]["env_redacted"] is True' "$OUT"
assert_not_contains "redacted output never carries the secret" 'hunter2' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"envy","include_env_values":true}')
assert_json "include_env_values=true shows the values" '"PASSWORD=hunter2" in r["result"]["structuredContent"]["config"]["Env"] and r["result"]["structuredContent"]["env_redacted"] is False' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"web","raw":true}')
assert_json "inspect_container raw=true returns the daemon document" 'r["result"]["structuredContent"]["HostConfig"]["LogConfig"]["Type"] == "json-file"' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"gone"}')
assert_contains "404 container maps to the not-found hint" 'HTTP 404' "$OUT"
assert_contains "404 container keeps the upstream message" 'No such container: gone' "$OUT"
assert_contains "404 container suggests list_containers" 'list_containers with all=true' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"forbidden-section"}')
assert_contains "403 Forbidden maps to the socket-proxy hint" 'docker-socket-proxy' "$OUT"
assert_contains "403 keeps the status" 'HTTP 403' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"ratelimit"}')
assert_contains "429 maps to the rate-limit hint" 'Rate limited' "$OUT"
assert_contains "429 surfaces Retry-After" 'Retry-After: 7' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"boom"}')
assert_contains "500 maps to the retry-once hint" 'HTTP 500' "$OUT"
assert_contains "500 says retry once" 'Retry once' "$OUT"
N=$(fx_count)
OUT=$(mcp_call inspect_container '{"id":"../etc/passwd"}')
assert_contains "traversal-shaped id is refused" 'id must match' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"web?x=1"}')
assert_contains "query-injection-shaped id is refused" 'id must match' "$OUT"
OUT=$(mcp_call inspect_container '{"id":"wéb"}')
assert_contains "unicode id is refused" 'id must match' "$OUT"
OUT=$(mcp_call inspect_container "$(python3 -c 'import json; print(json.dumps({"id": "a" * 200}))')")
assert_contains "200-char id is refused" 'id must match' "$OUT"
OUT=$(mcp_call inspect_container '{"id":""}')
assert_contains "empty id is refused" 'id must match' "$OUT"
OUT=$(mcp_call inspect_container '{}')
assert_failed "missing id is a clean failure" "$OUT"
[ "$(fx_count)" = "$N" ] && pass "rejected ids never reach the daemon" || fail "rejected ids never reach the daemon" "fixture saw extra requests"

echo "== container_logs =="
fx_reset
OUT=$(mcp_call container_logs '{"id":"web","max_bytes":1048576}')
assert_json "container_logs demultiplexes stdout and stderr frames" '"web: started\n" in r["result"]["structuredContent"]["stdout"] and "héllo ✓" in r["result"]["structuredContent"]["stdout"] and "tail-line" in r["result"]["structuredContent"]["stdout"] and r["result"]["structuredContent"]["stderr"] == "web: warn 1\nweb: warn 2\n"' "$OUT"
assert_json "container_logs combined output is prefixed in arrival order" 'r["result"]["structuredContent"]["combined"].startswith("out|web: started\nerr|web: warn 1\nout|héllo ✓\nerr|web: warn 2\n")' "$OUT"
assert_json "container_logs tolerates a truncated final frame" 'r["result"]["structuredContent"]["partial_frame"] is True and r["result"]["structuredContent"]["stdout"].endswith("truncated!") and r["result"]["structuredContent"]["frames"] == 7 and r["result"]["structuredContent"]["tty"] is False and r["result"]["structuredContent"]["truncated"] is False' "$OUT"
assert_req "container_logs sends follow=false, both streams, tail=200, since/until 0" 'last["raw_path"] == "/v1.44/containers/web/logs" and last["query"] == {"follow": ["false"], "stdout": ["true"], "stderr": ["true"], "tail": ["200"], "since": ["0"], "until": ["0"], "timestamps": ["false"]} and prev["raw_path"] == "/v1.44/containers/web/json"'
OUT=$(mcp_call container_logs '{"id":"web"}')
assert_json "container_logs default max_bytes keeps the newest bytes" 'r["result"]["structuredContent"]["truncated"] is True and r["result"]["structuredContent"]["bytes"] <= 65536 and r["result"]["structuredContent"]["total_bytes"] > 300000 and "tail-line" in r["result"]["structuredContent"]["stdout"] and "web: started" not in r["result"]["structuredContent"]["stdout"]' "$OUT"
assert_contains "truncated logs say so in the text" 'truncated: showing the newest' "$OUT"
OUT=$(mcp_call container_logs '{"id":"web","tail":999999,"max_bytes":1,"stderr":false,"timestamps":true}')
assert_req "container_logs clamps tail to 5000 and forwards stderr=false/timestamps" 'last["query"]["tail"] == ["5000"] and last["query"]["stderr"] == ["false"] and last["query"]["timestamps"] == ["true"]'
assert_json "container_logs clamps max_bytes up to 1024" 'r["result"]["structuredContent"]["bytes"] <= 1024 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call container_logs '{"id":"web","tail":-5}')
assert_req "negative tail is clamped to 1 (never sent raw)" 'last["query"]["tail"] == ["1"]'
OUT=$(mcp_call container_logs '{"id":"tty"}')
assert_json "TTY container logs are raw with CRLF normalised and no prefixes" 'r["result"]["structuredContent"]["tty"] is True and r["result"]["structuredContent"]["combined"] == "tty-out\ntty-err\n" and r["result"]["structuredContent"]["stderr"] == ""' "$OUT"
OUT=$(mcp_call container_logs '{"id":"web","since":"2026-09-01T12:00:00Z","until":1788700000}')
assert_req "RFC 3339 since is converted to unix seconds" 'last["query"]["since"] == [str(calendar.timegm((2026, 9, 1, 12, 0, 0)))] and last["query"]["until"] == ["1788700000"]'
OUT=$(mcp_call container_logs '{"id":"web","since":"2026-09-01T14:30:00+02:00"}')
assert_req "RFC 3339 offsets are honoured" 'last["query"]["since"] == [str(calendar.timegm((2026, 9, 1, 12, 30, 0)))]'
N=$(fx_count)
OUT=$(mcp_call container_logs '{"id":"web","stdout":false,"stderr":false}')
assert_contains "both streams off is refused client-side" 'at least one of stdout/stderr' "$OUT"
OUT=$(mcp_call container_logs '{"id":"web","since":5,"until":3}')
assert_contains "until before since is refused" 'until must be later than since' "$OUT"
OUT=$(mcp_call container_logs '{"id":"web","since":"yesterday"}')
assert_contains "unparsable since is refused" 'RFC 3339' "$OUT"
OUT=$(mcp_call container_logs '{"id":"web","since":-1}')
assert_contains "negative since is refused" 'must not be negative' "$OUT"
[ "$(fx_count)" = "$N" ] && pass "rejected log params never reach the daemon" || fail "rejected log params never reach the daemon" "fixture saw extra requests"
OUT=$(mcp_call container_logs '{"id":"nologs"}')
assert_contains "unreadable log driver maps to the driver hint" 'logging driver' "$OUT"
assert_contains "log-driver error keeps the status" 'HTTP 500' "$OUT"
OUT=$(mcp_call container_logs '{"id":"gone"}')
assert_contains "logs of a missing container is the 404 hint" 'No such container' "$OUT"

echo "== container_stats =="
fx_reset
OUT=$(mcp_call container_stats '{"id":"web"}')
assert_json "container_stats computes cpu_percent from the pre/post deltas" 'r["result"]["structuredContent"]["cpu_percent"] == 25.0' "$OUT"
assert_json "container_stats subtracts inactive_file and computes memory percent" 'r["result"]["structuredContent"]["memory_usage"] == 500 and r["result"]["structuredContent"]["memory_limit"] == 1000 and r["result"]["structuredContent"]["memory_percent"] == 50.0' "$OUT"
assert_json "container_stats sums networks and blkio, reports pids" 'r["result"]["structuredContent"]["network_rx_bytes"] == 15 and r["result"]["structuredContent"]["network_tx_bytes"] == 25 and r["result"]["structuredContent"]["block_read_bytes"] == 7 and r["result"]["structuredContent"]["block_write_bytes"] == 9 and r["result"]["structuredContent"]["pids"] == 3' "$OUT"
assert_req "container_stats asks for one sample with precpu (stream=false, one-shot=false)" 'last["raw_path"] == "/v1.44/containers/web/stats" and last["query"] == {"stream": ["false"], "one-shot": ["false"]}'
OUT=$(mcp_call container_stats '{"id":"web","raw":true}')
assert_json "container_stats raw=true includes the sample" 'r["result"]["structuredContent"]["raw"]["cpu_stats"]["online_cpus"] == 1' "$OUT"
OUT=$(mcp_call container_stats '{"id":"stopped-podman"}')
assert_contains "podman stats of a stopped container maps to the running-only hint" 'container is stopped' "$OUT"
assert_contains "podman stopped-stats hint names the tool" 'container_stats' "$OUT"

echo "== run_container =="
fx_reset
OUT=$(mcp_call run_container '{"image":"alpine:3.20","name":"job1","cmd":["sh","-c","echo hi"],"env":{"A":"1","B":"x y"},"labels":{"app":"e2e"},"ports":["8081:81","0.0.0.0:8082:82/udp","[::1]:0:83"],"restart_policy":"on-failure","memory_bytes":10485760,"cpus":0.5,"platform":"linux/arm64","working_dir":"/w","user":"1000:1000"}')
assert_json "run_container creates and starts" 'r["result"]["structuredContent"]["id"] == "createdabc12" and r["result"]["structuredContent"]["started"] is True and r["result"]["structuredContent"]["warnings"] == ["fixture warning"] and r["result"]["structuredContent"]["name"] == "job1"' "$OUT"
assert_req "run_container create body: Tty false, no Privileged, ports bound to 127.0.0.1 by default" '(lambda b: b["Tty"] is False and b["AttachStdin"] is False and "Privileged" not in b["HostConfig"] and "Binds" not in b["HostConfig"] and b["HostConfig"]["PortBindings"]["81/tcp"] == [{"HostIp": "127.0.0.1", "HostPort": "8081"}] and b["HostConfig"]["PortBindings"]["82/udp"] == [{"HostIp": "0.0.0.0", "HostPort": "8082"}] and b["HostConfig"]["PortBindings"]["83/tcp"] == [{"HostIp": "::1", "HostPort": ""}] and b["ExposedPorts"] == {"81/tcp": {}, "82/udp": {}, "83/tcp": {}})(json.loads(prev["body"]))'
assert_req "run_container create body: env/labels/limits/policy/user/workdir" '(lambda b: b["Env"] == ["A=1", "B=x y"] and b["Labels"] == {"app": "e2e"} and b["HostConfig"]["Memory"] == 10485760 and b["HostConfig"]["NanoCpus"] == 500000000 and b["HostConfig"]["RestartPolicy"] == {"Name": "on-failure"} and b["HostConfig"]["AutoRemove"] is False and b["WorkingDir"] == "/w" and b["User"] == "1000:1000" and b["Cmd"] == ["sh", "-c", "echo hi"])(json.loads(prev["body"]))'
assert_req "run_container create query carries name and encoded platform, then start" 'prev["raw_path"] == "/v1.44/containers/create" and prev["query"] == {"name": ["job1"], "platform": ["linux/arm64"]} and "platform=linux%2Farm64" in prev["raw_query"] and last["method"] == "POST" and last["raw_path"].endswith("/start")'
fx_reset
OUT=$(mcp_call run_container '{"image":"alpine:3.20","env":["A=1"],"wait":true}')
assert_json "run_container wait=true returns the exit code and demuxed logs" 'r["result"]["structuredContent"]["exited"] is True and r["result"]["structuredContent"]["exit_code"] == 3 and "done-out" in r["result"]["structuredContent"]["stdout"] and "done-err" in r["result"]["structuredContent"]["stderr"]' "$OUT"
assert_req "run_container wait uses the daemon's blocking wait endpoint" 'any(q["raw_path"].endswith("/wait") and q["query"] == {"condition": ["not-running"]} for q in reqs) and last["raw_path"].endswith("/logs") and last["query"]["tail"] == ["200"]'
fx_reset
OUT=$(mcp_call run_container '{"image":"pullme:1","pull_if_missing":true,"env":["DB_PASSWORD=hunter2-leak-test-info"]}')
assert_json "run_container pull_if_missing pulls once and retries the create" 'r["result"]["structuredContent"]["pulled"]["layers"] == 2 and r["result"]["structuredContent"]["started"] is True' "$OUT"
assert_req "pull_if_missing create body carries the env (sent, not logged)" 'any(q["raw_path"] == "/v1.44/containers/create" and "hunter2-leak-test-info" in q["body"] for q in reqs)'
if grep -q '"image missing, pulling before create"' "$E2E_TMP/server.log"; then pass "pull_if_missing emits its INFO event inside the tool span"; else fail "pull_if_missing emits its INFO event inside the tool span" "event missing from server.log"; fi
if grep -q 'tool.run_container' "$E2E_TMP/server.log"; then pass "the INFO event carries the tool.run_container span"; else fail "the INFO event carries the tool.run_container span" "span name missing from server.log"; fi
if grep -q 'hunter2-leak-test-info' "$E2E_TMP/server.log"; then fail "container Env never reaches the host log at RUST_LOG=info" "found in server.log: $(grep -m1 hunter2-leak-test-info "$E2E_TMP/server.log" | head -c 300)"; else pass "container Env never reaches the host log at RUST_LOG=info"; fi
if grep -q 'RunContainerParams' "$E2E_TMP/server.log"; then fail "tool spans do not record the params struct" "RunContainerParams found in server.log"; else pass "tool spans do not record the params struct"; fi
assert_req "pull_if_missing sequence: create 404, pull, create, start" '[q["raw_path"] for q in reqs] == ["/v1.44/containers/create", "/v1.44/images/create", "/v1.44/containers/create", "/v1.44/containers/createdabc123def4567890abcdef0123456789abcdef0123456789abcdef01/start"] and reqs[1]["query"] == {"fromImage": ["pullme"], "tag": ["1"]}'
OUT=$(mcp_call run_container '{"image":"missing:1"}')
assert_contains "run_container without pull maps No such image" 'No such image: missing:1' "$OUT"
assert_contains "No such image hint names pull_image" 'pull_image' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","name":"taken"}')
assert_contains "name conflict maps the 409" 'HTTP 409' "$OUT"
assert_contains "name conflict keeps the message" 'already in use' "$OUT"
N=$(fx_count)
OUT=$(mcp_call run_container '{"image":"alpine:3.20","ports":["abc"]}')
assert_contains "bad port spec is refused" "wrong number of ':' separators" "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","ports":["80:99999"]}')
assert_contains "out-of-range container port is refused" 'container port must be 1-65535' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","ports":["evil:80:80"]}')
assert_contains "non-IP host ip is refused" 'host ip is not an IP address' "$OUT"
OUT=$(mcp_call run_container "$(python3 -c 'import json; print(json.dumps({"image": "alpine:3.20", "env": ["BIG=" + "x" * (2 * 1024 * 1024)]}))')")
assert_contains "2 MiB env is refused" 'env entries total' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","env":["1BAD=x"]}')
assert_contains "bad env key is refused" 'env key' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","env":["NOEQUALS"]}')
assert_contains "env entry without = is refused" 'is not KEY=VALUE' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","name":"bad name!"}')
assert_contains "bad container name is refused" 'name must match' "$OUT"
OUT=$(mcp_call run_container '{"image":"../../etc"}')
assert_contains "traversal-shaped image is refused" 'image must be a reference' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpiné:1"}')
assert_contains "unicode image is refused" 'image must be a reference' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","memory_bytes":1}')
assert_contains "tiny memory limit is refused" '6 MiB' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","cpus":-1}')
assert_contains "negative cpus is refused" 'cpus must be' "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","restart_policy":"sometimes"}')
assert_failed "unknown restart_policy is a clean failure" "$OUT"
OUT=$(mcp_call run_container '{"image":"alpine:3.20","platform":"Linux/AMD64!"}')
assert_contains "bad platform is refused" 'platform must be' "$OUT"
[ "$(fx_count)" = "$N" ] && pass "rejected run_container params never reach the daemon" || fail "rejected run_container params never reach the daemon" "fixture saw extra requests"
OUT=$(mcp_call_on "$NOAUTH_BASE" run_container '{"image":"slowwait:1","wait":true}')
assert_json "run_container wait outliving the deadline reports still running" 'r["result"]["structuredContent"]["exited"] is False and "still running" in r["result"]["structuredContent"]["note"] and r["result"]["structuredContent"]["started"] is True' "$OUT"

echo "== start / stop / restart / kill / remove =="
fx_reset
OUT=$(mcp_call start_container '{"id":"web"}')
assert_json "start_container maps 304 to changed=false" 'r["result"]["structuredContent"]["changed"] is False and r["result"]["structuredContent"]["note"] == "already running"' "$OUT"
OUT=$(mcp_call start_container '{"id":"tty"}')
assert_json "start_container 204 is changed=true" 'r["result"]["structuredContent"]["changed"] is True' "$OUT"
assert_req "start_container POSTs /containers/tty/start" 'last["method"] == "POST" and last["raw_path"] == "/v1.44/containers/tty/start"'
OUT=$(mcp_call start_container '{"id":"gone"}')
assert_contains "start_container 404 is the not-found hint" 'No such container' "$OUT"
OUT=$(mcp_call stop_container '{"id":"web","timeout":999,"signal":"SIGTERM"}')
assert_json "stop_container clamps and caps the grace period to the deadline budget" 'r["result"]["structuredContent"]["changed"] is True and r["result"]["structuredContent"]["timeout"] == 25 and "exceeds the outbound deadline budget" in r["result"]["structuredContent"]["timeout_note"]' "$OUT"
assert_req "stop_container sends t and signal" 'last["raw_path"] == "/v1.44/containers/web/stop" and last["query"] == {"t": ["25"], "signal": ["SIGTERM"]}'
OUT=$(mcp_call stop_container '{"id":"tty"}')
assert_json "stop_container maps 304 to changed=false with default t" 'r["result"]["structuredContent"]["changed"] is False and r["result"]["structuredContent"]["timeout"] == 10 and "timeout_note" not in r["result"]["structuredContent"]' "$OUT"
assert_req "stop_container default t=10, no signal" 'last["query"] == {"t": ["10"]}'
OUT=$(mcp_call stop_container '{"id":"web","timeout":-3}')
assert_req "negative stop timeout is clamped to 0" 'last["query"]["t"] == ["0"]'
N=$(fx_count)
OUT=$(mcp_call stop_container '{"id":"web","signal":"rm -rf /"}')
assert_contains "bad signal is refused" 'signal must be' "$OUT"
[ "$(fx_count)" = "$N" ] && pass "rejected signal never reaches the daemon" || fail "rejected signal never reaches the daemon" "fixture saw extra requests"
OUT=$(mcp_call restart_container '{"id":"web"}')
assert_json "restart_container reports changed" 'r["result"]["structuredContent"]["changed"] is True and r["result"]["structuredContent"]["timeout"] == 10' "$OUT"
assert_req "restart_container POSTs /restart?t=10" 'last["raw_path"] == "/v1.44/containers/web/restart" and last["query"] == {"t": ["10"]}'
OUT=$(mcp_call kill_container '{"id":"web"}')
assert_json "kill_container defaults to SIGKILL" 'r["result"]["structuredContent"]["signal"] == "SIGKILL"' "$OUT"
assert_req "kill_container sends signal=SIGKILL" 'last["query"] == {"signal": ["SIGKILL"]}'
OUT=$(mcp_call kill_container '{"id":"web","signal":"hup"}')
assert_req "kill_container upper-cases the signal" 'last["query"] == {"signal": ["HUP"]}'
OUT=$(mcp_call kill_container '{"id":"tty"}')
assert_contains "kill of a stopped container maps the 409" 'is not running' "$OUT"
assert_contains "kill 409 hint names start_container" 'start_container' "$OUT"
OUT=$(mcp_call remove_container '{"id":"web"}')
assert_contains "remove of a running container maps Docker's 409" 'HTTP 409' "$OUT"
assert_contains "remove 409 hint says force=true" 'force=true' "$OUT"
OUT=$(mcp_call remove_container '{"id":"web","force":true,"volumes":true}')
assert_json "remove_container force=true succeeds" 'r["result"]["structuredContent"]["removed"] is True and r["result"]["structuredContent"]["force"] is True' "$OUT"
assert_req "remove_container sends v and force" 'last["method"] == "DELETE" and last["raw_path"] == "/v1.44/containers/web" and last["query"] == {"v": ["true"], "force": ["true"]}'
OUT=$(mcp_call remove_container '{"id":"podman-run"}')
assert_contains "podman's 500 for a running container maps to the force hint" 'podman reports a running/paused container' "$OUT"
assert_contains "podman remove hint says force=true" 'force=true' "$OUT"
OUT=$(mcp_call remove_container '{"id":"gone"}')
assert_contains "remove of a missing container is the 404 hint" 'No such container' "$OUT"

echo "== list_images / inspect_image =="
fx_reset
OUT=$(mcp_call list_images '{}')
assert_json "list_images sorts newest first with short ids and dangling flags" 'r["result"]["structuredContent"]["count"] == 3 and [i["id"] for i in r["result"]["structuredContent"]["images"]] == ["img300000000", "img200000000", "img100000000"] and r["result"]["structuredContent"]["images"][2]["dangling"] is True and r["result"]["structuredContent"]["images"][0]["dangling"] is False' "$OUT"
assert_json "list_images renders sizes and container counts" 'r["result"]["structuredContent"]["images"][0]["size_human"] == "143.1 MiB" and r["result"]["structuredContent"]["images"][0]["containers"] == 1 and r["result"]["structuredContent"]["images"][1]["containers"] is None and "repo_digests" not in r["result"]["structuredContent"]["images"][0]' "$OUT"
assert_req "list_images defaults: all=false digests=false shared-size=false" 'last["raw_path"] == "/v1.44/images/json" and last["query"] == {"all": ["false"], "digests": ["false"], "shared-size": ["false"]}'
OUT=$(mcp_call list_images '{"limit":2}')
assert_json "list_images limit truncates client-side" 'r["result"]["structuredContent"]["count"] == 2 and r["result"]["structuredContent"]["total"] == 3 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call list_images '{"limit":99999,"digests":true,"all":true,"filters":{"reference":["alpine:*"],"dangling":"true"}}')
assert_json "list_images clamps limit to 1000 and adds digests" 'r["result"]["structuredContent"]["limit"] == 1000 and r["result"]["structuredContent"]["images"][0]["repo_digests"] == ["nginx@sha256:aaaa"]' "$OUT"
assert_req "list_images forwards all/digests and JSON filters" 'last["query"]["all"] == ["true"] and last["query"]["digests"] == ["true"] and json.loads(last["query"]["filters"][0]) == {"dangling": ["true"], "reference": ["alpine:*"]}'
OUT=$(mcp_call list_images '{"filters":{"status":["running"]}}')
assert_contains "list_images refuses container-only filter keys" 'unknown filter key' "$OUT"
fx_reset
OUT=$(mcp_call inspect_image '{"name":"docker.io/library/alpine:3.20"}')
assert_json "inspect_image trims and redacts" 'r["result"]["structuredContent"]["id"] == "img200000000" and r["result"]["structuredContent"]["rootfs_layers"] == 2 and r["result"]["structuredContent"]["config"]["Env"] == ["PATH=/bin", "SECRET_KEY=***"] and r["result"]["structuredContent"]["variant"] == "v8"' "$OUT"
assert_req "inspect_image preserves slashes and colons in the path" 'last["raw_path"] == "/v1.44/images/docker.io/library/alpine:3.20/json"'
OUT=$(mcp_call inspect_image '{"name":"alpine:3.20","include_env_values":true}')
assert_json "inspect_image include_env_values shows the value" '"SECRET_KEY=abc" in r["result"]["structuredContent"]["config"]["Env"]' "$OUT"
OUT=$(mcp_call inspect_image '{"name":"sha256:img200000000000000000000000000000000000000000000000000000000000","raw":true}')
assert_json "inspect_image by digest id with raw=true" 'r["result"]["structuredContent"]["RootFS"]["Type"] == "layers"' "$OUT"
OUT=$(mcp_call inspect_image '{"name":"gone:1"}')
assert_contains "inspect_image 404 maps to the not-found hint" 'No such image' "$OUT"
assert_contains "image 404 hint names list_images" 'list_images' "$OUT"
N=$(fx_count)
OUT=$(mcp_call inspect_image '{"name":"a//b"}')
assert_contains "empty path segment in an image ref is refused" 'image must be a reference' "$OUT"
OUT=$(mcp_call inspect_image '{"name":"a/../b"}')
assert_contains "dot-dot segment in an image ref is refused" 'image must be a reference' "$OUT"
OUT=$(mcp_call inspect_image '{"name":"alpine 3"}')
assert_contains "space in an image ref is refused" 'image must be a reference' "$OUT"
OUT=$(mcp_call inspect_image '{"name":"alpine:3.20?x=1"}')
assert_contains "query-shaped image ref is refused" 'image must be a reference' "$OUT"
[ "$(fx_count)" = "$N" ] && pass "rejected image refs never reach the daemon" || fail "rejected image refs never reach the daemon" "fixture saw extra requests"

echo "== pull_image =="
fx_reset
OUT=$(mcp_call pull_image '{"image":"alpine:3.20"}')
assert_json "pull_image summarises layers, digest and status" 'r["result"]["structuredContent"]["layers"] == {"aaaa1111": "Pull complete", "bbbb2222": "Already exists"} and r["result"]["structuredContent"]["digest"] == "sha256:abc123" and r["result"]["structuredContent"]["defaulted_to_latest"] is False and r["result"]["structuredContent"]["registry_auth_sent"] is True and r["result"]["structuredContent"]["progress_lines"] == 7' "$OUT"
assert_req "pull_image POSTs /images/create?fromImage&tag with PADDED base64url X-Registry-Auth (Go base64.URLEncoding)" 'last["method"] == "POST" and last["raw_path"] == "/v1.44/images/create" and last["query"] == {"fromImage": ["alpine"], "tag": ["3.20"]} and len(last["headers"]["x-registry-auth"]) % 4 == 0 and last["headers"]["x-registry-auth"].endswith("==") and b64strict(last["headers"]["x-registry-auth"]) == {"username": "u", "password": "p", "serveraddress": "reg.test"}'
assert_req "X-Registry-Auth is exactly the padded url-safe encoding of the compact JSON" 'last["headers"]["x-registry-auth"] == "eyJwYXNzd29yZCI6InAiLCJzZXJ2ZXJhZGRyZXNzIjoicmVnLnRlc3QiLCJ1c2VybmFtZSI6InUifQ=="'
OUT=$(mcp_call pull_image '{"image":"badauthparse:1"}')
assert_contains "daemon 400 on the auth header maps to the X-Registry-Auth parse hint" 'could not parse the X-Registry-Auth header' "$OUT"
assert_contains "auth-header 400 hint keeps the daemon's message" 'failed to parse' "$OUT"
assert_contains "auth-header 400 hint names the secret ref" 'docker-mcp-registry-auth' "$OUT"
assert_contains "auth-header 400 is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call pull_image '{"image":"alpine"}')
assert_json "pull_image without a tag defaults to latest and says so" 'r["result"]["structuredContent"]["tag"] == "latest" and r["result"]["structuredContent"]["defaulted_to_latest"] is True and "every tag" in r["result"]["structuredContent"]["note"]' "$OUT"
OUT=$(mcp_call pull_image '{"image":"ghcr.io/org/app@sha256:deadbeef","platform":"linux/amd64","raw_progress":true}')
assert_json "pull_image raw_progress returns the tail of the stream" 'len(r["result"]["structuredContent"]["raw_progress"]) == 7 and r["result"]["structuredContent"]["tag"] == "sha256:deadbeef"' "$OUT"
assert_req "pull_image splits digest references and encodes platform" 'last["query"] == {"fromImage": ["ghcr.io/org/app"], "tag": ["sha256:deadbeef"], "platform": ["linux/amd64"]} and "fromImage=ghcr.io%2Forg%2Fapp" in last["raw_query"]'
OUT=$(mcp_call pull_image '{"image":"localhost:5000/foo"}')
assert_req "pull_image keeps a registry port out of the tag" 'last["query"] == {"fromImage": ["localhost:5000/foo"], "tag": ["latest"]}'
OUT=$(mcp_call pull_image '{"image":"broken:1"}')
assert_contains "mid-stream pull error is surfaced" 'failed mid-stream' "$OUT"
assert_contains "mid-stream pull error keeps the manifest message" 'manifest unknown' "$OUT"
assert_contains "mid-stream pull error is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call pull_image '{"image":"private/secret"}')
assert_json "pull_image with the credential pulls a private image" 'r["result"]["structuredContent"]["registry_auth_sent"] is True and r["result"].get("isError") is False' "$OUT"
N=$(fx_count)
OUT=$(mcp_call pull_image '{"image":"alpine:3.20","platform":"Linux/AMD64!"}')
assert_contains "pull_image refuses a bad platform" 'platform must be' "$OUT"
OUT=$(mcp_call pull_image '{"image":"repo@notadigest"}')
assert_contains "pull_image refuses a malformed digest reference" 'digest references look like' "$OUT"
[ "$(fx_count)" = "$N" ] && pass "rejected pull params never reach the daemon" || fail "rejected pull params never reach the daemon" "fixture saw extra requests"

echo "== missing / invalid registry credential =="
fx_reset
OUT=$(mcp_call_on "$NOAUTH_BASE" pull_image '{"image":"private/secret"}')
assert_contains "pull denied without the credential is an actionable tool error" 'pull access denied' "$OUT"
assert_contains "missing-credential error names the env var" 'DOCKER_REGISTRY_AUTH' "$OUT"
assert_contains "missing-credential error names the secret ref" 'docker-mcp-registry-auth' "$OUT"
assert_contains "missing-credential error says where to get a token" 'app.docker.com/settings/personal-access-tokens' "$OUT"
assert_contains "missing-credential error names cosmonic_set_secret" 'cosmonic_set_secret' "$OUT"
assert_contains "missing-credential error is a tool error" '"isError":true' "$OUT"
assert_req "no X-Registry-Auth header is sent without a credential" '"x-registry-auth" not in last["headers"]'
OUT=$(mcp_call_on "$NOAUTH_BASE" version '{}')
assert_json "no-auth instance reports registry_auth_configured=false" 'r["result"]["structuredContent"]["registry_auth_configured"] is False and "registry_auth_valid" not in r["result"]["structuredContent"]' "$OUT"
assert_contains "no-auth version text says public pulls only" 'not configured (public pulls only)' "$OUT"

echo "== pre-encoded registry credential (standard base64, unpadded) =="
fx_reset
OUT=$(mcp_call_on "$PREENC_BASE" version '{}')
assert_json "pre-encoded credential is accepted by version" 'r["result"]["structuredContent"]["registry_auth_configured"] is True and r["result"]["structuredContent"]["registry_auth_valid"] is True' "$OUT"
OUT=$(mcp_call_on "$PREENC_BASE" pull_image '{"image":"private/secret:1"}')
assert_json "pre-encoded credential pulls the private image" 'r["result"].get("isError") is False and r["result"]["structuredContent"]["registry_auth_sent"] is True' "$OUT"
assert_req "pre-encoded credential is re-emitted as padded url-safe base64 ('+'/'/' -> '-'/'_', '=' restored)" "last[\"headers\"][\"x-registry-auth\"] == \"${PREENC_EXPECT}\" and b64strict(last[\"headers\"][\"x-registry-auth\"]) == json.loads('${PREENC_JSON}')"
assert_not_contains "pre-encoded credential never appears in tool output" 'x>?a?' "$OUT"
kill "$PREENC_PID" 2>/dev/null; wait "$PREENC_PID" 2>/dev/null; PREENC_PID=""
N=$(fx_count)
OUT=$(mcp_call_on "$OLDAPI_BASE" pull_image '{"image":"alpine:3.20"}')
assert_contains "placeholder credential is refused as misconfigured" 'neither a JSON object nor base64url' "$OUT"
assert_contains "invalid-credential error names the secret ref" 'docker-mcp-registry-auth' "$OUT"
assert_contains "invalid-credential error is a tool error" '"isError":true' "$OUT"
[ "$(fx_count)" = "$N" ] && pass "invalid credential is caught before dialling" || fail "invalid credential is caught before dialling" "fixture saw extra requests"

echo "== log hygiene at RUST_LOG=debug (no-auth instance) =="
fx_reset
OUT=$(mcp_call_on "$NOAUTH_BASE" run_container '{"image":"alpine:3.20","name":"dbg1","env":{"API_TOKEN":"hunter2-leak-test-debug"},"labels":{"k":"v"}}')
assert_json "debug instance runs a container with a secret-bearing env" 'r["result"]["structuredContent"]["started"] is True' "$OUT"
assert_req "debug instance sent the env to the daemon" 'any(q["raw_path"] == "/v1.44/containers/create" and "hunter2-leak-test-debug" in q["body"] for q in reqs)'
if grep -q 'tool.run_container' "$E2E_TMP/noauth.log"; then pass "debug log carries the tool.run_container span"; else fail "debug log carries the tool.run_container span" "span missing from noauth.log"; fi
if grep -q '"level":"DEBUG"' "$E2E_TMP/noauth.log"; then pass "debug instance really logs at DEBUG"; else fail "debug instance really logs at DEBUG" "no DEBUG records in noauth.log"; fi
if grep -q 'hunter2-leak-test-debug' "$E2E_TMP/noauth.log"; then fail "container Env never reaches the host log at RUST_LOG=debug" "found in noauth.log: $(grep -m1 hunter2-leak-test-debug "$E2E_TMP/noauth.log" | head -c 300)"; else pass "container Env never reaches the host log at RUST_LOG=debug"; fi
OUT=$(mcp_call_on "$NOAUTH_BASE" pull_image '{"image":"private/secret"}')
if grep -q 'PullImageParams\|RunContainerParams' "$E2E_TMP/noauth.log"; then fail "no params struct is ever serialised into a span" "found in noauth.log"; else pass "no params struct is ever serialised into a span"; fi
for f in server.log guard.log noauth.log oldapi.log preenc.log; do
  if grep -q 'x>?a?\|"password":"p"\|REPLACE_ME\|eyJwYXNzd29yZCI6InAi' "$E2E_TMP/$f" 2>/dev/null; then fail "registry credential never reaches the host log ($f)" "credential material found"; else pass "registry credential never reaches the host log ($f)"; fi
done

echo "== outbound timeout (slow pull) =="
OUT=$(mcp_call_on "$NOAUTH_BASE" pull_image '{"image":"slow:1"}')
assert_contains "slow pull maps the outbound timeout" 'timed out' "$OUT"
assert_contains "timeout hint mentions MCP_OUTBOUND_TIMEOUT_MS" 'MCP_OUTBOUND_TIMEOUT_MS' "$OUT"
assert_contains "timeout hint says to pre-pull" 'pre-pull' "$OUT"
OUT=$(mcp_call_on "$NOAUTH_BASE" version '{}')
assert_json "no-auth instance alive after the timeout" 'r["result"]["structuredContent"]["api_version_ok"] is True' "$OUT"

echo "== remove_image =="
fx_reset
OUT=$(mcp_call remove_image '{"name":"alpine:3.20"}')
assert_json "remove_image returns untagged/deleted" 'r["result"]["structuredContent"]["untagged"] == ["alpine:3.20"] and r["result"]["structuredContent"]["deleted"] == ["img200000000"] and r["result"]["structuredContent"]["changes"] == 2' "$OUT"
assert_req "remove_image DELETEs with force=false&noprune=false" 'last["method"] == "DELETE" and last["raw_path"] == "/v1.44/images/alpine:3.20" and last["query"] == {"force": ["false"], "noprune": ["false"]}'
OUT=$(mcp_call remove_image '{"name":"inuse:1"}')
assert_contains "image used by a stopped container maps to the force hint" 'must be forced' "$OUT"
assert_contains "must-be-forced hint says force=true" 'force=true' "$OUT"
OUT=$(mcp_call remove_image '{"name":"inuse:1","force":true,"noprune":true}')
assert_json "remove_image force=true succeeds" 'r["result"]["structuredContent"]["force"] is True and r["result"]["structuredContent"]["untagged"] == ["inuse:1"]' "$OUT"
assert_req "remove_image forwards force and noprune" 'last["query"] == {"force": ["true"], "noprune": ["true"]}'
OUT=$(mcp_call remove_image '{"name":"running:1"}')
assert_contains "image used by a running container says force does not help" 'force cannot override' "$OUT"
OUT=$(mcp_call remove_image '{"name":"gone:1"}')
assert_contains "remove of a missing image is the 404 hint" 'No such image' "$OUT"

echo "== list_networks / list_volumes / system_df =="
fx_reset
OUT=$(mcp_call list_networks '{}')
assert_json "list_networks trims rows with IPAM" 'r["result"]["structuredContent"]["count"] == 2 and r["result"]["structuredContent"]["networks"][0]["name"] == "bridge" and r["result"]["structuredContent"]["networks"][0]["ipam"][0]["subnet"] == "172.17.0.0/16" and r["result"]["structuredContent"]["networks"][1]["ipam"][1]["gateway"] == "fd00::1" and r["result"]["structuredContent"]["networks"][0]["containers"] == 1' "$OUT"
assert_req "list_networks GETs /networks without filters" 'last["raw_path"] == "/v1.44/networks" and last["raw_query"] == ""'
OUT=$(mcp_call list_networks '{"filters":{"driver":["bridge"],"type":"custom"}}')
assert_req "list_networks forwards JSON filters" 'json.loads(last["query"]["filters"][0]) == {"driver": ["bridge"], "type": ["custom"]}'
OUT=$(mcp_call list_networks '{"filters":{"status":["x"]}}')
assert_contains "list_networks refuses unknown filter keys" 'unknown filter key' "$OUT"
OUT=$(mcp_call list_volumes '{}')
assert_json "list_volumes trims rows and passes Warnings" 'r["result"]["structuredContent"]["count"] == 2 and r["result"]["structuredContent"]["volumes"][0]["name"] == "vol1" and r["result"]["structuredContent"]["volumes"][1]["usage"]["size"] == 20 and r["result"]["structuredContent"]["warnings"] == ["fixture volume warning"]' "$OUT"
OUT=$(mcp_call list_volumes '{"filters":{"dangling":["true"]}}')
assert_req "list_volumes forwards JSON filters" 'last["raw_path"] == "/v1.44/volumes" and json.loads(last["query"]["filters"][0]) == {"dangling": ["true"]}'
OUT=$(mcp_call list_volumes '{"filters":{"driver":[]}}')
assert_contains "list_volumes refuses an empty filter value list" 'has no values' "$OUT"
OUT=$(mcp_call system_df '{}')
assert_json "system_df computes image totals" 'r["result"]["structuredContent"]["images"] == {"total": 2, "active": 1, "size": 150, "size_human": "150 B", "reclaimable": 50, "reclaimable_human": "50 B", "reported": True}' "$OUT"
assert_json "system_df computes container/volume/build-cache totals" 'r["result"]["structuredContent"]["containers"]["size"] == 15 and r["result"]["structuredContent"]["containers"]["reclaimable"] == 5 and r["result"]["structuredContent"]["volumes"]["size"] == 50 and r["result"]["structuredContent"]["volumes"]["reclaimable"] == 20 and r["result"]["structuredContent"]["build_cache"]["reclaimable"] == 40 and r["result"]["structuredContent"]["layers_size"] == 150' "$OUT"
assert_json "system_df summary has no item lists" '"image_items" not in r["result"]["structuredContent"]' "$OUT"
OUT=$(mcp_call system_df '{"detail":true}')
assert_json "system_df detail lists the largest images first" 'r["result"]["structuredContent"]["image_items"][0]["size"] == 100 and r["result"]["structuredContent"]["container_items"][0]["names"] == ["web"] and r["result"]["structuredContent"]["volume_items"][1]["ref_count"] == 0' "$OUT"
assert_req "system_df GETs /system/df without parameters" 'last["raw_path"] == "/v1.44/system/df" and last["raw_query"] == ""'

echo "== read-only gate (guard instance, DOCKER_READ_ONLY unset) =="
fx_reset
for TOOL in "${WRITE_TOOLS[@]}"; do
  case "$TOOL" in
    run_container) ARGS='{"image":"alpine:3.20"}' ;;
    pull_image) ARGS='{"image":"alpine:3.20"}' ;;
    remove_image) ARGS='{"name":"alpine:3.20"}' ;;
    *) ARGS='{"id":"web"}' ;;
  esac
  OUT=$(mcp_call_on "$GUARD_BASE" "$TOOL" "$ARGS")
  assert_contains "$TOOL is refused under the read-only default" 'docker-mcp is read-only' "$OUT"
  assert_contains "$TOOL refusal names DOCKER_READ_ONLY and the tool" "enable $TOOL" "$OUT"
done
assert_contains "read-only refusal says nothing was sent" 'was not sent to the daemon' "$OUT"
assert_contains "read-only refusal is a tool error" '"isError":true' "$OUT"
[ "$(fx_count)" = "0" ] && pass "gated writes never dial the daemon" || fail "gated writes never dial the daemon" "fixture saw requests"
OUT=$(mcp_call_on "$GUARD_BASE" list_containers '{}')
assert_contains "read tools on the guard instance still dial (and report the transport error)" 'could not reach the Docker daemon' "$OUT"

# On Cosmonic Desktop a closed loopback door does not surface as a policy
# denial: host.wasmcloud.internal simply fails to resolve (wasi:http
# `DnsError ... "address not available"`, the same text wasmtime produces for
# any unresolvable name). The hint must name the door for that case.
echo "== unresolvable DOCKER_HOST (DNS failure hint) =="
"$WASMTIME" serve -Sp3,cli,http --env DOCKER_HOST=http://docker-mcp-e2e-no-such-host.invalid:2375 \
  --env RUST_LOG=info --addr "127.0.0.1:${DNS_PORT}" "$WASM" >"$E2E_TMP/dns.log" 2>&1 &
DNS_PID=$!
mcp_wait_ready "$DNS_PORT"
OUT=$(mcp_call_on "http://127.0.0.1:${DNS_PORT}/" version '{}')
assert_contains "unresolvable DOCKER_HOST is an actionable tool error" 'could not reach the Docker daemon' "$OUT"
assert_contains "DNS failure surfaces the wasi:http DnsError" 'DnsError' "$OUT"
assert_contains "DNS failure hint explains the closed loopback door" 'closed loopback door' "$OUT"
assert_contains "DNS failure hint distinguishes a typo in DOCKER_HOST" 'any other name means DOCKER_HOST has a typo' "$OUT"
assert_contains "DNS failure hint still quotes the Security toggle" 'allow host loopback' "$OUT"
assert_contains "DNS failure is a tool error" '"isError":true' "$OUT"
kill "$DNS_PID" 2>/dev/null; wait "$DNS_PID" 2>/dev/null; DNS_PID=""

if [ "${E2E_LIVE:-}" = "1" ]; then
  LIVE_HOST="${DOCKER_LIVE_HOST:-http://127.0.0.1:2375}"
  echo "== live (${LIVE_HOST}) =="
  "$WASMTIME" serve -Sp3,cli,http --env "DOCKER_HOST=${LIVE_HOST}" --env DOCKER_READ_ONLY=false \
    --env MCP_OUTBOUND_TIMEOUT_MS=120000 --env RUST_LOG=info \
    --addr "127.0.0.1:${LIVE_PORT}" "$WASM" >"$E2E_TMP/live.log" 2>&1 &
  LIVE_PID=$!
  mcp_wait_ready "$LIVE_PORT"
  SUFFIX="$$"
  OUT=$(mcp_call_on "$LIVE_BASE" version '{}')
  assert_json "live: version reports a docker or podman engine inside the window" 'r["result"]["structuredContent"]["engine"] in ("docker", "podman") and r["result"]["structuredContent"]["api_version_ok"] is True' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" info '{}')
  assert_json "live: info reports NCPU" 'r["result"]["structuredContent"]["NCPU"] >= 1' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" list_images '{}')
  assert_json "live: list_images works" 'r["result"]["structuredContent"]["count"] >= 0 and r["result"].get("isError") is False' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" pull_image '{"image":"docker.io/library/alpine:3.20"}')
  assert_json "live: pull_image alpine:3.20" 'r["result"].get("isError") is False and r["result"]["structuredContent"]["tag"] == "3.20"' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" run_container "{\"image\":\"docker.io/library/alpine:3.20\",\"name\":\"e2e-docker-mcp-${SUFFIX}\",\"cmd\":[\"sh\",\"-c\",\"echo hello-out; echo hello-err 1>&2\"],\"wait\":true}")
  assert_json "live: run_container wait demultiplexes stdout/stderr and returns exit 0" 'r["result"]["structuredContent"]["exit_code"] == 0 and "hello-out" in r["result"]["structuredContent"]["stdout"] and "hello-err" in r["result"]["structuredContent"]["stderr"] and "hello-err" not in r["result"]["structuredContent"]["stdout"]' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" container_logs "{\"id\":\"e2e-docker-mcp-${SUFFIX}\"}")
  assert_json "live: container_logs on the finished container" '"hello-out" in r["result"]["structuredContent"]["stdout"] and "hello-err" in r["result"]["structuredContent"]["stderr"]' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" inspect_container "{\"id\":\"e2e-docker-mcp-${SUFFIX}\"}")
  assert_json "live: inspect_container shows the exited state" 'r["result"]["structuredContent"]["state"]["Running"] is False' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" list_containers "{\"filters\":{\"name\":[\"e2e-docker-mcp-${SUFFIX}\"]}}")
  assert_json "live: list_containers filter by name finds it" 'any("e2e-docker-mcp-'"${SUFFIX}"'" in c["names"] for c in r["result"]["structuredContent"]["containers"])' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" remove_container "{\"id\":\"e2e-docker-mcp-${SUFFIX}\"}")
  assert_json "live: remove_container" 'r["result"]["structuredContent"]["removed"] is True' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" run_container "{\"image\":\"docker.io/library/alpine:3.20\",\"name\":\"e2e-docker-mcp-sleep-${SUFFIX}\",\"cmd\":[\"sleep\",\"30\"]}")
  assert_json "live: run_container detached" 'r["result"]["structuredContent"]["started"] is True' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" container_stats "{\"id\":\"e2e-docker-mcp-sleep-${SUFFIX}\"}")
  assert_json "live: container_stats on a running container" 'r["result"].get("isError") is False and r["result"]["structuredContent"]["memory_limit"] > 0' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" kill_container "{\"id\":\"e2e-docker-mcp-sleep-${SUFFIX}\"}")
  assert_json "live: kill_container" 'r["result"]["structuredContent"]["changed"] is True' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" remove_container "{\"id\":\"e2e-docker-mcp-sleep-${SUFFIX}\",\"force\":true}")
  assert_json "live: remove_container force" 'r["result"]["structuredContent"]["removed"] is True' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" pull_image '{"image":"docker.io/library/busybox:1.36"}')
  assert_json "live: pull_image busybox:1.36" 'r["result"].get("isError") is False' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" remove_image '{"name":"docker.io/library/busybox:1.36"}')
  assert_json "live: remove_image busybox:1.36" 'r["result"].get("isError") is False' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" list_networks '{}')
  assert_json "live: list_networks" 'r["result"]["structuredContent"]["count"] >= 1' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" list_volumes '{}')
  assert_json "live: list_volumes" 'r["result"].get("isError") is False' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" system_df '{}')
  assert_json "live: system_df" 'r["result"]["structuredContent"]["images"]["total"] >= 1' "$OUT"

  # A bogus credential must be PARSED by the real daemon and rejected by the
  # registry (401/403/404/500 with a registry message) — never a 400 "failed
  # to parse X-Registry-Auth", which is what an unpadded header produces.
  kill "$OLDAPI_PID" 2>/dev/null; wait "$OLDAPI_PID" 2>/dev/null; OLDAPI_PID=""
  echo "starting live bogus-credential instance on :${OLDAPI_PORT}..."
  "$WASMTIME" serve -Sp3,cli,http --env "DOCKER_HOST=${LIVE_HOST}" --env DOCKER_READ_ONLY=false \
    --env MCP_OUTBOUND_TIMEOUT_MS=120000 --env RUST_LOG=info \
    --env 'DOCKER_REGISTRY_AUTH={"username":"docker-mcp-e2e-nobody","password":"not-a-real-token","serveraddress":"index.docker.io"}' \
    --addr "127.0.0.1:${OLDAPI_PORT}" "$WASM" >"$E2E_TMP/liveauth.log" 2>&1 &
  LIVEAUTH_PID=$!
  mcp_wait_ready "$OLDAPI_PORT"
  OUT=$(mcp_call_on "$OLDAPI_BASE" version '{}')
  assert_json "live: bogus credential is well-formed as far as version can tell" 'r["result"]["structuredContent"]["registry_auth_valid"] is True' "$OUT"
  OUT=$(mcp_call_on "$OLDAPI_BASE" pull_image '{"image":"docker.io/library/docker-mcp-e2e-no-such-repo-8f3a:1"}')
  assert_contains "live: bogus credential pull is a tool error" '"isError":true' "$OUT"
  assert_not_contains "live: the daemon parsed the padded X-Registry-Auth (no 'failed to parse')" 'failed to parse' "$OUT"
  assert_not_contains "live: the daemon did not answer HTTP 400 to the auth header" 'HTTP 400' "$OUT"
  assert_not_contains "live: the pull error never echoes the credential" 'not-a-real-token' "$OUT"
  assert_contains "live: the registry-side failure carries the credential hint" 'docker-mcp-registry-auth' "$OUT"
  kill "$LIVEAUTH_PID" 2>/dev/null; wait "$LIVEAUTH_PID" 2>/dev/null; LIVEAUTH_PID=""
fi

guard_tests
mcp_harness_report
