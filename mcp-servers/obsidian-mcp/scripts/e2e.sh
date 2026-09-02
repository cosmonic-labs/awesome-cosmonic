#!/usr/bin/env bash
# End-to-end tests for obsidian-mcp. Framework checks (protocol, spec
# enforcement, discovery route, skills over MCP, robustness, Host guard) come
# from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases live
# below.
#
# Hermetic: scripts/obsidian_fixture.py impersonates the "Local REST API with
# MCP" plugin 5.1.0 on 127.0.0.1:FIXTURE_PORT and OBSIDIAN_BASE_URL points
# every instance at it. Two wasmtime instances run:
#   :PORT        key + OBSIDIAN_ENABLE_COMMANDS=true          — most cases
#   :GUARD_PORT  no key + OBSIDIAN_READ_ONLY=true (commands off)
#                — missing-secret path, gates, Host guard
#
# Usage: scripts/e2e.sh [--no-build]
#   E2E_LIVE=1 OBSIDIAN_API_KEY=<key> [OBSIDIAN_LIVE_URL=http://127.0.0.1:27123]
#   adds a read-only smoke against a running Obsidian with the HTTP listener on.
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9678}
GUARD_PORT=${GUARD_PORT:-9679}
FIXTURE_PORT=${FIXTURE_PORT:-9680}
LIVE_PORT=${LIVE_PORT:-9681}
PLACEHOLDER_PORT=${PLACEHOLDER_PORT:-9682}
WASM=${WASM:-target/wasm32-wasip2/release/obsidian_mcp.wasm}
SKILL_NAME=obsidian-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIXTURE_URL="http://127.0.0.1:${FIXTURE_PORT}"
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"
TODAY=$(date -u +%F)
YESTERDAY=$(date -u -d yesterday +%F 2>/dev/null || date -u -v-1d +%F)
Y_YEAR=${YESTERDAY%%-*}; Y_REST=${YESTERDAY#*-}; Y_MONTH=$((10#${Y_REST%%-*})); Y_DAY=$((10#${Y_REST#*-}))
LIVE_PID=""
PLACEHOLDER_PID=""

cleanup_all() {
  [ -n "$LIVE_PID" ] && kill "$LIVE_PID" 2>/dev/null
  [ -n "$PLACEHOLDER_PID" ] && kill "$PLACEHOLDER_PID" 2>/dev/null
  mcp_harness_cleanup
}
trap cleanup_all EXIT

# assert_reqs <name> <python-expr> — evaluates <expr> with `reqs` bound to the
# fixture's recorded requests (oldest first) and `last` to the newest.
assert_reqs() {
  local name="$1" expr="$2"
  if curl -sS "${FIXTURE_URL}/_fixture/requests" | python3 -c '
import json, sys
reqs = json.load(sys.stdin)
last = reqs[-1] if reqs else {}
prev = reqs[-2] if len(reqs) > 1 else {}
sys.exit(0 if eval(sys.argv[1]) else 1)
' "$expr" 2>/dev/null; then
    pass "$name"
  else
    fail "$name" "expression [$expr] false; last request: $(curl -sS "${FIXTURE_URL}/_fixture/requests" | python3 -c 'import json,sys; r=json.load(sys.stdin); print(json.dumps(r[-1]) if r else "none")' | head -c 500)"
  fi
}

# assert_failed <name> <out> — a tool-level error (isError) or a JSON-RPC error.
assert_failed() {
  case "$2" in
    *'"isError":true'* | *'"error":{'*) pass "$1" ;;
    *) fail "$1" "expected a failure, got: $2" ;;
  esac
}

fx_post() { curl -sS -X POST "${FIXTURE_URL}$1" -H 'Content-Type: application/json' -d "$2" >/dev/null; }
# Total upstream requests the fixture has seen (to prove a refusal sent nothing).
fx_count() { curl -sS "${FIXTURE_URL}/_fixture/count" | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])'; }
# assert_elapsed <name> <start-ns> <max-ms>
assert_elapsed() {
  local ms=$(( ($(date +%s%N) - $2) / 1000000 ))
  if [ "$ms" -le "$3" ]; then pass "$1 (${ms} ms)"; else fail "$1" "took ${ms} ms, limit $3 ms"; fi
}

# The concurrency test in framework_tests fires this tool 8x in parallel.
FIRST_TOOL_NAME=list_files_in_vault
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"Welcome.md"'

mcp_build_if_needed "${1:-}"

echo "starting fixture on :${FIXTURE_PORT}..."
python3 scripts/obsidian_fixture.py "$FIXTURE_PORT" >"$E2E_TMP/fixture.log" 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "${FIXTURE_URL}/" && break
  sleep 0.2
done

# The short outbound timeout keeps the slow-upstream case fast (fixture sleeps 6 s).
COMMON=(--env "OBSIDIAN_BASE_URL=${FIXTURE_URL}" --env RUST_LOG=info --env MCP_OUTBOUND_TIMEOUT_MS=3000)
mcp_harness_start "${COMMON[@]}" --env OBSIDIAN_API_KEY=test-key --env OBSIDIAN_ENABLE_COMMANDS=true
# Guard instance: no OBSIDIAN_API_KEY (missing-secret path), read-only, commands off.
mcp_harness_start_guard "${COMMON[@]}" --env OBSIDIAN_READ_ONLY=true

ALL_TOOLS=(check_auth get_server_info list_files_in_vault list_files_in_dir get_file_contents
  batch_get_file_contents simple_search complex_search get_recent_changes list_tags
  get_active_file append_content put_content patch_content delete_file get_periodic_note
  get_recent_periodic_notes open_file list_commands execute_command)

framework_tests "${ALL_TOOLS[@]}"
discovery_tests list_files_in_vault
skills_tests "$SKILL_NAME" references/TOOLS.md references/PATCH.md

echo "== discovery: credentials block =="
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / lists the obsidian-mcp-api-key credential" '"ref": "obsidian-mcp-api-key"' "$ROOT"
assert_contains "GET / names the env var" '"env": "OBSIDIAN_API_KEY"' "$ROOT"
assert_contains "GET / reports the key as configured (presence only)" '"status": "configured"' "$ROOT"
assert_contains "GET / names check_auth as the validator" '"validate": "check_auth"' "$ROOT"
assert_not_contains "GET / never leaks the key value" 'test-key' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD_BASE")
assert_contains "GET / on the guard instance reports the key missing" '"status": "missing"' "$ROOT"
assert_contains "GET / on the guard instance says the ref is not a placeholder" '"placeholder": false' "$ROOT"
if curl -sS --max-time 20 "$MCP_BASE" | python3 -c '
import json, sys, yaml
live = json.load(sys.stdin)["credentials"]
for c in live:
    for k in ("status", "validate", "placeholder"):
        c.pop(k, None)
ann = yaml.safe_load(open("deploy/workload.yaml"))["metadata"]["annotations"]["desktop.cosmonic.com/credentials"]
sys.exit(0 if json.loads(ann) == live else 1)
'; then
  pass "deploy/workload.yaml credentials annotation equals the GET / block (minus runtime fields)"
else
  fail "deploy/workload.yaml credentials annotation equals the GET / block (minus runtime fields)" "annotation and credentials() drifted"
fi

echo "== placeholder secret (OBSIDIAN_API_KEY=REPLACE_ME) =="
# A freshly registered ref on Desktop carries REPLACE_ME until the user fills it.
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env OBSIDIAN_API_KEY=REPLACE_ME \
  --addr "127.0.0.1:${PLACEHOLDER_PORT}" "$WASM" >"$E2E_TMP/placeholder.log" 2>&1 &
