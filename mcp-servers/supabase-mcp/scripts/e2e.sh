#!/usr/bin/env bash
# End-to-end tests for supabase-mcp. Framework checks (protocol, spec
# enforcement, discovery route, skills over MCP, robustness, Host guard) come
# from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases live
# below and run against a hermetic Python stand-in for the Supabase
# Management API (scripts/fixture.py) selected with SUPABASE_BASE_URL.
#
# Instances under test (all wasmtime, all 127.0.0.1):
#   PORT        read-only server with the token (the primary)
#   GUARD_PORT  Host-guard instance started WITHOUT the token (missing-secret path)
#   RW_PORT     SUPABASE_READ_ONLY=false (writes + apply_migration)
#   PIN_PORT    SUPABASE_PROJECT_REF=abcdefghijklmnopqrst (project-scoped mode)
#   BAD_PORT    a wrong token (401 mapping)
#
# Optional: E2E_LIVE=1 with SUPABASE_ACCESS_TOKEN in the environment adds one
# smoke case against the real api.supabase.com; E2E_PG=1 makes the fixture run
# the borrowed pg-meta SQL against the local `mcp-pg` Postgres container.
#
# Usage: scripts/e2e.sh [--no-build]
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9845}
GUARD_PORT=${GUARD_PORT:-9846}
FIXTURE_PORT=${FIXTURE_PORT:-9847}
RW_PORT=${RW_PORT:-9848}
PIN_PORT=${PIN_PORT:-9849}
BAD_PORT=${BAD_PORT:-9850}
LIVE_PORT=${LIVE_PORT:-9851}
WASM=${WASM:-target/wasm32-wasip2/release/supabase_mcp.wasm}
SKILL_NAME=supabase-mcp
REF=abcdefghijklmnopqrst

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIXTURE_LOG="$E2E_TMP/fixture-requests.jsonl"
: >"$FIXTURE_LOG"
EXTRA_PIDS=()
cleanup_all() {
  local pid
  for pid in "${EXTRA_PIDS[@]}"; do kill "$pid" 2>/dev/null; done
  mcp_harness_cleanup
}
trap cleanup_all EXIT

# The concurrency test in framework_tests fires this tool 8x in parallel —
# an outbound tool, so concurrent outbound is what gets exercised.
FIRST_TOOL_NAME=list_extensions
FIRST_TOOL_ARGS="{\"project_id\":\"$REF\"}"
FIRST_TOOL_EXPECT='"installed_version"'

# fixture_last <target-substring> — the last request the fixture logged whose
# raw target (path + query string) contains the substring, as one JSON
# object (json.dumps spacing).
fixture_last() {
  python3 - "$FIXTURE_LOG" "$1" <<'PY'
import json, sys
path, needle = sys.argv[1], sys.argv[2]
last = None
for line in open(path, encoding="utf-8"):
    try:
        entry = json.loads(line)
    except ValueError:
        continue
    if needle in entry.get("target", ""):
        last = entry
print(json.dumps(last, ensure_ascii=False) if last else "")
PY
}

# fixture_count <method> <path-substring>
fixture_count() {
  python3 - "$FIXTURE_LOG" "$1" "$2" <<'PY'
import json, sys
path, method, needle = sys.argv[1:4]
n = 0
for line in open(path, encoding="utf-8"):
    try:
        entry = json.loads(line)
    except ValueError:
        continue
    if entry.get("method") == method and needle in entry.get("path", ""):
        n += 1
print(n)
PY
}

# assert_equal <name> <expected> <actual> — exact string equality (a
# substring match would let a count of 1 pass against 11).
assert_equal() {
  if [ "$2" = "$3" ]; then
    pass "$1"
  else
    fail "$1" "expected [$2], got [$3]"
  fi
}

start_extra() {
  local port="$1"
  shift
  "$WASMTIME" serve -Sp3,cli,http "$@" --addr "127.0.0.1:${port}" "$WASM" \
    >"$E2E_TMP/extra-${port}.log" 2>&1 &
  EXTRA_PIDS+=($!)
  mcp_wait_ready "$port"
}

mcp_build_if_needed "${1:-}"

echo "starting fixture on :${FIXTURE_PORT}..."
python3 scripts/fixture.py "$FIXTURE_PORT" "$FIXTURE_LOG" >"$E2E_TMP/fixture.log" 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "http://127.0.0.1:${FIXTURE_PORT}/health" && break
  sleep 0.1
done
BASE_URL="http://127.0.0.1:${FIXTURE_PORT}"

mcp_harness_start --env "SUPABASE_BASE_URL=$BASE_URL" --env SUPABASE_ACCESS_TOKEN=sbp_e2e_token \
  --env SUPABASE_READ_ONLY=true --env SUPABASE_MAX_ROWS=200
# Guard instance: no token, so the missing-secret path is testable.
mcp_harness_start_guard --env "SUPABASE_BASE_URL=$BASE_URL"
start_extra "$RW_PORT" --env "SUPABASE_BASE_URL=$BASE_URL" --env SUPABASE_ACCESS_TOKEN=sbp_e2e_token \
  --env SUPABASE_READ_ONLY=false
start_extra "$PIN_PORT" --env "SUPABASE_BASE_URL=$BASE_URL" --env SUPABASE_ACCESS_TOKEN=sbp_e2e_token \
  --env "SUPABASE_PROJECT_REF=$REF"