PLACEHOLDER_PID=$!
mcp_wait_ready "$PLACEHOLDER_PORT"
PH_BASE="http://127.0.0.1:${PLACEHOLDER_PORT}/"
ROOT=$(curl -sS --max-time 20 "$PH_BASE")
assert_contains "GET / reports a placeholder key as missing" '"status": "missing"' "$ROOT"
assert_contains "GET / flags the placeholder" '"placeholder": true' "$ROOT"
N0=$(fx_count)
OUT=$(mcp_call_on "$PH_BASE" list_files_in_vault '{}')
assert_contains "placeholder key is a tool error naming the ref and the value" 'obsidian-mcp-api-key` secret still holds the placeholder value REPLACE_ME' "$OUT"
assert_contains "placeholder error carries the registration command" 'cosmonic_set_secret' "$OUT"
[ "$(fx_count)" = "$N0" ] && pass "placeholder key sends nothing upstream" || fail "placeholder key sends nothing upstream" "fixture saw a request"
OUT=$(mcp_call_on "$PH_BASE" check_auth '{}')
assert_json "check_auth reports missing + placeholder" 'r["result"]["structuredContent"]["status"] == "missing" and r["result"]["structuredContent"]["placeholder"] is True and "REPLACE_ME" in r["result"]["structuredContent"]["remediation"]' "$OUT"
kill "$PLACEHOLDER_PID" 2>/dev/null; wait "$PLACEHOLDER_PID" 2>/dev/null; PLACEHOLDER_PID=""

echo "== check_auth / get_server_info =="
OUT=$(mcp_call check_auth '{}')
assert_json "check_auth reports status ok" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
assert_json "check_auth names plugin + Obsidian versions" 'r["result"]["structuredContent"]["identity"]["plugin_version"] == "5.1.0" and r["result"]["structuredContent"]["identity"]["obsidian_version"] == "1.13.4"' "$OUT"
assert_json "check_auth carries ref/env" 'r["result"]["structuredContent"]["ref"] == "obsidian-mcp-api-key" and r["result"]["structuredContent"]["env"] == "OBSIDIAN_API_KEY"' "$OUT"
assert_reqs "check_auth sent the Bearer header to GET /" 'last["raw_path"] == "/" and last["headers"].get("Authorization") == "Bearer test-key"'
OUT=$(mcp_call get_server_info '{}')
assert_json "get_server_info reports authenticated=true and json-v2" 'r["result"]["structuredContent"]["authenticated"] is True and r["result"]["structuredContent"]["patch_format"] == "json-v2"' "$OUT"
assert_json "get_server_info reports versions.self" 'r["result"]["structuredContent"]["versions"]["self"] == "5.1.0"' "$OUT"
assert_json "get_server_info reports the gates" 'r["result"]["structuredContent"]["read_only"] is False and r["result"]["structuredContent"]["commands_enabled"] is True' "$OUT"

echo "== missing secret (guard instance) =="
OUT=$(mcp_call_on "$GUARD_BASE" list_files_in_vault '{}')
assert_contains "missing OBSIDIAN_API_KEY is an actionable tool error" 'OBSIDIAN_API_KEY is not set' "$OUT"
assert_contains "missing-key error names the secret ref" 'obsidian-mcp-api-key' "$OUT"
assert_contains "missing-key error says where the key comes from" 'Local REST API with MCP' "$OUT"
assert_contains "missing-key error names cosmonic_set_secret" 'cosmonic_set_secret' "$OUT"
assert_contains "missing-key error is a tool error (isError)" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" check_auth '{}')
assert_json "check_auth on the guard instance reports status missing" 'r["result"]["structuredContent"]["status"] == "missing" and "cosmonic_set_secret" in r["result"]["structuredContent"]["remediation"]' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" get_server_info '{}')
assert_json "get_server_info works without a key (auth-exempt) and says so" 'r["result"]["structuredContent"]["authenticated"] is False and r["result"]["structuredContent"]["key_configured"] is False and any("OBSIDIAN_API_KEY" in h for h in r["result"]["structuredContent"]["hints"])' "$OUT"

echo "== invalid secret (fixture rotates the accepted key) =="
fx_post /_fixture/key '{"key":"rotated"}'
OUT=$(mcp_call list_files_in_vault '{}')
assert_contains "401 maps to the re-register hint" 'HTTP 401' "$OUT"
assert_contains "401 keeps the upstream message" 'Authorization required' "$OUT"
assert_contains "401 names the secret ref" 'obsidian-mcp-api-key' "$OUT"
assert_not_contains "401 error never echoes the key" 'test-key' "$OUT"
OUT=$(mcp_call check_auth '{}')
assert_json "check_auth reports status invalid on a rejected key" 'r["result"]["structuredContent"]["status"] == "invalid" and "obsidian-mcp-api-key" in r["result"]["structuredContent"]["remediation"]' "$OUT"
OUT=$(mcp_call get_server_info '{}')
assert_json "get_server_info reports authenticated=false with a hint" 'r["result"]["structuredContent"]["authenticated"] is False and any("authenticated=false" in h for h in r["result"]["structuredContent"]["hints"])' "$OUT"
fx_post /_fixture/key '{"key":"test-key"}'

echo "== listing =="
OUT=$(mcp_call list_files_in_vault '{}')
assert_json "vault root lists notes and folders" '"Welcome.md" in r["result"]["structuredContent"]["entries"] and "Projects/" in r["result"]["structuredContent"]["folders"]' "$OUT"
assert_reqs "root listing hits /vault/ with the trailing slash" 'last["raw_path"] == "/vault/"'
OUT=$(mcp_call list_files_in_dir '{"dirpath":"Projects"}')
assert_json "folder listing returns direct children" 'r["result"]["structuredContent"]["entries"] == ["Archive/", "Plan.md"]' "$OUT"
assert_reqs "folder listing appends the mandatory trailing slash" 'last["raw_path"] == "/vault/Projects/"'
OUT=$(mcp_call list_files_in_dir '{"dirpath":"Projects/Archive/"}')
assert_json "a trailing slash in dirpath is accepted" 'r["result"]["structuredContent"]["entries"] == ["Old.md"]' "$OUT"
OUT=$(mcp_call list_files_in_dir '{"dirpath":""}')
assert_json "empty dirpath lists the root" '"Welcome.md" in r["result"]["structuredContent"]["entries"]' "$OUT"
OUT=$(mcp_call list_files_in_dir '{"dirpath":"Notes/Héllo Wörld"}')
assert_json "unicode folder lists its note" 'r["result"]["structuredContent"]["entries"] == ["Tôdo.md"]' "$OUT"
assert_reqs "unicode folder is percent-encoded per segment" 'last["raw_path"] == "/vault/Notes/H%C3%A9llo%20W%C3%B6rld/"'
OUT=$(mcp_call list_files_in_dir '{"dirpath":"Nope"}')
assert_contains "missing folder maps 404 to the empty-or-missing hint" 'empty or does not exist' "$OUT"
OUT=$(mcp_call list_files_in_dir '{"dirpath":"../etc"}')
assert_contains "'..' is refused in-guest" 'vault-relative' "$OUT"
OUT=$(mcp_call list_files_in_dir '{"dirpath":"/Projects"}')
assert_contains "leading '/' is refused in-guest" 'drop the leading' "$OUT"
OUT=$(mcp_call list_files_in_dir '{"dirpath":"x\r\nAuthorization: y"}')
assert_failed "injection-shaped path is not found" "$OUT"
assert_reqs "CRLF in a path is percent-encoded, not split into a header" 'last["raw_path"] == "/vault/x%0D%0AAuthorization%3A%20y/"'
OUT=$(mcp_call list_files_in_dir '{}')
assert_failed "missing dirpath is rejected" "$OUT"