# The bad-token instance also carries a 2 s outbound deadline so the
# fixture's slow route (any token) exercises the timeout mapping.
start_extra "$BAD_PORT" --env "SUPABASE_BASE_URL=$BASE_URL" --env SUPABASE_ACCESS_TOKEN=sbp_wrong_token \
  --env MCP_OUTBOUND_TIMEOUT_MS=2000
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"
RW_BASE="http://127.0.0.1:${RW_PORT}/"
PIN_BASE="http://127.0.0.1:${PIN_PORT}/"
BAD_BASE="http://127.0.0.1:${BAD_PORT}/"

ALL_TOOLS=(check_auth list_organizations list_projects get_project list_tables list_extensions
  list_migrations execute_sql apply_migration get_logs get_advisors list_edge_functions
  get_edge_function get_project_url get_publishable_keys generate_typescript_types)

framework_tests "${ALL_TOOLS[@]}"
discovery_tests list_projects
skills_tests "$SKILL_NAME" "references/TOOLS.md"

echo "== discovery: credentials block =="
OUT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / carries the credentials block" '"credentials"' "$OUT"
assert_contains "GET / names the secret ref" '"ref": "supabase-mcp-access-token"' "$OUT"
assert_contains "GET / reports the token as configured" '"status": "configured"' "$OUT"
assert_contains "GET / points at check_auth" '"validate": "check_auth"' "$OUT"
OUT=$(curl -sS --max-time 20 "$GUARD_BASE")
assert_contains "GET / on the tokenless instance reports missing" '"status": "missing"' "$OUT"
assert_not_contains "GET / never leaks the token value" 'sbp_e2e_token' "$(curl -sS --max-time 20 "$MCP_BASE")"

echo "== check_auth =="
OUT=$(mcp_call check_auth '{}')
assert_contains "check_auth ok with a valid token" '"status":"ok"' "$OUT"
assert_contains "check_auth reports organizations" '"organization_count":2' "$OUT"
assert_contains "check_auth reports read-only mode" '"read_only":true' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" check_auth '{}')
assert_contains "check_auth missing: status missing" '"status":"missing"' "$OUT"
assert_contains "check_auth missing: names the env var" 'SUPABASE_ACCESS_TOKEN is not set' "$OUT"
assert_contains "check_auth missing: names the secret ref" 'supabase-mcp-access-token' "$OUT"
assert_contains "check_auth missing: says where to get a token" 'dashboard/account/tokens' "$OUT"
assert_contains "check_auth missing: is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$BAD_BASE" check_auth '{}')
assert_contains "check_auth invalid token: status invalid" '"status":"invalid"' "$OUT"
assert_contains "check_auth invalid token: upstream 401 surfaced" 'HTTP 401' "$OUT"
assert_contains "check_auth invalid token: remediation names the ref" 'supabase-mcp-access-token' "$OUT"
OUT=$(mcp_call_on "$PIN_BASE" check_auth '{}')
assert_contains "check_auth pinned: status ok" '"status":"ok"' "$OUT"
assert_contains "check_auth pinned: reports the pinned project" "\"pinned_project\":\"$REF\"" "$OUT"
assert_contains "check_auth pinned: identity is the project" '"project":{' "$OUT"

echo "== list_organizations / list_projects =="
OUT=$(mcp_call list_organizations '{}')
assert_contains "list_organizations returns slugs" '"slug":"acme"' "$OUT"
assert_contains "list_organizations round-trips unicode" 'Ünïcode' "$OUT"
assert_contains "list_organizations counts" '"count":2' "$OUT"
REQ=$(fixture_last /v1/organizations)
assert_contains "bearer token sent as Authorization header" '"authorization": "Bearer sbp_e2e_token"' "$REQ"
assert_contains "descriptive User-Agent sent" '"user-agent": "supabase-mcp-cosmonic/' "$REQ"
assert_contains "Accept: application/json sent on JSON GETs" '"accept": "application/json"' "$REQ"
OUT=$(mcp_call list_organizations '{"unexpected":"argument","nested":{"x":[1,2,3]}}')
assert_contains "list_organizations ignores unexpected arguments" '"slug":"acme"' "$OUT"
OUT=$(mcp_call list_projects '{}')
assert_contains "list_projects returns the project ref as id" "\"id\":\"$REF\"" "$OUT"
assert_contains "list_projects flattens the Postgres version" '"database_version":"15.1.0.117"' "$OUT"
assert_contains "list_projects shows a paused project's status" '"status":"INACTIVE"' "$OUT"
assert_contains "list_projects round-trips unicode names" 'Prod ✓' "$OUT"

echo "== missing secret (guard instance) =="
OUT=$(mcp_call_on "$GUARD_BASE" list_projects '{}')
assert_contains "missing secret: tool error, not a raw 401" '"isError":true' "$OUT"
assert_contains "missing secret: names the variable" 'SUPABASE_ACCESS_TOKEN is not set' "$OUT"
assert_contains "missing secret: names the secret ref" 'supabase-mcp-access-token' "$OUT"
assert_contains "missing secret: says where to get the credential" 'https://supabase.com/dashboard/account/tokens' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" execute_sql "{\"project_id\":\"$REF\",\"query\":\"select 1\"}")
assert_contains "missing secret: execute_sql also refuses" 'SUPABASE_ACCESS_TOKEN is not set' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" get_project_url "{\"project_id\":\"$REF\"}")
assert_contains "get_project_url needs no token (computed)" "\"url\":\"https://$REF.supabase.co\"" "$OUT"

echo "== invalid credential (401) =="
OUT=$(mcp_call_on "$BAD_BASE" list_organizations '{}')
assert_contains "401 maps to Unauthorized" 'Unauthorized (HTTP 401)' "$OUT"
assert_contains "401 carries the upstream message" 'fixture rejected the bearer token' "$OUT"
assert_contains "401 says to provide a valid token" 'provide a valid access token' "$OUT"
assert_contains "401 repeats the secret hint" 'supabase-mcp-access-token' "$OUT"
assert_contains "401 is a tool error" '"isError":true' "$OUT"

echo "== project-scoped (pinned) mode =="
OUT=$(printf '%s' "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{$META}}" | mcp_post "$PIN_BASE" -H 'Mcp-Method: tools/list')
assert_not_contains "pinned: list_projects hidden from tools/list" '"name":"list_projects"' "$OUT"
assert_not_contains "pinned: list_organizations hidden from tools/list" '"name":"list_organizations"' "$OUT"
assert_contains "pinned: project tools still listed" '"name":"get_project"' "$OUT"
OUT=$(mcp_call_on "$PIN_BASE" list_projects '{}')
assert_contains "pinned: list_projects refused with the reason" "pinned to project $REF" "$OUT"
assert_contains "pinned: refusal is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$PIN_BASE" get_project '{}')
assert_contains "pinned: get_project needs no project_id" '"healthy":true' "$OUT"
OUT=$(mcp_call_on "$PIN_BASE" get_project '{"project_id":"zzzzzzzzzzzzzzzzzzzz"}')
assert_contains "pinned: project_id argument is ignored" '"healthy":true' "$OUT"
OUT=$(curl -sS --max-time 20 "$PIN_BASE")
assert_not_contains "pinned: discovery document hides list_projects" '"list_projects"' "$OUT"
assert_contains "pinned: discovery document lists get_project" '"get_project"' "$OUT"

echo "== get_project + upstream error mapping =="
OUT=$(mcp_call get_project "{\"project_id\":\"$REF\"}")
assert_contains "get_project healthy" '"healthy":true' "$OUT"
assert_contains "get_project carries the status" '"status":"ACTIVE_HEALTHY"' "$OUT"
OUT=$(mcp_call get_project '{"project_id":"pppppppppppppppppppp"}')
assert_contains "get_project paused: healthy false" '"healthy":false' "$OUT"
assert_contains "get_project paused: explains restore" 'paused' "$OUT"
OUT=$(mcp_call get_project '{"project_id":"zzzzzzzzzzzzzzzzzzzz"}')
assert_contains "404 maps to Not found" 'Not found (HTTP 404)' "$OUT"
assert_contains "404 suggests list_projects" 'list_projects' "$OUT"
OUT=$(mcp_call get_project '{"project_id":"ffffffffffffffffffff"}')
assert_contains "403 maps to Forbidden" 'Forbidden (HTTP 403)' "$OUT"
assert_contains "403 explains organization membership" 'organization' "$OUT"
OUT=$(mcp_call get_project '{"project_id":"rrrrrrrrrrrrrrrrrrrr"}')
assert_contains "429 maps to Rate limited" 'Rate limited (HTTP 429)' "$OUT"
assert_contains "429 reports X-RateLimit-Reset" 'Wait 42 second' "$OUT"
assert_contains "429 states the limits" '120 requests/min' "$OUT"
OUT=$(mcp_call get_project '{"project_id":"eeeeeeeeeeeeeeeeeeee"}')
assert_contains "500 maps to an upstream error" 'HTTP 500' "$OUT"
assert_contains "500 points at the status page" 'status.supabase.com' "$OUT"
OUT=$(mcp_call get_project '{"project_id":"bbbbbbbbbbbbbbbbbbbb"}')
assert_contains "non-JSON 200 body is a decode error, not a trap" 'could not parse' "$OUT"
OUT=$(mcp_call get_project '{"project_id":"ABC"}')
assert_contains "malformed ref rejected locally" 'project_id is invalid' "$OUT"
OUT=$(mcp_call get_project '{}')
assert_contains "missing ref is an actionable error" 'project_id is required' "$OUT"
BEFORE=$(fixture_count GET /v1/organizations)
OUT=$(mcp_call get_project '{"project_id":"../organizations?x=1#"}')
assert_contains "traversal-shaped ref rejected" 'project_id is invalid' "$OUT"
AFTER=$(fixture_count GET /v1/organizations)
assert_equal "traversal-shaped ref never reaches upstream" "$BEFORE" "$AFTER"
OUT=$(mcp_call get_project '{"project_id":"ábcdefghijklmnopqrst"}')
assert_contains "unicode ref rejected without trapping" 'project_id is invalid' "$OUT"
OUT=$(mcp_call get_project '{"project_id":123}')
assert_not_contains "wrong-typed ref is an error" '"healthy"' "$OUT"
assert_contains "wrong-typed ref is a clean tool error" '"isError":true' "$OUT"