echo "== get_file_contents =="
OUT=$(mcp_call get_file_contents '{"filepath":"Welcome.md"}')
assert_contains "reads markdown" 'This is the welcome note' "$OUT"
assert_json "markdown read is structured" 'r["result"]["structuredContent"]["format"] == "markdown" and r["result"]["structuredContent"]["truncated"] is False' "$OUT"
assert_reqs "markdown read sends Accept: text/markdown" 'last["headers"].get("Accept") == "text/markdown"'
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","format":"metadata"}')
assert_json "metadata read parses tags and stat" '"project" in r["result"]["structuredContent"]["data"]["tags"] and isinstance(r["result"]["structuredContent"]["data"]["stat"]["mtime"], int)' "$OUT"
assert_reqs "metadata read negotiates note+json" 'last["headers"].get("Accept") == "application/vnd.olrapi.note+json"'
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","format":"document_map"}')
assert_json "document map has headings and a version token" 'r["result"]["structuredContent"]["data"]["headings"][0]["heading"] == "Overview" and r["result"]["structuredContent"]["data"]["version"] == "v1"' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","heading":["Overview","Details"]}')
assert_contains "heading section read returns the section" 'Some details here' "$OUT"
assert_not_contains "heading section read excludes sibling sections" '2026-09-01 started' "$OUT"
assert_reqs "heading path becomes /heading/<a>/<b>" 'last["raw_path"] == "/vault/Projects/Plan.md/heading/Overview/Details"'
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","heading":["Overview","Details"],"scope":"marker"}')
assert_contains "scope=marker returns the heading line" '## Details' "$OUT"
assert_reqs "scope travels as Target-Scope" 'last["headers"].get("Target-Scope") == "marker"'
OUT=$(mcp_call get_file_contents '{"filepath":"Notes/Héllo Wörld/Tôdo.md","heading":["TODO/DONE"]}')
assert_contains "unicode note + heading with a slash resolves" 'ünïcode task' "$OUT"
assert_reqs "a slash inside a heading is %2F within its own segment" 'last["raw_path"] == "/vault/Notes/H%C3%A9llo%20W%C3%B6rld/T%C3%B4do.md/heading/TODO%2FDONE"'
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","block":"^abc123"}')
assert_contains "block read returns the paragraph" 'Some details here' "$OUT"
assert_reqs "block id is sent bare" 'last["raw_path"] == "/vault/Projects/Plan.md/block/abc123"'
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","frontmatter_key":"status"}')
assert_contains "frontmatter field read returns the value" 'active' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","heading":["Nope"]}')
assert_contains "missing heading maps 404 with the document-map hint" 'document_map' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"big.md"}')
assert_json "large multibyte note is truncated on a char boundary" 'r["result"]["structuredContent"]["truncated"] is True and "[truncated" in r["result"]["structuredContent"]["content"] and len(r["result"]["structuredContent"]["content"]) < 100100' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Binary.png"}')
assert_contains "binary attachment is refused with its content type" 'image/png' "$OUT"
assert_contains "binary refusal is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","heading":["Overview"],"block":"abc123"}')
assert_contains "two targets at once are refused" 'at most one' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Projects/Plan.md","format":"metadata","heading":["Overview"]}')
assert_contains "targets need format=markdown" 'only apply to format=markdown' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Welcome.md","format":"pdf"}')
assert_failed "unknown format enum is rejected" "$OUT"
OUT=$(mcp_call get_file_contents '{}')
assert_failed "missing filepath is rejected" "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Missing.md"}')
assert_contains "missing note is a 404 tool error" 'HTTP 404' "$OUT"

echo "== batch_get_file_contents =="
OUT=$(mcp_call batch_get_file_contents '{"filepaths":["Welcome.md","Projects/Plan.md","Missing.md","Welcome.md"]}')
assert_contains "batch concatenates with # path headers" '# Welcome.md' "$OUT"
assert_contains "batch separates entries with ---" '---' "$OUT"
assert_contains "batch inlines a 404 and continues" 'Error 404' "$OUT"
assert_json "batch de-duplicates and reports per-file status" 'r["result"]["structuredContent"]["count"] == 3 and r["result"]["structuredContent"]["requested"] == 4 and [f["ok"] for f in r["result"]["structuredContent"]["files"]] == [True, True, False]' "$OUT"
MANY=$(python3 -c 'import json; print(json.dumps({"filepaths": ["Missing/%d.md" % i for i in range(25)]}))')
OUT=$(mcp_call batch_get_file_contents "$MANY")
assert_json "batch clamps 25 paths to 20" 'r["result"]["structuredContent"]["count"] == 20 and r["result"]["structuredContent"]["clamped"] is True' "$OUT"
OUT=$(mcp_call batch_get_file_contents '{"filepaths":[]}')
assert_contains "empty batch is refused" 'at least one' "$OUT"
OUT=$(mcp_call batch_get_file_contents '{"filepaths":["../secret.md","Welcome.md"]}')
assert_json "invalid path inside a batch is inlined, not fatal" 'r["result"]["structuredContent"]["files"][0]["ok"] is False and r["result"]["structuredContent"]["files"][1]["ok"] is True' "$OUT"
DUPS=$(python3 -c 'import json; print(json.dumps({"filepaths": ["Welcome.md"] * 200}))')
OUT=$(mcp_call batch_get_file_contents "$DUPS")
assert_json "200 entries (all one path) are accepted and read once" 'r["result"]["structuredContent"]["requested"] == 200 and r["result"]["structuredContent"]["count"] == 1 and r["result"]["structuredContent"]["clamped"] is False' "$OUT"
MANY=$(python3 -c 'import json; print(json.dumps({"filepaths": ["Missing/%d.md" % i for i in range(201)]}))')
N0=$(fx_count)
OUT=$(mcp_call batch_get_file_contents "$MANY")
assert_contains "201 entries are refused in-guest" 'at most 200' "$OUT"
assert_contains "the refusal says nothing was sent" 'Nothing was sent' "$OUT"
[ "$(fx_count)" = "$N0" ] && pass "refused batch sent nothing upstream" || fail "refused batch sent nothing upstream" "fixture saw a request"
HUGE=$(python3 -c 'import json; print(json.dumps({"filepaths": ["Path/%06d.md" % i for i in range(60000)]}))')
T0=$(date +%s%N)
OUT=$(mcp_call batch_get_file_contents "$HUGE")
assert_elapsed "60k-entry (1 MiB) list is refused in bounded time" "$T0" 3000
assert_contains "60k-entry list is refused with the bound" 'filepaths has 60000 entries' "$OUT"
OUT=$(mcp_call list_files_in_vault '{}')
assert_contains "server alive after the 60k-entry refusal" '"Welcome.md"' "$OUT"
# Slow-but-answering upstream (2.2 s per file, under the 3 s per-exchange
# deadline): the call's wall-clock budget is 2 x MCP_OUTBOUND_TIMEOUT_MS = 6 s,
# so reads 1-3 happen (0, 2.2, 4.4 s) and the rest are reported, not attempted.
OUT=$(mcp_call batch_get_file_contents '{"filepaths":["Slow/1.md","Slow/2.md","Slow/3.md","Slow/4.md","Slow/5.md","Welcome.md"]}')
assert_json "batch stops issuing reads once the call budget is spent" 'r["result"]["structuredContent"]["budget_ms"] == 6000 and r["result"]["structuredContent"]["not_attempted"] == 3 and [f["ok"] for f in r["result"]["structuredContent"]["files"]] == [True, True, True, False, False, False] and r["result"]["structuredContent"]["files"][5]["skipped"] is True' "$OUT"
assert_contains "not-attempted entries say why and what to do" 'time budget was spent' "$OUT"
assert_contains "not-attempted entries say to re-issue the rest" 're-issue the batch with the remaining paths' "$OUT"

echo "== simple_search =="
OUT=$(mcp_call simple_search '{"query":"Plan"}')
assert_json "simple_search ranks results with snippets" 'r["result"]["structuredContent"]["results"][0]["filename"] == "Projects/Plan.md" and len(r["result"]["structuredContent"]["results"][0]["matches"]) >= 1' "$OUT"
assert_reqs "simple_search POSTs query params with defaults" 'last["method"] == "POST" and last["raw_path"] == "/search/simple/?query=Plan&contextLength=100"'
OUT=$(mcp_call simple_search '{"query":"héllo wörld & more","context_length":5000}')
assert_json "query is echoed back decoded (encoding round-trips)" '"[query=héllo wörld & more]" in r["result"]["structuredContent"]["results"][0]["matches"][0]["context"]' "$OUT"
assert_reqs "query value is percent-encoded and contextLength clamped to 1000" 'last["raw_path"] == "/search/simple/?query=h%C3%A9llo%20w%C3%B6rld%20%26%20more&contextLength=1000"'
OUT=$(mcp_call simple_search '{"query":"many","limit":5,"context_length":-3}')
assert_json "simple_search clamps files and matches and reports truncation" 'r["result"]["structuredContent"]["total_files"] == 501 and r["result"]["structuredContent"]["returned"] == 5 and r["result"]["structuredContent"]["truncated"] is True and len(r["result"]["structuredContent"]["results"][0]["matches"]) == 10 and r["result"]["structuredContent"]["results"][0]["total_matches"] == 20 and r["result"]["structuredContent"]["context_length"] == 0' "$OUT"
OUT=$(mcp_call simple_search '{"query":"many","limit":1000,"max_matches_per_file":1}')
assert_json "limit 1000 clamps to 100" 'r["result"]["structuredContent"]["limit"] == 100 and r["result"]["structuredContent"]["returned"] == 100 and len(r["result"]["structuredContent"]["results"][0]["matches"]) == 1' "$OUT"
OUT=$(mcp_call simple_search '{"query":"   "}')
assert_contains "blank query is refused in-guest" 'must not be empty' "$OUT"
LONGQ=$(python3 -c 'print("q" * 5000, end="")')
OUT=$(mcp_call simple_search "{\"query\":\"$LONGQ\"}")
assert_contains "5000-char query is refused in-guest" 'longer than 1000' "$OUT"
OUT=$(mcp_call simple_search '{"query":"boom"}')
assert_contains "500 errorCode 50010 maps to the retry-once hint" '50010' "$OUT"
assert_contains "50010 hint says retry once" 'Retry once' "$OUT"