echo "== list_tables =="
OUT=$(mcp_call list_tables "{\"project_id\":\"$REF\"}")
assert_contains "list_tables compact: schema-qualified names" '"name":"public.orders"' "$OUT"
assert_contains "list_tables compact: RLS flag" '"rls_enabled":false' "$OUT"
assert_contains "list_tables compact: row estimate" '"rows":42' "$OUT"
assert_contains "list_tables compact: primary keys" '"primary_keys":["id"]' "$OUT"
assert_contains "list_tables compact: RLS advisory" '"rls_disabled"' "$OUT"
assert_contains "list_tables wraps rows in the untrusted boundary" 'untrusted-data-' "$OUT"
assert_not_contains "list_tables compact omits columns" '"columns"' "$OUT"
REQ=$(fixture_last database/query)
assert_contains "list_tables passes schemas as bound parameters" '"parameters": ["public"]' "$REQ"
assert_contains "list_tables always runs read-only" '"read_only": true' "$REQ"
assert_contains "list_tables sends JSON content type" '"content-type": "application/json"' "$REQ"
OUT=$(mcp_call list_tables "{\"project_id\":\"$REF\",\"verbose\":true,\"schemas\":[\"public\",\"auth\"]}")
assert_contains "list_tables verbose: columns" '"columns":[' "$OUT"
assert_contains "list_tables verbose: column options" '"options":["identity","updatable"]' "$OUT"
assert_contains "list_tables verbose: enums" '"enums":["new","paid"]' "$OUT"
assert_contains "list_tables verbose: foreign keys" '"foreign_key_constraints"' "$OUT"
assert_contains "list_tables verbose: fk target" '"target_table":"public.customers"' "$OUT"
REQ=$(fixture_last database/query)
assert_contains "list_tables binds every schema in order" '"parameters": ["public", "auth"]' "$REQ"
OUT=$(mcp_call list_tables "{\"project_id\":\"$REF\",\"schemas\":[]}")
assert_contains "list_tables empty schemas = all non-system" '"schemas":"all non-system schemas"' "$OUT"
REQ=$(fixture_last database/query)
assert_contains "list_tables excludes system schemas by parameter" '"information_schema"' "$REQ"
assert_contains "list_tables uses NOT IN for the system set" 'not in ($1, $2, $3, $4)' "$REQ"
OUT=$(mcp_call list_tables "{\"project_id\":\"$REF\",\"schemas\":[\"public; drop table users --\"]}")
assert_contains "list_tables rejects injection-shaped schema" 'schemas is invalid' "$OUT"
OUT=$(mcp_call list_tables "{\"project_id\":\"$REF\",\"schemas\":[\"pübl1c\"]}")
assert_contains "list_tables rejects non-identifier schema" 'schemas is invalid' "$OUT"
MANY=$(python3 -c 'import json; print(json.dumps(["s%d" % i for i in range(21)]))')
OUT=$(mcp_call list_tables "{\"project_id\":\"$REF\",\"schemas\":$MANY}")
assert_contains "list_tables caps schemas at 20" 'at most 20 schemas' "$OUT"
OUT=$(mcp_call list_tables "{\"project_id\":\"$REF\",\"max_rows\":0}")
assert_contains "list_tables clamps max_rows 0 up to 1" '"count":1' "$OUT"
assert_contains "list_tables reports truncation" '"truncated":true' "$OUT"

echo "== list_extensions / list_migrations =="
OUT=$(mcp_call list_extensions "{\"project_id\":\"$REF\"}")
assert_contains "list_extensions returns names" '"name":"pg_stat_statements"' "$OUT"
assert_contains "list_extensions counts installed" '"installed":1' "$OUT"
assert_contains "list_extensions keeps null installed_version" '"installed_version":null' "$OUT"
REQ=$(fixture_last database/query)
assert_contains "list_extensions runs pg_available_extensions read-only" '"read_only": true' "$REQ"
OUT=$(mcp_call list_extensions '{"project_id":"rrrrrrrrrrrrrrrrrrrr"}')
assert_contains "list_extensions surfaces 429" 'Rate limited (HTTP 429)' "$OUT"
OUT=$(mcp_call list_migrations "{\"project_id\":\"$REF\"}")
assert_contains "list_migrations returns versions" '"version":"20240101000000"' "$OUT"
assert_contains "list_migrations returns names" '"name":"add_orders"' "$OUT"
assert_contains "list_migrations counts" '"count":2' "$OUT"
OUT=$(mcp_call list_migrations '{"project_id":"zzzzzzzzzzzzzzzzzzzz"}')
assert_contains "list_migrations surfaces 404" 'Not found (HTTP 404)' "$OUT"

echo "== execute_sql (read-only default) =="
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"select 1 as one\"}")
assert_contains "execute_sql returns rows" '"query":"select 1 as one"' "$OUT"
assert_contains "execute_sql wraps rows in the untrusted boundary" 'untrusted-data-' "$OUT"
assert_contains "execute_sql reports read_only" '"read_only":true' "$OUT"
assert_contains "execute_sql reports row_count" '"row_count":1' "$OUT"
REQ=$(fixture_last database/query)
assert_contains "execute_sql sends read_only:true upstream" '"read_only": true' "$REQ"
assert_not_contains "execute_sql sends no parameters field" '"parameters"' "$(printf '%s' "$REQ" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["json"]))')"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"select 'héllo ✓; drop table users; --' as x\"}")
assert_contains "execute_sql round-trips unicode + injection-shaped text" "héllo ✓; drop table users; --" "$OUT"
REQ=$(fixture_last database/query)
assert_contains "execute_sql forwards the SQL verbatim as JSON" "drop table users; --" "$REQ"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"   \"}")
assert_contains "execute_sql rejects an empty query" 'query must not be empty' "$OUT"
HUGE=$(python3 -c 'print("select " + "x" * 100001)')
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"$HUGE\"}")
assert_contains "execute_sql rejects an oversized query" 'query is too long' "$OUT"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"select * from nope\"}")
assert_contains "SQL error surfaces the SQLSTATE" 'SQL error (SQLSTATE 42P01' "$OUT"
assert_contains "SQL error surfaces the message" 'relation \"nope\" does not exist' "$OUT"
assert_contains "SQL error surfaces the position" 'position 15' "$OUT"
assert_contains "SQL error is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"insert into t values (1)\"}")
assert_contains "write under read-only: upstream 25006 surfaced" 'SQLSTATE 25006' "$OUT"
assert_contains "write under read-only: explains the mode" 'read-only mode (SUPABASE_READ_ONLY=true)' "$OUT"
assert_contains "write under read-only: says how to enable writes" 'SUPABASE_READ_ONLY=false' "$OUT"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"select many_rows\"}")
assert_contains "row cap: reports total row_count" '"row_count":1500' "$OUT"
assert_contains "row cap: default SUPABASE_MAX_ROWS=200" '"returned":200' "$OUT"
assert_contains "row cap: truncated flag" '"truncated":true' "$OUT"
assert_contains "row cap: text explains how to get more" 'raise max_rows up to 1000' "$OUT"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"select many_rows\",\"max_rows\":5000}")
assert_contains "row cap: max_rows clamped to 1000" '"returned":1000' "$OUT"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"select many_rows\",\"max_rows\":3}")
assert_contains "row cap: max_rows 3 honoured" '"returned":3' "$OUT"
BEFORE=$(fixture_count POST database/query)
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":5}")
assert_not_contains "execute_sql wrong-typed query returns no rows" 'untrusted-data-' "$OUT"
assert_contains "execute_sql wrong-typed query is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\"}")
assert_not_contains "execute_sql missing query returns no rows" 'untrusted-data-' "$OUT"
assert_contains "execute_sql missing query is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call execute_sql "{\"project_id\":\"$REF\",\"query\":\"\"}")
assert_contains "execute_sql empty query is a tool error" '"isError":true' "$OUT"
AFTER=$(fixture_count POST database/query)
assert_equal "execute_sql malformed params never reach upstream" "$BEFORE" "$AFTER"
OUT=$(mcp_call execute_sql "{\"project_id\":\"rrrrrrrrrrrrrrrrrrrr\",\"query\":\"select 1\"}")
assert_contains "execute_sql surfaces 429" 'Rate limited (HTTP 429)' "$OUT"

echo "== execute_sql / apply_migration with SUPABASE_READ_ONLY=false =="
OUT=$(mcp_call_on "$RW_BASE" execute_sql "{\"project_id\":\"$REF\",\"query\":\"insert into t values (1)\"}")
assert_contains "RW: write succeeds" '"read_only":false' "$OUT"
assert_contains "RW: rows returned" 'insert into t values (1)' "$OUT"
REQ=$(fixture_last database/query)
assert_contains "RW: read_only:false sent upstream" '"read_only": false' "$REQ"
OUT=$(mcp_call_on "$RW_BASE" apply_migration "{\"project_id\":\"$REF\",\"name\":\"create_orders\",\"query\":\"create table orders(id int)\"}")
assert_contains "RW: apply_migration succeeds" '"success":true' "$OUT"
assert_contains "RW: apply_migration echoes the name" '"name":"create_orders"' "$OUT"
REQ=$(fixture_last database/migrations)
assert_contains "RW: migration posted with name" '"name": "create_orders"' "$REQ"
assert_contains "RW: migration posted with query" '"query": "create table orders(id int)"' "$REQ"
OUT=$(mcp_call_on "$RW_BASE" apply_migration "{\"project_id\":\"$REF\",\"name\":\"Create-Orders\",\"query\":\"create table x(id int)\"}")
assert_contains "RW: non-snake_case name rejected" 'name is invalid' "$OUT"
OUT=$(mcp_call_on "$RW_BASE" apply_migration "{\"project_id\":\"$REF\",\"name\":\"bad_one\",\"query\":\"create table nope(id int)\"}")
assert_contains "RW: failed migration says not recorded" 'failed and was not recorded' "$OUT"
assert_contains "RW: failed migration surfaces HTTP 500" 'HTTP 500' "$OUT"
OUT=$(mcp_call_on "$RW_BASE" apply_migration "{\"project_id\":\"$REF\",\"name\":\"empty\",\"query\":\"\"}")
assert_contains "RW: empty migration rejected" 'query must not be empty' "$OUT"
HUGE=$(python3 -c 'print("select " + "x" * 200001)')
OUT=$(mcp_call_on "$RW_BASE" apply_migration "{\"project_id\":\"$REF\",\"name\":\"huge\",\"query\":\"$HUGE\"}")
assert_contains "RW: oversized migration rejected" 'query is too long' "$OUT"

echo "== apply_migration gated by read-only mode =="
BEFORE=$(fixture_count POST database/migrations)
OUT=$(mcp_call apply_migration "{\"project_id\":\"$REF\",\"name\":\"create_orders\",\"query\":\"create table orders(id int)\"}")
assert_contains "RO: apply_migration refused" 'apply_migration is disabled' "$OUT"
assert_contains "RO: refusal explains the switch" 'SUPABASE_READ_ONLY=false' "$OUT"
assert_contains "RO: refusal is a tool error" '"isError":true' "$OUT"
AFTER=$(fixture_count POST database/migrations)
assert_equal "RO: refusal never reaches upstream" "$BEFORE" "$AFTER"