echo "== complex_search / get_recent_changes =="
OUT=$(mcp_call complex_search '{"query":{"in":["project",{"var":"tags"}]}}')
assert_json "JsonLogic tag query returns matching notes" 'sorted(x["filename"] for x in r["result"]["structuredContent"]["results"]) == ["Projects/Archive/Old.md", "Projects/Plan.md"]' "$OUT"
assert_reqs "complex_search sends the jsonlogic content type" 'last["headers"].get("Content-Type") == "application/vnd.olrapi.jsonlogic+json" and last["raw_path"] == "/search/"'
OUT=$(mcp_call complex_search '{"query":{"glob":["Daily/*",{"var":"path"}]}}')
assert_json "glob on path works" 'r["result"]["structuredContent"]["total"] == 2' "$OUT"
OUT=$(mcp_call complex_search '{"query":{"var":"content"},"limit":1000}')
assert_json "content results are truncated to 2000 chars" 'any(x["filename"] == "big.md" and "[truncated" in x["result"] for x in r["result"]["structuredContent"]["results"]) and r["result"]["structuredContent"]["limit"] == 500' "$OUT"
OUT=$(mcp_call complex_search '{"query":{"var":"path"},"limit":1}')
assert_json "complex_search limit truncates and reports the total" 'r["result"]["structuredContent"]["returned"] == 1 and r["result"]["structuredContent"]["truncated"] is True and r["result"]["structuredContent"]["total"] >= 8' "$OUT"
OUT=$(mcp_call complex_search '{"query":"tags"}')
assert_contains "non-object query is refused" 'JSON object' "$OUT"
HUGE=$(python3 -c 'import json; print(json.dumps({"query": {"in": ["x" * 70000, {"var": "path"}]}}))')
OUT=$(mcp_call complex_search "$HUGE")
assert_contains "oversized JsonLogic body is refused in-guest" '65536' "$OUT"
OUT=$(mcp_call complex_search '{"query":{"nope":[1]}}')
assert_contains "unknown operator maps 40070 to the operators hint" '40070' "$OUT"
OUT=$(mcp_call get_recent_changes '{"days":2}')
assert_json "recent changes are newest first with ISO times" 'r["result"]["structuredContent"]["notes"][0]["path"] == "Daily/'"$TODAY"'.md" and r["result"]["structuredContent"]["notes"][0]["mtime"].endswith("Z") and any(n["path"] == "Projects/Plan.md" for n in r["result"]["structuredContent"]["notes"])' "$OUT"
assert_reqs "recent changes is a stat.mtime JsonLogic query" '"stat.mtime" in last["body"] and last["raw_path"] == "/search/"'
OUT=$(mcp_call get_recent_changes '{"days":0,"limit":1}')
assert_json "days clamps to 1 and limit 1 truncates" 'r["result"]["structuredContent"]["days"] == 1 and r["result"]["structuredContent"]["returned"] == 1 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call get_recent_changes '{"days":99999,"limit":99999}')
assert_json "days/limit clamp to 3650/100" 'r["result"]["structuredContent"]["days"] == 3650 and r["result"]["structuredContent"]["limit"] == 100' "$OUT"

echo "== list_tags / get_active_file =="
OUT=$(mcp_call list_tags '{}')
assert_json "tags include nested tags and their parents" 'set(t["name"] for t in r["result"]["structuredContent"]["tags"]) >= {"project", "work/tasks", "work", "welcome"}' "$OUT"
OUT=$(mcp_call list_tags '{"prefix":"#Work","limit":1}')
assert_json "prefix filter is case-insensitive and limit truncates" 'r["result"]["structuredContent"]["total"] == 2 and r["result"]["structuredContent"]["returned"] == 1 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call list_tags '{"limit":0}')
assert_json "limit 0 clamps to 1" 'r["result"]["structuredContent"]["returned"] == 1' "$OUT"
OUT=$(mcp_call get_active_file '{}')
assert_json "active file reports its path from Content-Location" 'r["result"]["structuredContent"]["path"] == "Projects/Plan.md" and "Overview" in r["result"]["structuredContent"]["content"]' "$OUT"
OUT=$(mcp_call get_active_file '{"format":"metadata"}')
assert_json "active file metadata" 'r["result"]["structuredContent"]["data"]["path"] == "Projects/Plan.md"' "$OUT"
OUT=$(mcp_call get_active_file '{"format":"document_map"}')
assert_contains "active file refuses document_map" 'not available' "$OUT"

echo "== append_content / put_content =="
OUT=$(mcp_call append_content '{"filepath":"Welcome.md","content":"Appended line."}')
assert_json "append reports bytes" 'r["result"]["structuredContent"]["status"] == "appended" and r["result"]["structuredContent"]["bytes"] == 14' "$OUT"
assert_reqs "append POSTs bare text/markdown (no charset parameter: the plugin matches it exactly)" 'last["method"] == "POST" and last["raw_path"] == "/vault/Welcome.md" and last["headers"].get("Content-Type") == "text/markdown" and "Reject-If-Content-Preexists" not in last["headers"]'
# Prove the fixture enforces the plugin's exact-match guard, so the assertion above is not vacuous.
FX_CT=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${FIXTURE_URL}/vault/Projects/Plan.md/heading/Overview" -H 'Authorization: Bearer test-key' -H 'Content-Type: text/markdown; charset=utf-8' -d 'x')
if [ "$FX_CT" = "400" ]; then pass "fixture rejects 'text/markdown; charset=utf-8' on a targeted write like the plugin (400 40012)"; else fail "fixture Content-Type guard" "got HTTP $FX_CT"; fi
FX_CT=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${FIXTURE_URL}/vault/Projects/Plan.md/heading/Overview" -H 'Authorization: Bearer test-key' -H 'Content-Type: text/markdown' -d 'x')
if [ "$FX_CT" = "200" ]; then pass "fixture accepts bare text/markdown on a targeted write"; else fail "fixture Content-Type guard (bare)" "got HTTP $FX_CT"; fi
OUT=$(mcp_call get_file_contents '{"filepath":"Welcome.md"}')
assert_contains "appended text is readable back" 'Appended line.' "$OUT"
# The plugin honours Reject-If-Content-Preexists only on targeted writes; an
# untargeted append with the flag is emulated in-guest (GET probe, then POST or refuse).
BEFORE=$(fx_count)
OUT=$(mcp_call append_content '{"filepath":"Welcome.md","content":"Appended line.","reject_if_content_preexists":true}')
assert_contains "untargeted duplicate append with the reject flag is refused in-guest" 'already present' "$OUT"
assert_contains "the refusal explains the plugin only honours the flag on heading targets" 'heading-targeted' "$OUT"
assert_reqs "the refusal read the note and sent no POST" 'last["method"] == "GET" and last["raw_path"] == "/vault/Welcome.md" and last["headers"].get("Accept") == "text/markdown"'
if [ "$(fx_count)" -eq $((BEFORE + 1)) ]; then pass "exactly one upstream request (the probe) was sent for the refusal"; else fail "refusal request count" "before=$BEFORE after=$(fx_count)"; fi
OUT=$(mcp_call append_content '{"filepath":"Welcome.md","content":"  Appended line.  ","reject_if_content_preexists":true}')
assert_contains "the in-guest check compares trimmed content" 'already present' "$OUT"
OUT=$(mcp_call append_content '{"filepath":"Welcome.md","content":"Second line with flag.","reject_if_content_preexists":true}')
assert_json "untargeted append with the flag and new content proceeds and reports the note-read check" 'r["result"]["structuredContent"]["status"] == "appended" and r["result"]["structuredContent"]["duplicate_check"] == "note-read"' "$OUT"
assert_reqs "the probe GET preceded a POST that carries no reject header (the plugin ignores it there)" 'prev["method"] == "GET" and prev["raw_path"] == "/vault/Welcome.md" and last["method"] == "POST" and last["raw_path"] == "/vault/Welcome.md" and "Reject-If-Content-Preexists" not in last["headers"] and last["headers"].get("Content-Type") == "text/markdown"'
OUT=$(mcp_call get_file_contents '{"filepath":"Welcome.md"}')
assert_contains "the flagged append landed" 'Second line with flag.' "$OUT"
OUT=$(mcp_call append_content '{"filepath":"Flagged/Note.md","content":"# Flagged\n","reject_if_content_preexists":true}')
assert_json "the flag on a missing note creates it (404 probe, then POST)" 'r["result"]["structuredContent"]["status"] == "appended" and r["result"]["structuredContent"]["duplicate_check"] == "note-read"' "$OUT"
assert_reqs "missing-note probe then POST" 'prev["method"] == "GET" and prev["raw_path"] == "/vault/Flagged/Note.md" and last["method"] == "POST" and last["raw_path"] == "/vault/Flagged/Note.md"'
OUT=$(mcp_call append_content '{"filepath":"Welcome.md","content":"plain append","reject_if_content_preexists":false}')
assert_json "reject flag false sends no probe and reports no duplicate check" 'r["result"]["structuredContent"]["status"] == "appended" and r["result"]["structuredContent"]["duplicate_check"] is None' "$OUT"
assert_reqs "reject flag false: POST without a preceding probe" 'last["method"] == "POST" and prev["method"] != "GET"'
OUT=$(mcp_call append_content '{"filepath":"Projects/Plan.md","content":"- 2026-09-03 appended","heading":["Overview","Log"]}')
assert_json "targeted append returns the updated note" '"2026-09-03 appended" in r["result"]["structuredContent"]["updated_content"] and r["result"]["structuredContent"]["duplicate_check"] is None' "$OUT"
assert_reqs "targeted append uses the heading URL with bare text/markdown and no reject header" 'last["raw_path"] == "/vault/Projects/Plan.md/heading/Overview/Log" and last["headers"].get("Content-Type") == "text/markdown" and "Reject-If-Content-Preexists" not in last["headers"]'
OUT=$(mcp_call append_content '{"filepath":"Projects/Plan.md","content":"- 2026-09-03 appended","heading":["Overview","Log"],"reject_if_content_preexists":true}')
assert_contains "duplicate heading-targeted append with the flag maps the plugin's 409 to 'already present'" 'already present' "$OUT"
assert_reqs "on a heading target the flag travels as the header and the plugin decides (no probe)" 'last["method"] == "POST" and last["raw_path"] == "/vault/Projects/Plan.md/heading/Overview/Log" and last["headers"].get("Reject-If-Content-Preexists") == "true" and prev["raw_path"] != "/vault/Projects/Plan.md"'
OUT=$(mcp_call append_content '{"filepath":"Projects/Plan.md","content":"- 2026-09-04 flagged","heading":["Overview","Log"],"reject_if_content_preexists":true}')
assert_json "heading-targeted append with the flag and new content reports the plugin check" 'r["result"]["structuredContent"]["status"] == "appended" and r["result"]["structuredContent"]["duplicate_check"] == "plugin" and "2026-09-04 flagged" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
OUT=$(mcp_call append_content '{"filepath":"New/Fresh.md","content":"# Fresh\n"}')
assert_json "append creates a missing note" 'r["result"]["structuredContent"]["status"] == "appended"' "$OUT"
OUT=$(mcp_call list_files_in_dir '{"dirpath":"New"}')
assert_json "created note appears in its folder" 'r["result"]["structuredContent"]["entries"] == ["Fresh.md"]' "$OUT"
OUT=$(mcp_call append_content '{"filepath":"Projects","content":"x","allow_non_markdown":true}')
assert_contains "append to a folder maps 405 to the folder hint" 'folder' "$OUT"
OUT=$(mcp_call append_content '{"filepath":"notes.txt","content":"x"}')
assert_contains "non-.md path is refused without allow_non_markdown" 'allow_non_markdown' "$OUT"
OUT=$(mcp_call append_content '{"filepath":"Welcome.md","content":""}')
assert_contains "empty append content is refused" 'must not be empty' "$OUT"
BIG=$(python3 -c 'print("x" * 1100000, end="")')
OUT=$(mcp_call append_content "{\"filepath\":\"Welcome.md\",\"content\":\"$BIG\"}")
assert_contains "1.1 MiB content is refused in-guest" '1 MiB' "$OUT"
OUT=$(mcp_call put_content '{"filepath":"Tmp/one.md","content":"# One\n"}')
assert_json "put creates a note" 'r["result"]["structuredContent"]["status"] == "written" and r["result"]["structuredContent"]["bytes"] == 6' "$OUT"
assert_reqs "put uses PUT with bare text/markdown" 'last["method"] == "PUT" and last["raw_path"] == "/vault/Tmp/one.md" and last["headers"].get("Content-Type") == "text/markdown"'
OUT=$(mcp_call put_content '{"filepath":"Tmp/two.md","content":""}')
assert_json "put accepts empty content" 'r["result"]["structuredContent"]["bytes"] == 0' "$OUT"
OUT=$(mcp_call put_content '{"filepath":"Nope/x.md","content":"x","require_existing":true}')
assert_contains "require_existing refuses to create" 'does not exist' "$OUT"
assert_reqs "require_existing probe sent no PUT" 'last["method"] == "GET"'
OUT=$(mcp_call put_content '{"filepath":"Tmp/one.md","content":"# One v2\n","require_existing":true}')
assert_json "require_existing overwrites an existing note" 'r["result"]["structuredContent"]["status"] == "written"' "$OUT"