echo "== get_logs =="
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"api\"}")
assert_contains "get_logs api: service echoed" '"service":"api"' "$OUT"
assert_contains "get_logs api: uses the edge_logs template" "source = 'edge_logs'" "$OUT"
assert_contains "get_logs api: wraps rows in the untrusted boundary" 'untrusted-data-' "$OUT"
assert_contains "get_logs api: default limit 100" '"limit":100' "$OUT"
assert_contains "get_logs api: window not clamped by default" '"window_clamped":false' "$OUT"
REQ=$(fixture_last analytics/endpoints/logs)
assert_contains "get_logs sends both timestamps" '"iso_timestamp_start": "' "$REQ"
assert_contains "get_logs percent-encodes the timestamps" 'iso_timestamp_end=20' "$REQ"
assert_contains "get_logs percent-encodes colons" '%3A' "$REQ"
assert_contains "get_logs percent-encodes the SQL" 'sql=select%20id' "$REQ"
assert_contains "get_logs template ends with limit 100" 'limit 100' "$REQ"
for SVC in postgres auth storage realtime edge-function branch-action; do
  OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"$SVC\"}")
  assert_contains "get_logs $SVC works" "\"service\":\"$SVC\"" "$OUT"
done
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"edge-function-runtime\"}")
assert_contains "get_logs edge-function-runtime works" '"service":"edge-function-runtime"' "$OUT"
assert_contains "get_logs edge-function-runtime uses function_logs" "source = 'function_logs'" "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"postgres\",\"iso_timestamp_start\":\"2026-09-01T10:00:00Z\",\"iso_timestamp_end\":\"2026-09-01T13:00:00+02:00\"}")
assert_contains "get_logs normalises start to UTC" '"iso_timestamp_start":"2026-09-01T10:00:00.000Z"' "$OUT"
assert_contains "get_logs normalises offset end to UTC" '"iso_timestamp_end":"2026-09-01T11:00:00.000Z"' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"postgres\",\"iso_timestamp_start\":\"2026-09-01T00:00:00Z\",\"iso_timestamp_end\":\"2026-09-03T00:00:00.5Z\"}")
assert_contains "get_logs clamps a >24h window" '"window_clamped":true' "$OUT"
assert_contains "get_logs clamped start = end - 24h" '"iso_timestamp_start":"2026-09-02T00:00:00.500Z"' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"postgres\",\"iso_timestamp_start\":\"2026-09-01T12:00:00Z\",\"iso_timestamp_end\":\"2026-09-01T12:00:00Z\"}")
assert_contains "get_logs rejects start >= end" 'must be before' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"postgres\",\"iso_timestamp_start\":\"yesterday\"}")
assert_contains "get_logs rejects a non-ISO timestamp" 'not an RFC 3339' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"postgres\",\"iso_timestamp_end\":\"2026-09-01T10:00:00\"}")
assert_contains "get_logs rejects a timestamp without offset" 'not an RFC 3339' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"postgres\",\"iso_timestamp_end\":\"2026-02-30T10:00:00Z\"}")
assert_contains "get_logs rejects an impossible date" 'not an RFC 3339' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"kafka\"}")
assert_not_contains "get_logs rejects an unknown service" '"service":"kafka"' "$OUT"
assert_contains "get_logs unknown service is a clean tool error" '"isError":true' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"auth\",\"limit\":5000}")
assert_contains "get_logs clamps limit to 100" '"limit":100' "$OUT"
OUT=$(mcp_call get_logs "{\"project_id\":\"$REF\",\"service\":\"auth\",\"limit\":0}")
assert_contains "get_logs clamps limit 0 to 1" '"limit":1' "$OUT"
REQ=$(fixture_last analytics/endpoints/logs)
assert_contains "get_logs limit reaches the SQL" 'limit 1"' "$REQ"
OUT=$(mcp_call get_logs '{"project_id":"llllllllllllllllllll","service":"api"}')
assert_contains "get_logs 200-with-error becomes a tool error" 'logs endpoint rejected the query' "$OUT"
assert_contains "get_logs 200-with-error carries the message" 'bad clickhouse' "$OUT"
OUT=$(mcp_call get_logs '{"project_id":"qqqqqqqqqqqqqqqqqqqq","service":"api"}')
assert_contains "402 maps to Payment required" 'Payment required (HTTP 402)' "$OUT"
OUT=$(mcp_call get_logs '{"project_id":"rrrrrrrrrrrrrrrrrrrr","service":"api"}')
assert_contains "get_logs 429 mentions the analytics limit" '30/min for analytics' "$OUT"

echo "== get_advisors =="
OUT=$(mcp_call get_advisors "{\"project_id\":\"$REF\",\"type\":\"security\"}")
assert_contains "get_advisors security: lint names" '"name":"rls_disabled_in_public"' "$OUT"
assert_contains "get_advisors security: remediation URL kept" 'database-linter?lint=0013' "$OUT"
assert_contains "get_advisors security: all levels by default" '"count":3' "$OUT"
assert_contains "get_advisors security: by_level tally" '"ERROR":1' "$OUT"
assert_not_contains "get_advisors drops cache_key" 'cache_key' "$OUT"
OUT=$(mcp_call get_advisors "{\"project_id\":\"$REF\",\"type\":\"security\",\"level_min\":\"WARN\"}")
assert_contains "get_advisors level_min WARN filters INFO" '"count":2' "$OUT"
assert_contains "get_advisors reports filtered_out" '"filtered_out":1' "$OUT"
OUT=$(mcp_call get_advisors "{\"project_id\":\"$REF\",\"type\":\"security\",\"level_min\":\"ERROR\"}")
assert_contains "get_advisors level_min ERROR keeps errors only" '"count":1' "$OUT"
OUT=$(mcp_call get_advisors "{\"project_id\":\"$REF\",\"type\":\"performance\"}")
assert_contains "get_advisors performance" '"unindexed_foreign_keys"' "$OUT"
BEFORE=$(fixture_count GET advisors)
OUT=$(mcp_call get_advisors "{\"project_id\":\"$REF\",\"type\":\"compliance\"}")
assert_not_contains "get_advisors unknown type returns no lints" '"lints"' "$OUT"
assert_contains "get_advisors unknown type is a tool error" '"isError":true' "$OUT"
AFTER=$(fixture_count GET advisors)
assert_equal "get_advisors unknown type never reaches upstream" "$BEFORE" "$AFTER"
OUT=$(mcp_call get_advisors '{"project_id":"uuuuuuuuuuuuuuuuuuuu","type":"security"}')
assert_contains "get_advisors unexpected shape passes through raw" 'unexpected advisors response shape' "$OUT"
OUT=$(mcp_call get_advisors '{"project_id":"ffffffffffffffffffff","type":"security"}')
assert_contains "get_advisors surfaces 403" 'Forbidden (HTTP 403)' "$OUT"