echo "== patch_content (plugin 5.x JSON instruction) =="
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"],"content":"Appended detail line."}')
assert_json "heading append returns the updated note as json-v2" '"Appended detail line." in r["result"]["structuredContent"]["updated_content"] and r["result"]["structuredContent"]["patch_format"] == "json-v2" and r["result"]["structuredContent"]["warnings"] == []' "$OUT"
assert_reqs "patch sends the JSON instruction with an array heading target" 'last["method"] == "PATCH" and last["headers"].get("Content-Type") == "application/vnd.olrapi.patch-instruction+json" and json.loads(last["body"])["target"] == ["Overview", "Details"] and json.loads(last["body"])["targetType"] == "heading" and "content" in json.loads(last["body"]) and "value" not in json.loads(last["body"]) and "Target-Type" not in last["headers"]'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"replace","target_type":"frontmatter","target":"status","value":"done"}')
assert_json "frontmatter replace with a JSON value" '"status: done" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
assert_reqs "frontmatter patch sends value, not content" '"value" in json.loads(last["body"]) and "content" not in json.loads(last["body"]) and json.loads(last["body"])["target"] == "status"'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"frontmatter","target":"tags","value":["extra"]}')
assert_json "frontmatter list append merges" '"- extra" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"block","target":"^abc123","content":"after block"}')
assert_json "block append works with a bare id" '"after block" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
assert_reqs "block id is sent without the caret" 'json.loads(last["body"])["target"] == "abc123"'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"prepend","target_type":"heading","target":"Overview::Log","content":"- first"}')
assert_json "'A::B' string target is split into an array" '"- first" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
assert_reqs "split target is sent as an array" 'json.loads(last["body"])["target"] == ["Overview", "Log"]'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"replace","target_type":"heading","target":["Overview","Log"],"scope":"marker","content":"Journal"}')
assert_json "scope=marker renames a heading" '"## Journal" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
assert_reqs "scope travels in the instruction" 'json.loads(last["body"])["scope"] == "marker"'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"delete","target_type":"heading","target":["Overview","Journal"],"scope":"markerAndContent"}')
assert_json "delete needs no payload and removes the section" '"## Journal" not in r["result"]["structuredContent"]["updated_content"]' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"],"content":"x","if_match":"stale-token"}')
assert_contains "stale if_match maps 412 to the re-read hint" 'HTTP 412' "$OUT"
assert_contains "412 hint mentions the document map" 'document_map' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"],"content":"x","value":1}')
assert_contains "content and value together are refused in-guest" 'exactly one' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"]}')
assert_contains "append without a payload is refused in-guest" 'needs content' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"block","target":["a","b"],"content":"x"}')
assert_contains "array target for a block is refused" 'single string' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Nope"],"content":"x"}')
assert_contains "missing heading maps 404 with create_target_if_missing hint" 'create_target_if_missing' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["a","b","c","d","e","f"],"content":"deep","create_target_if_missing":true}')
assert_json "Markdown-Patch-Warnings header is decoded and surfaced" 'r["result"]["structuredContent"]["warnings"][0]["code"] == "heading-depth-overflow"' "$OUT"
assert_reqs "createTargetIfMissing travels in the instruction" 'json.loads(last["body"])["createTargetIfMissing"] is True'
OUT=$(mcp_call patch_content '{"filepath":"BadYaml.md","operation":"replace","target_type":"frontmatter","target":"title","value":"x"}')
assert_contains "invalid frontmatter maps 40005 to the fix-with-put hint" '40005' "$OUT"
assert_contains "40005 hint names put_content" 'put_content' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"sideways","target_type":"heading","target":["Overview"],"content":"x"}')
assert_failed "unknown operation enum is rejected" "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview"],"content":"x","if_match":"bad\ttoken"}')
assert_contains "if_match with control characters is refused" 'printable' "$OUT"

echo "== patch_content: 40081 surfaces when it is not a format mismatch =="
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"],"content":"INVALID-INSTRUCTION"}')
assert_contains "40081 maps to the patch-rules hint" '40081' "$OUT"
assert_contains "40081 hint points at PATCH.md" 'PATCH.md' "$OUT"
# The version-refresh GET / failing must not replace the patch diagnosis.
fx_post /_fixture/root '{"fail":true}'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"],"content":"INVALID-INSTRUCTION"}')
assert_contains "40081 still surfaces when the version refresh fails" '40081' "$OUT"
assert_not_contains "a failed refresh does not replace the diagnosis with the GET / error" 'status document unavailable' "$OUT"
fx_post /_fixture/root '{"fail":false}'

echo "== patch_content (plugin 4.x legacy headers) =="
# The plugin version is cached per warm instance (wasmtime serve keeps several,
# Desktop keeps poolSize). Switching the fixture's version makes some caches
# stale; patch_content refreshes on a 40081/40083/40084 and re-sends once, so
# every call below must land in the right format whichever instance answers.
fx_post /_fixture/version '{"version":"4.1.7"}'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"],"content":"legacy line","scope":"marker"}')
assert_json "legacy patch reports legacy-v1 and the deprecation note" 'r["result"]["structuredContent"]["patch_format"] == "legacy-v1" and "legacy line" in r["result"]["structuredContent"]["updated_content"] and any("deprecated" in n for n in r["result"]["structuredContent"]["notes"]) and any("ignored" in n for n in r["result"]["structuredContent"]["notes"])' "$OUT"
assert_reqs "legacy patch uses Operation/Target-Type/Target headers, '::'-joined and encoded" 'last["headers"].get("Operation") == "append" and last["headers"].get("Target-Type") == "heading" and last["headers"].get("Target") == "Overview%3A%3ADetails" and last["headers"].get("Content-Type") == "text/markdown" and last["body"] == "legacy line" and "Markdown-Patch-Version" not in last["headers"]'
OUT=$(mcp_call get_server_info '{}')
assert_json "get_server_info reports legacy-v1 for 4.x" 'r["result"]["structuredContent"]["patch_format"] == "legacy-v1" and any("plugin < 5.0" in h for h in r["result"]["structuredContent"]["hints"])' "$OUT"
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"replace","target_type":"frontmatter","target":"status","value":"legacy"}')
assert_json "legacy frontmatter replace succeeds" 'r["result"]["structuredContent"]["patch_format"] == "legacy-v1" and "status: legacy" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
assert_reqs "legacy frontmatter value goes as application/json" 'last["headers"].get("Content-Type") == "application/json" and last["headers"].get("Target") == "status" and last["body"] == "\"legacy\""'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"delete","target_type":"heading","target":["Overview","Details"]}')
assert_contains "delete is refused on plugin < 5" 'needs plugin >= 5.0' "$OUT"
fx_post /_fixture/version '{"version":"5.1.0"}'
OUT=$(mcp_call patch_content '{"filepath":"Projects/Plan.md","operation":"append","target_type":"heading","target":["Overview","Details"],"content":"back on v2"}')
assert_json "after the plugin upgrade the JSON format is used (stale caches self-heal)" 'r["result"]["structuredContent"]["patch_format"] == "json-v2" and "back on v2" in r["result"]["structuredContent"]["updated_content"]' "$OUT"
assert_reqs "the JSON instruction was the last request sent" 'last["headers"].get("Content-Type") == "application/vnd.olrapi.patch-instruction+json" and json.loads(last["body"])["content"] == "back on v2"'
OUT=$(mcp_call get_server_info '{}')
assert_json "get_server_info reports json-v2 again" 'r["result"]["structuredContent"]["patch_format"] == "json-v2"' "$OUT"

echo "== delete_file =="
OUT=$(mcp_call delete_file '{"filepath":"Tmp/one.md"}')
assert_contains "delete without confirm is refused" 'confirm=true' "$OUT"
assert_reqs "refused delete sent nothing" 'last["method"] != "DELETE"'
OUT=$(mcp_call delete_file '{"filepath":"Tmp/one.md","confirm":true}')
assert_json "confirmed delete goes to trash" 'r["result"]["structuredContent"]["deleted"] is True and r["result"]["structuredContent"]["permanent"] is False' "$OUT"
assert_reqs "delete sends permanent=false" 'last["method"] == "DELETE" and last["raw_path"] == "/vault/Tmp/one.md?permanent=false"'
OUT=$(mcp_call delete_file '{"filepath":"Tmp/two.md","confirm":true,"permanent":true}')
assert_reqs "permanent delete sends permanent=true" 'last["raw_path"] == "/vault/Tmp/two.md?permanent=true"'
OUT=$(mcp_call delete_file '{"filepath":"Tmp/one.md","confirm":true}')
assert_contains "deleting a missing note is a 404 tool error" 'HTTP 404' "$OUT"
OUT=$(mcp_call delete_file '{"filepath":"Projects","confirm":true}')
assert_contains "deleting a folder maps 405 to the folder hint" 'folders cannot be deleted' "$OUT"

echo "== write gates (guard instance: OBSIDIAN_READ_ONLY=true) =="
for CALL in 'append_content {"filepath":"Welcome.md","content":"x"}' \
            'put_content {"filepath":"Welcome.md","content":"x"}' \
            'patch_content {"filepath":"Welcome.md","operation":"append","target_type":"heading","target":["Welcome"],"content":"x"}' \
            'delete_file {"filepath":"Welcome.md","confirm":true}' \
            'open_file {"filepath":"Welcome.md"}' \
            'execute_command {"command_id":"editor:toggle-bold"}'; do
  TOOL=${CALL%% *}; ARGS=${CALL#* }
  OUT=$(mcp_call_on "$GUARD_BASE" "$TOOL" "$ARGS")
  case "$TOOL" in
    execute_command) assert_contains "$TOOL is gated (commands disabled wins)" 'command execution is disabled' "$OUT" ;;
    *) assert_contains "$TOOL is refused read-only" 'read-only' "$OUT" ;;
  esac
done
OUT=$(mcp_call_on "$GUARD_BASE" list_commands '{}')
assert_contains "list_commands is refused when commands are disabled" 'OBSIDIAN_ENABLE_COMMANDS=true' "$OUT"

echo "== periodic notes =="
OUT=$(mcp_call get_periodic_note '{"period":"daily"}')
assert_json "daily note resolves through the 307 redirect" 'r["result"]["structuredContent"]["path"] == "Daily/'"$TODAY"'.md" and "daily entry today" in r["result"]["structuredContent"]["content"]' "$OUT"
assert_reqs "redirect followed exactly once with Accept preserved" 'prev["raw_path"] == "/periodic/daily/" and last["raw_path"] == "/vault/Daily/'"$TODAY"'.md" and last["headers"].get("Accept") == "text/markdown" and prev["headers"].get("Accept") == "text/markdown"'
OUT=$(mcp_call get_periodic_note "{\"period\":\"daily\",\"date\":\"$YESTERDAY\"}")
assert_json "dated daily note resolves" '"daily entry yesterday" in r["result"]["structuredContent"]["content"] and r["result"]["structuredContent"]["date"] == "'"$YESTERDAY"'"' "$OUT"
assert_reqs "dated route is /periodic/{period}/{y}/{m}/{d}/" 'prev["raw_path"] == "/periodic/daily/'"$Y_YEAR"'/'"$Y_MONTH"'/'"$Y_DAY"'/"'
OUT=$(mcp_call get_periodic_note '{"period":"daily","format":"metadata"}')
assert_json "periodic metadata format" 'r["result"]["structuredContent"]["data"]["path"] == "Daily/'"$TODAY"'.md"' "$OUT"
OUT=$(mcp_call get_periodic_note '{"period":"weekly"}')
assert_contains "period not enabled maps 400 to the enable hint" 'not enabled' "$OUT"
OUT=$(mcp_call get_periodic_note '{"period":"daily","date":"yesterday"}')
assert_contains "bad date is refused in-guest" 'YYYY-MM-DD' "$OUT"
OUT=$(mcp_call get_periodic_note '{"period":"daily","date":"2026-02-30"}')
assert_contains "impossible date is refused in-guest" 'YYYY-MM-DD' "$OUT"
OUT=$(mcp_call get_periodic_note '{"period":"daily","date":"2026-01-15"}')
assert_contains "missing dated note is classified as no-note (first-hop 40461)" 'has no note for 2026-01-15 yet' "$OUT"
OUT=$(mcp_call get_periodic_note '{"period":"hourly"}')
assert_failed "unknown period is rejected" "$OUT"
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":3}')
assert_json "recent daily notes finds today and yesterday, skips the rest" 'r["result"]["structuredContent"]["found"] == 2 and r["result"]["structuredContent"]["skipped"] == 1 and r["result"]["structuredContent"]["notes"][0]["path"] == "Daily/'"$TODAY"'.md"' "$OUT"
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":50,"include_content":true}')
assert_json "recent periodic clamps lookups to 10 and includes content" 'r["result"]["structuredContent"]["lookups"] == 10 and "daily entry today" in r["result"]["structuredContent"]["notes"][0]["content"]' "$OUT"
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":1}')
assert_json "the current period is resolved by the plugin (undated route)" 'r["result"]["structuredContent"]["notes"][0]["resolved_by"] == "plugin" and r["result"]["structuredContent"]["tz_offset_minutes"] == 0' "$OUT"
assert_reqs "recent periodic first hop is the undated /periodic/daily/" 'prev["raw_path"] == "/periodic/daily/" and last["raw_path"] == "/vault/Daily/'"$TODAY"'.md"'
EXPECT=$(python3 -c 'from datetime import datetime, timedelta, timezone; d = (datetime.now(timezone.utc) - timedelta(minutes=840)).date() - timedelta(days=1); print("/periodic/daily/%d/%d/%d/" % (d.year, d.month, d.day))')
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":2,"tz_offset_minutes":-840}')
assert_json "tz_offset_minutes is echoed" 'r["result"]["structuredContent"]["tz_offset_minutes"] == -840' "$OUT"
assert_reqs "tz_offset_minutes shifts the dated look-back (UTC-14 yesterday)" 'prev["raw_path"] == "'"$EXPECT"'"'
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","tz_offset_minutes":841}')
assert_contains "tz_offset_minutes beyond +-840 is refused" 'between -840 and 840' "$OUT"
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","tz_offset_minutes":-99999999999999}')
assert_contains "huge negative tz_offset_minutes is refused" 'between -840 and 840' "$OUT"
# The plugin's local "today" is a day behind the server's UTC day: the undated
# hop and the first dated look-back resolve to the same note — listed once.
fx_post /_fixture/periodic '{"enabled":true,"today_shift_days":-1}'
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":2}')
assert_json "plugin-local today differing from UTC is de-duplicated" 'r["result"]["structuredContent"]["found"] == 1 and r["result"]["structuredContent"]["duplicates"] == 1 and r["result"]["structuredContent"]["notes"][0]["path"] == "Daily/'"$YESTERDAY"'.md"' "$OUT"
assert_contains "duplicate hint suggests tz_offset_minutes" 'tz_offset_minutes' "$OUT"
# Slow /periodic/ answers (2.2 s each): lookups start at 0, 2.2, 4.4 s; the
# fourth would start past the 6 s budget and is not attempted.
fx_post /_fixture/periodic '{"enabled":true,"delay_ms":2200}'
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":5}')
assert_json "recent periodic stops at the call budget" 'r["result"]["structuredContent"]["budget_exhausted"] is True and r["result"]["structuredContent"]["attempted"] == 3 and r["result"]["structuredContent"]["lookups"] == 5' "$OUT"
assert_contains "budget stop is explained in the text" 'time budget' "$OUT"
fx_post /_fixture/periodic '{"enabled":true}'