echo "== edge functions =="
OUT=$(mcp_call list_edge_functions "{\"project_id\":\"$REF\"}")
assert_contains "list_edge_functions returns slugs" '"slug":"hello-world"' "$OUT"
assert_contains "list_edge_functions normalises entrypoint_path" '"entrypoint_path":"index.ts"' "$OUT"
assert_contains "list_edge_functions normalises import_map_path" '"import_map_path":"deno.json"' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"hello-world\"}")
assert_contains "get_edge_function: entrypoint file" '"name":"index.ts"' "$OUT"
assert_contains "get_edge_function: source content" 'Deno.serve' "$OUT"
assert_contains "get_edge_function: unicode in source" 'héllo from the fixture' "$OUT"
assert_contains "get_edge_function: import map file (source/ prefix stripped)" '"name":"deno.json"' "$OUT"
assert_contains "get_edge_function: file_count" '"file_count":2' "$OUT"
assert_contains "get_edge_function: not truncated" '"truncated":false' "$OUT"
REQ=$(fixture_last functions/hello-world/body)
assert_contains "get_edge_function sends Accept: multipart/form-data" '"accept": "multipart/form-data"' "$REQ"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"nope-fn\"}")
assert_contains "get_edge_function unknown slug: 404 mapped" 'Not found (HTTP 404)' "$OUT"
assert_contains "get_edge_function unknown slug: upstream message" 'Edge Function not found' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"../body\"}")
assert_contains "get_edge_function rejects traversal-shaped slug" 'function_slug' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"héllo\"}")
assert_contains "get_edge_function rejects unicode slug" 'function_slug' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"big-fn\"}")
assert_contains "get_edge_function caps a 600 KiB file" '"truncated":true' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"grumpy-fn\"}")
assert_contains "get_edge_function 406 mapped" 'Not acceptable (HTTP 406)' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"raw-fn\"}")
assert_contains "get_edge_function non-multipart body passed through" '"name":"(raw body)"' "$OUT"
assert_contains "get_edge_function raw body content" 'raw body ✓' "$OUT"
# Non-object 2xx metadata (array / string / number / null) must be a tool
# error, not a trap: with panic=abort a trap would kill the instance, so the
# hello-world call afterwards doubles as a liveness probe.
for shape in array-fn string-fn number-fn null-fn; do
  OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"$shape\"}")
  assert_contains "get_edge_function $shape metadata: tool error" '"isError":true' "$OUT"
  assert_contains "get_edge_function $shape metadata: explains the shape" 'unexpected Edge Function metadata shape' "$OUT"
  assert_not_contains "get_edge_function $shape metadata: no HTTP 500" 'Internal Server Error' "$OUT"
done
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"array-fn\"}")
assert_contains "get_edge_function array metadata names the JSON type" 'returned an array instead of a JSON object' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"hello-world\"}")
assert_contains "instance still alive after non-object metadata" '"file_count":2' "$OUT"
OUT=$(mcp_call get_edge_function "{\"project_id\":\"$REF\",\"function_slug\":\"strver-fn\"}")
assert_contains "get_edge_function string version: entrypoint normalised" '"entrypoint_path":"index.ts"' "$OUT"
assert_contains "get_edge_function string version: file names normalised" '"name":"index.ts"' "$OUT"
assert_contains "get_edge_function string version: version echoed as given" '"version":"3"' "$OUT"