echo "== periodic 404 flavours (classified by errorCode, never by status alone) =="
# 1. Companion plugin missing: the core plugin's notFoundHandler answers its
#    generic {"errorCode": 40400, "message": "Not Found"} envelope.
fx_post /_fixture/periodic '{"enabled":false}'
OUT=$(mcp_call get_periodic_note '{"period":"daily"}')
assert_contains "companion missing (40400 envelope): route-not-served classification" 'route is not served' "$OUT"
assert_contains "companion missing: names the companion plugin" 'Local REST API - Periodic Notes' "$OUT"
assert_contains "companion missing: says it is not installed" 'not installed or enabled' "$OUT"
assert_not_contains "companion missing is not reported as a missing note" 'has no note' "$OUT"
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily"}')
assert_contains "recent periodic aborts on a missing companion plugin" 'not installed or enabled' "$OUT"
# 2. The same without the envelope (a bare text/plain 404, e.g. from a proxy).
fx_post /_fixture/periodic '{"enabled":false,"envelope":false}'
OUT=$(mcp_call get_periodic_note '{"period":"daily"}')
assert_contains "companion missing (bare 404): route-not-served classification" 'route is not served' "$OUT"
assert_contains "bare 404 still names the companion plugin" 'Local REST API - Periodic Notes' "$OUT"
# 3. Period unknown to the periodic-note plugins: 404 errorCode 40460.
fx_post /_fixture/periodic '{"enabled":true,"unknown_periods":["quarterly"]}'
OUT=$(mcp_call get_periodic_note '{"period":"quarterly"}')
assert_contains "40460 maps to period-not-configured" 'no quarterly period configured' "$OUT"
assert_contains "40460 carries its errorCode" 'errorCode 40460' "$OUT"
assert_contains "40460 hint names the plugin that provides the period" 'Periodic Notes plugin' "$OUT"
assert_not_contains "40460 is not reported as a missing companion" 'route is not served' "$OUT"
assert_not_contains "40460 is not reported as a missing note" 'has no note' "$OUT"
OUT=$(mcp_call get_recent_periodic_notes '{"period":"quarterly","limit":2}')
assert_contains "recent periodic aborts on an unconfigured period with the same message" 'no quarterly period configured' "$OUT"
# 4. Period enabled but no note for the current period: 404 errorCode 40461
#    on the first hop (the companion redirects only when the file exists).
fx_post /_fixture/periodic '{"enabled":true}'
OUT=$(mcp_call delete_file "{\"filepath\":\"Daily/$TODAY.md\",\"confirm\":true}")
assert_json "today's daily note removed for the no-note case" 'r["result"]["structuredContent"]["deleted"] is True' "$OUT"
OUT=$(mcp_call get_periodic_note '{"period":"daily"}')
assert_contains "40461 maps to no-note-yet for the current period" 'has no note for the current period yet' "$OUT"
assert_contains "40461 hint says how to create it" 'put_content or append_content' "$OUT"
assert_contains "40461 carries its errorCode" 'errorCode 40461' "$OUT"
assert_not_contains "40461 is not reported as a missing companion" 'route is not served' "$OUT"
assert_not_contains "40461 hint does not send the user to install the companion" 'not installed or enabled' "$OUT"
assert_reqs "40461 is answered on the first hop (no redirect to follow)" 'last["raw_path"] == "/periodic/daily/"'
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":3}')
assert_json "recent periodic skips a 40461 period instead of aborting" 'r["result"]["structuredContent"]["found"] == 1 and r["result"]["structuredContent"]["skipped"] == 2 and r["result"]["structuredContent"]["notes"][0]["path"] == "Daily/'"$YESTERDAY"'.md"' "$OUT"
OUT=$(mcp_call put_content "{\"filepath\":\"Daily/$TODAY.md\",\"content\":\"# $TODAY\\n\\n- daily entry today\\n\"}")
assert_json "today's daily note restored" 'r["result"]["structuredContent"]["status"] == "written"' "$OUT"
OUT=$(mcp_call get_periodic_note '{"period":"daily"}')
assert_json "restored daily note resolves again" '"daily entry today" in r["result"]["structuredContent"]["content"]' "$OUT"
# 5. The note vanishes between the redirect and the read: 404 on /vault/.
fx_post /_fixture/periodic '{"enabled":true,"redirect_missing":true}'
OUT=$(mcp_call get_periodic_note '{"period":"daily","date":"2026-01-15"}')
assert_contains "post-redirect 404 names the would-be path" "would be 'Daily/2026-01-15.md' but it does not exist" "$OUT"
assert_contains "post-redirect 404 says how to create it" 'create it with put_content or append_content at that path' "$OUT"
OUT=$(mcp_call get_recent_periodic_notes '{"period":"daily","limit":3}')
assert_json "recent periodic skips a post-redirect 404 too" 'r["result"]["structuredContent"]["found"] == 2 and r["result"]["structuredContent"]["skipped"] == 1' "$OUT"
# 6. A 404 with an errorCode this server does not know: reported as upstream
#    said it, with no guess about what it means.
fx_post /_fixture/periodic '{"enabled":true,"error_code":40499}'
OUT=$(mcp_call get_periodic_note '{"period":"daily"}')
assert_contains "unrecognised 404 code is reported as upstream said" 'HTTP 404 errorCode 40499 for /periodic/daily/: Something else entirely' "$OUT"
assert_not_contains "unrecognised 404 code is not called a missing companion" 'route is not served' "$OUT"
assert_not_contains "unrecognised 404 code is not called a missing note" 'has no note' "$OUT"
# 7. The companion plugin itself failed: 500 errorCode 50060.
fx_post /_fixture/periodic '{"enabled":true,"fail":true}'
OUT=$(mcp_call get_periodic_note '{"period":"daily"}')
assert_contains "50060 maps to the companion-failed hint" 'companion plugin failed' "$OUT"
assert_contains "50060 hint says retry once" 'Retry once' "$OUT"
# 8. 400 errorCode 40060 (period switched off) keeps its own hint.
fx_post /_fixture/periodic '{"enabled":true}'
OUT=$(mcp_call get_periodic_note '{"period":"monthly"}')
assert_contains "40060 carries its errorCode" 'errorCode 40060' "$OUT"
assert_contains "40060 hint says to enable the period" 'Enable it in Obsidian' "$OUT"

echo "== open_file / commands =="
OUT=$(mcp_call open_file '{"filepath":"Fresh2.md","new_leaf":true}')
assert_json "open_file opens (and creates) the note" 'r["result"]["structuredContent"]["opened"] is True and r["result"]["structuredContent"]["new_leaf"] is True' "$OUT"
assert_reqs "open_file POSTs /open/{path}?newLeaf=true" 'last["method"] == "POST" and last["raw_path"] == "/open/Fresh2.md?newLeaf=true"'
OUT=$(mcp_call list_files_in_vault '{}')
assert_json "open_file created the note" '"Fresh2.md" in r["result"]["structuredContent"]["files"]' "$OUT"
OUT=$(mcp_call list_commands '{}')
assert_json "list_commands lists ids and names" 'any(c["id"] == "editor:toggle-bold" for c in r["result"]["structuredContent"]["commands"])' "$OUT"
OUT=$(mcp_call list_commands '{"filter":"BOLD"}')
assert_json "list_commands filter is case-insensitive" 'r["result"]["structuredContent"]["total"] == 1 and r["result"]["structuredContent"]["commands"][0]["id"] == "editor:toggle-bold"' "$OUT"
OUT=$(mcp_call list_commands '{"limit":1}')
assert_json "list_commands limit truncates" 'r["result"]["structuredContent"]["returned"] == 1 and r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call execute_command '{"command_id":"editor:toggle-bold"}')
assert_json "execute_command runs a known command" 'r["result"]["structuredContent"]["executed"] is True' "$OUT"
assert_reqs "execute_command POSTs /commands/{id}/ encoded" 'last["method"] == "POST" and last["raw_path"] == "/commands/editor%3Atoggle-bold/"'
OUT=$(mcp_call execute_command '{"command_id":"nope:nothing"}')
assert_contains "unknown command maps 404 to the list_commands hint" 'list_commands' "$OUT"
LONGID=$(python3 -c 'print("c" * 300, end="")')
OUT=$(mcp_call execute_command "{\"command_id\":\"$LONGID\"}")
assert_contains "300-char command id is refused in-guest" 'longer than 200' "$OUT"
OUT=$(mcp_call execute_command '{"command_id":"   "}')
assert_contains "blank command id is refused" 'must not be empty' "$OUT"

echo "== upstream error mapping =="
OUT=$(mcp_call get_file_contents '{"filepath":"Forbidden.md"}')
assert_contains "403 is surfaced with the plugin message" 'HTTP 403' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"RateLimited.md"}')
assert_contains "429 maps to the rate-limit hint" 'Rate limited' "$OUT"
assert_contains "429 hint carries Retry-After" 'Retry-After: 7' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"Crash.md"}')
assert_contains "500 errorCode 50020 maps to the transient hint" '50020' "$OUT"
assert_contains "50020 hint says retry once" 'Retry once' "$OUT"
OUT=$(mcp_call get_file_contents '{"filepath":"slow.md"}')
assert_contains "slow upstream times out instead of wedging" 'timed out' "$OUT"
assert_contains "timeout hint says retry once then narrow" 'Retry once' "$OUT"
OUT=$(mcp_call list_files_in_vault '{}')
assert_contains "server alive after the outbound timeout" '"Welcome.md"' "$OUT"

if [ "${E2E_LIVE:-0}" = "1" ]; then
  echo "== live smoke (E2E_LIVE=1) =="
  LIVE_URL=${OBSIDIAN_LIVE_URL:-http://127.0.0.1:27123}
  "$WASMTIME" serve -Sp3,cli,http --env "OBSIDIAN_BASE_URL=${LIVE_URL}" --env "OBSIDIAN_API_KEY=${OBSIDIAN_API_KEY:-}" \
    --addr "127.0.0.1:${LIVE_PORT}" "$WASM" >"$E2E_TMP/live.log" 2>&1 &
  LIVE_PID=$!
  mcp_wait_ready "$LIVE_PORT"
  LIVE_BASE="http://127.0.0.1:${LIVE_PORT}/"
  OUT=$(mcp_call_on "$LIVE_BASE" check_auth '{}')
  assert_json "live: check_auth reports ok" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" list_files_in_vault '{}')
  assert_json "live: vault root is non-empty" 'r["result"]["structuredContent"]["count"] > 0' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" simple_search '{"query":"the","limit":3}')
  assert_json "live: simple_search answers" '"results" in r["result"]["structuredContent"]' "$OUT"
fi

guard_tests
mcp_harness_report