echo "== get_project_url / get_publishable_keys / generate_typescript_types =="
OUT=$(mcp_call get_project_url "{\"project_id\":\"$REF\"}")
assert_contains "get_project_url computes the supabase.co URL" "\"url\":\"https://$REF.supabase.co\"" "$OUT"
OUT=$(mcp_call get_project_url '{"project_id":"evil.example/../x"}')
assert_contains "get_project_url rejects a malformed ref" 'project_id is invalid' "$OUT"
OUT=$(mcp_call get_publishable_keys "{\"project_id\":\"$REF\"}")
assert_contains "get_publishable_keys: publishable key" 'sb_publishable_abc123' "$OUT"
assert_contains "get_publishable_keys: legacy anon key" '"name":"anon"' "$OUT"
assert_contains "get_publishable_keys: anon disabled when legacy keys are off" '"disabled":true' "$OUT"
assert_contains "get_publishable_keys: publishable not disabled" '"disabled":false' "$OUT"
assert_not_contains "get_publishable_keys never returns service_role" 'service_role' "$OUT"
assert_not_contains "get_publishable_keys never returns secret keys" 'sb_secret' "$OUT"
assert_not_contains "get_publishable_keys never returns the service JWT" 'eyJservice' "$OUT"
REQ=$(fixture_last 'api-keys?reveal=false')
assert_contains "get_publishable_keys requests reveal=false" '"query": {"reveal": "false"}' "$REQ"
REQ=$(fixture_last 'api-keys/legacy')
assert_contains "get_publishable_keys checks legacy key status" 'api-keys/legacy' "$REQ"
OUT=$(mcp_call get_publishable_keys '{"project_id":"kkkkkkkkkkkkkkkkkkkk"}')
assert_contains "get_publishable_keys: no client keys is an error" 'No client-safe API keys' "$OUT"
OUT=$(mcp_call get_publishable_keys '{"project_id":"mmmmmmmmmmmmmmmmmmmm"}')
assert_contains "get_publishable_keys: legacy status failure is tolerated" 'sb_publishable_abc123' "$OUT"
assert_not_contains "get_publishable_keys: no disabled field without legacy status" '"disabled"' "$OUT"
OUT=$(mcp_call generate_typescript_types "{\"project_id\":\"$REF\"}")
assert_contains "generate_typescript_types returns types" 'export type Database' "$OUT"
assert_contains "generate_typescript_types defaults to public" '"included_schemas":["public"]' "$OUT"
REQ=$(fixture_last types/typescript)
assert_contains "generate_typescript_types sends included_schemas" 'included_schemas=public' "$REQ"
OUT=$(mcp_call generate_typescript_types "{\"project_id\":\"$REF\",\"included_schemas\":[\"public\",\"auth\"]}")
assert_contains "generate_typescript_types passes multiple schemas" 'schemas: public,auth' "$OUT"
REQ=$(fixture_last types/typescript)
assert_contains "generate_typescript_types encodes the comma" 'included_schemas=public%2Cauth' "$REQ"
OUT=$(mcp_call generate_typescript_types "{\"project_id\":\"$REF\",\"included_schemas\":[\"public\",\"x;y\"]}")
assert_contains "generate_typescript_types rejects a bad schema" 'included_schemas is invalid' "$OUT"
OUT=$(mcp_call generate_typescript_types "{\"project_id\":\"$REF\",\"included_schemas\":[]}")
assert_contains "generate_typescript_types rejects an empty list" 'at least one schema' "$OUT"
OUT=$(mcp_call generate_typescript_types '{"project_id":"tttttttttttttttttttt"}')
assert_contains "generate_typescript_types caps output at 1 MiB" '"truncated":true' "$OUT"

echo "== bridge failures: size cap and deadline get specific hints =="
OUT=$(mcp_call generate_typescript_types '{"project_id":"hhhhhhhhhhhhhhhhhhhh"}')
assert_contains "oversized upstream body is a tool error" '"isError":true' "$OUT"
assert_contains "oversized upstream body names the cap" 'exceeded the outbound body cap (4 MiB' "$OUT"
assert_contains "oversized upstream body says to narrow the request" 'narrow included_schemas' "$OUT"
assert_not_contains "oversized upstream body does not blame allowedHosts" 'allowedHosts' "$OUT"
OUT=$(mcp_call generate_typescript_types "{\"project_id\":\"$REF\"}")
assert_contains "instance still alive after an oversized body" 'export type Database' "$OUT"
OUT=$(mcp_call_on "$BAD_BASE" get_project '{"project_id":"ssssssssssssssssssss"}')
assert_contains "outbound deadline is a tool error" '"isError":true' "$OUT"
assert_contains "outbound deadline names the timeout" 'did not answer within 2000 ms' "$OUT"
assert_contains "outbound deadline suggests narrowing and the status page" 'status.supabase.com' "$OUT"
assert_not_contains "outbound deadline does not blame allowedHosts" 'allowedHosts' "$OUT"
OUT=$(mcp_call_on "$BAD_BASE" get_project_url '{"project_id":"ssssssssssssssssssss"}')
assert_contains "instance still alive after a timeout" 'ssssssssssssssssssss.supabase.co' "$OUT"

if [ "${E2E_PG:-0}" = "1" ]; then
  echo "== E2E_PG: borrowed pg-meta SQL against local Postgres =="
  OUT=$(mcp_call list_tables '{"project_id":"gggggggggggggggggggg","schemas":["pg_catalog"],"verbose":true,"max_rows":5}')
  assert_contains "pg-meta tables SQL parses on Postgres 16" '"name":"pg_catalog.' "$OUT"
  assert_contains "pg-meta tables SQL yields columns" '"data_type"' "$OUT"
  OUT=$(mcp_call list_tables '{"project_id":"gggggggggggggggggggg","schemas":[]}')
  assert_contains "pg-meta tables SQL with the system-schema exclusion parses" '"truncated":false' "$OUT"
  OUT=$(mcp_call list_extensions '{"project_id":"gggggggggggggggggggg"}')
  assert_contains "pg-meta extensions SQL parses on Postgres 16" '"name":"plpgsql"' "$OUT"
fi

if [ "${E2E_LIVE:-0}" = "1" ] && [ -n "${SUPABASE_ACCESS_TOKEN:-}" ]; then
  echo "== E2E_LIVE: api.supabase.com =="
  start_extra "$LIVE_PORT" --env "SUPABASE_ACCESS_TOKEN=$SUPABASE_ACCESS_TOKEN"
  OUT=$(mcp_call_on "http://127.0.0.1:${LIVE_PORT}/" check_auth '{}')
  assert_contains "live check_auth against api.supabase.com" '"status":"ok"' "$OUT"
fi

guard_tests
mcp_harness_report
