#!/usr/bin/env bash
# End-to-end tests for atlassian-jira-mcp. Framework checks (protocol, spec
# enforcement, discovery route, skills over MCP, robustness, Host guard) come
# from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases live
# below. Every upstream call goes to the hermetic Jira impersonator in
# scripts/jira_fixture.py (a ThreadingHTTPServer), selected with the
# ATLASSIAN_BASE_URL override — no network, no credentials.
#
# Instances under test:
#   primary  — classic-token route, full credentials
#   guard    — no ATLASSIAN_API_TOKEN (missing-secret path) + JIRA_READ_ONLY=true
#              (gated-write refusal) + MCP_ALLOWED_HOSTS pinned (Host guard)
#   cloud    — ATLASSIAN_CLOUD_ID set (api.atlassian.com/ex/jira/<id> route)
#              + JIRA_PROJECTS_FILTER (JQL rewrite, project list filter)
#
# Usage: scripts/e2e.sh [--no-build]
# Optional live smoke (reads only) against a real site:
#   E2E_LIVE=1 ATLASSIAN_SITE=… ATLASSIAN_EMAIL=… ATLASSIAN_API_TOKEN=… scripts/e2e.sh
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9586}
GUARD_PORT=${GUARD_PORT:-9587}
FIXTURE_PORT=${FIXTURE_PORT:-9588}
CLOUD_PORT=${CLOUD_PORT:-9589}
LIVE_PORT=${LIVE_PORT:-9590}
WASM=${WASM:-target/wasm32-wasip2/release/atlassian_jira_mcp.wasm}
SKILL_NAME=atlassian-jira-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

CLOUD_PID=""
LIVE_PID=""
cleanup_all() {
  [ -n "$CLOUD_PID" ] && kill "$CLOUD_PID" 2>/dev/null
  [ -n "$LIVE_PID" ] && kill "$LIVE_PID" 2>/dev/null
  mcp_harness_cleanup
}
trap cleanup_all EXIT

FIXTURE="http://127.0.0.1:${FIXTURE_PORT}"
GUARD="http://127.0.0.1:${GUARD_PORT}/"
CLOUD="http://127.0.0.1:${CLOUD_PORT}/"

# What the fixture last received (any method) / last stored (204 writes).
last_request() { curl -sS --max-time 10 "$FIXTURE/__fixture/last-request"; }
last_write() { curl -sS --max-time 10 "$FIXTURE/__fixture/last-write"; }

# The concurrency test in framework_tests fires this tool 8x in parallel —
# an outbound tool, so concurrent outbound is what gets exercised.
FIRST_TOOL_NAME=get_myself
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"accountId"'

TOOLS="check_auth get_myself search_issues count_issues get_issue create_issue update_issue \
add_comment get_comments get_transitions transition_issue assign_issue list_projects \
get_project list_issue_types get_create_fields search_users"

mcp_build_if_needed "${1:-}"

echo "starting jira fixture on :${FIXTURE_PORT}..."
python3 scripts/jira_fixture.py "$FIXTURE_PORT" >"$E2E_TMP/fixture.log" 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "$FIXTURE/__fixture/health" && break
  sleep 0.2
done

COMMON=(--env "ATLASSIAN_BASE_URL=$FIXTURE" --env ATLASSIAN_SITE=fixture.atlassian.net
        --env MCP_OUTBOUND_TIMEOUT_MS=10000)
mcp_harness_start "${COMMON[@]}" --env ATLASSIAN_EMAIL=e2e@example.com --env ATLASSIAN_API_TOKEN=tok-e2e
# Guard instance: no token, read-only.
mcp_harness_start_guard "${COMMON[@]}" --env ATLASSIAN_EMAIL=e2e@example.com --env JIRA_READ_ONLY=true
echo "starting cloud-id instance on :${CLOUD_PORT}..."
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env ATLASSIAN_EMAIL=cloud@example.com \
  --env ATLASSIAN_API_TOKEN=tok-cloud --env ATLASSIAN_CLOUD_ID=abc-123 \
  --env "JIRA_PROJECTS_FILTER= proj, e2e ,,ops" \
  --addr "127.0.0.1:${CLOUD_PORT}" "$WASM" >"$E2E_TMP/cloud.log" 2>&1 &
CLOUD_PID=$!
mcp_wait_ready "$CLOUD_PORT"

# shellcheck disable=SC2086
framework_tests $TOOLS
discovery_tests search_issues
skills_tests "$SKILL_NAME" "references/TOOLS.md"

echo "== discovery: credentials block =="
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / carries the credentials block" '"credentials"' "$ROOT"
assert_contains "GET / names the secret ref" '"ref": "atlassian-api-token"' "$ROOT"
assert_contains "GET / reports the token configured" '"status": "configured"' "$ROOT"
assert_not_contains "GET / never leaks the token value" 'tok-e2e' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD")
assert_contains "GET / on the guard reports the token missing" '"status": "missing"' "$ROOT"

echo "== credentials (check_auth / get_myself) =="
# The fixture only answers 200 for `Basic base64(e2e@example.com:tok-e2e)`
# with Accept: application/json — a status of ok proves the header encoding.
OUT=$(mcp_call check_auth '{}')
assert_json "check_auth status ok" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
assert_json "check_auth returns the accountId" 'r["result"]["structuredContent"]["account"]["accountId"] == "5b10ac8d82e05b22cc7d4ef5"' "$OUT"
assert_json "check_auth reports the site route" 'r["result"]["structuredContent"]["route"] == "site"' "$OUT"
assert_json "check_auth names the secret ref" 'r["result"]["structuredContent"]["credential"]["ref"] == "atlassian-api-token"' "$OUT"
assert_not_contains "check_auth never echoes the token" 'tok-e2e' "$OUT"
REQ=$(last_request)
assert_json "Basic auth decodes to the configured email" 'r["authUser"] == "e2e@example.com"' "$REQ"
assert_json "Accept: application/json is sent" '"application/json" in r["accept"]' "$REQ"
assert_json "User-Agent identifies the server" 'r["userAgent"].startswith("atlassian-jira-mcp/")' "$REQ"

OUT=$(mcp_call get_myself '{}')
assert_json "get_myself is not an error" 'r["result"]["isError"] is False' "$OUT"
assert_json "get_myself returns displayName" 'r["result"]["structuredContent"]["displayName"] == "E2E Runner"' "$OUT"
assert_json "get_myself drops avatar noise" '"avatarUrls" not in r["result"]["structuredContent"]' "$OUT"

echo "== missing secret (guard instance) =="
OUT=$(mcp_call_on "$GUARD" get_myself '{}')
assert_contains "missing secret is a tool error" '"isError":true' "$OUT"
assert_contains "missing secret names the env var" 'ATLASSIAN_API_TOKEN is not set' "$OUT"
assert_contains "missing secret names the secret ref" 'atlassian-api-token' "$OUT"
assert_contains "missing secret points at the token page" 'https://id.atlassian.com/manage-profile/security/api-tokens' "$OUT"
assert_contains "missing secret names the deploy mechanism" 'cosmonic_set_secret' "$OUT"
OUT=$(mcp_call_on "$GUARD" check_auth '{}')
assert_json "check_auth reports status missing" 'r["result"]["structuredContent"]["status"] == "missing"' "$OUT"
assert_json "check_auth missing carries remediation" '"atlassian-api-token" in r["result"]["structuredContent"]["remediation"]' "$OUT"
OUT=$(mcp_call_on "$GUARD" search_issues '{"jql":"project = E2E"}')
assert_contains "search_issues without a token is the same actionable error" 'ATLASSIAN_API_TOKEN is not set' "$OUT"

echo "== invalid credential (401) =="
OUT=$(mcp_call get_issue '{"issue_key":"AUTH-1"}')
assert_contains "401 is a tool error" '"isError":true' "$OUT"
assert_contains "401 carries the upstream message" 'Client must be authenticated' "$OUT"
assert_contains "401 hints at token expiry" 'expire' "$OUT"
assert_contains "401 hints at scoped tokens" 'ATLASSIAN_CLOUD_ID' "$OUT"
assert_contains "401 names the secret ref" 'atlassian-api-token' "$OUT"
assert_json "401 is not retryable" 'r["result"]["structuredContent"]["error"]["retryable"] is False' "$OUT"

echo "== read-only gate (JIRA_READ_ONLY=true on the guard instance) =="
for CASE in \
  'create_issue|{"project_key":"E2E","issue_type":"Task","summary":"x"}' \
  'update_issue|{"issue_key":"E2E-1","summary":"x"}' \
  'add_comment|{"issue_key":"E2E-1","body":"x"}' \
  'transition_issue|{"issue_key":"E2E-1","transition":"Done"}' \
  'assign_issue|{"issue_key":"E2E-1","account_id":"5b10ac8d82e05b22cc7d4ef5"}'; do
  TOOL=${CASE%%|*}; ARGS=${CASE#*|}
  OUT=$(mcp_call_on "$GUARD" "$TOOL" "$ARGS")
  assert_contains "$TOOL refused in read-only mode" 'JIRA_READ_ONLY=true' "$OUT"
  assert_not_contains "$TOOL read-only refusal precedes the credential check" 'ATLASSIAN_API_TOKEN is not set' "$OUT"
done
OUT=$(mcp_call_on "$GUARD" get_comments '{"issue_key":"E2E-1"}')
assert_not_contains "read tools are not gated by read-only mode" 'JIRA_READ_ONLY=true' "$OUT"

echo "== search_issues =="
OUT=$(mcp_call search_issues '{"jql":"project = E2E ORDER BY created DESC","max_results":1000}')
assert_json "search returns page 1 (2 issues)" 'r["result"]["structuredContent"]["count"] == 2' "$OUT"
assert_json "search exposes next_page_token" 'r["result"]["structuredContent"]["next_page_token"] == "p2"' "$OUT"
assert_json "search is_last false on page 1" 'r["result"]["structuredContent"]["is_last"] is False' "$OUT"
assert_json "search compacts status to name+category" 'r["result"]["structuredContent"]["issues"][0]["fields"]["status"]["statusCategory"] == "To Do"' "$OUT"
assert_json "search compacts assignee" 'r["result"]["structuredContent"]["issues"][0]["fields"]["assignee"]["displayName"] == "Mia Krystof"' "$OUT"
assert_json "search drops null fields" '"resolution" not in r["result"]["structuredContent"]["issues"][0]["fields"]' "$OUT"
REQ=$(last_request)
assert_json "search uses POST /search/jql (not the removed /search)" 'r["method"] == "POST" and r["path"] == "/search/jql"' "$REQ"
assert_json "search clamps maxResults 1000 -> 100" 'r["body"]["maxResults"] == 100' "$REQ"
assert_json "search sends the default field list" 'r["body"]["fields"] == ["summary","status","assignee","priority","issuetype","created","updated"]' "$REQ"
assert_json "search omits nextPageToken on page 1" '"nextPageToken" not in r["body"]' "$REQ"

# ADF rendering of the rich description that came back with page 1.
DESC='r["result"]["structuredContent"]["issues"][0]["fields"]["description"]'
assert_json "ADF heading rendered" "'## Überblick' in $DESC" "$OUT"
assert_json "ADF mention rendered" "'@Mia Krystof' in $DESC" "$OUT"
assert_json "ADF inlineCard rendered as URL" "'https://example.com/spec' in $DESC" "$OUT"
assert_json "ADF link mark rendered" "'docs (https://example.com/docs)' in $DESC" "$OUT"
assert_json "ADF emoji rendered" "'🎉' in $DESC" "$OUT"
assert_json "ADF bullet list rendered" "'- first item' in $DESC" "$OUT"
assert_json "ADF nested ordered list keeps start number" "'3. nested three' in $DESC" "$OUT"
assert_json "ADF code block rendered" "'\`\`\`rust' in $DESC and 'fn main() {}' in $DESC" "$OUT"
assert_json "ADF table rendered" "'| Col A | Col B |' in $DESC and '| a1 | b1 |' in $DESC" "$OUT"
assert_json "ADF blockquote rendered" "'> quoted line' in $DESC" "$OUT"
assert_json "ADF panel rendered" "'[info]' in $DESC and '> panel body' in $DESC" "$OUT"
assert_json "ADF media placeholder" "'[media: diagram.png]' in $DESC" "$OUT"
assert_json "ADF date rendered" "'2023-11-14' in $DESC" "$OUT"
assert_json "ADF status rendered" "'[BLOCKED]' in $DESC" "$OUT"
assert_json "ADF unknown node degrades to text" "'unknown node text survives' in $DESC" "$OUT"
assert_json "ADF hardBreak + unicode" "'line one\nline two 日本語' in $DESC" "$OUT"
assert_json "plain-string description passes through" 'r["result"]["structuredContent"]["issues"][1]["fields"]["description"] == "plain string description"' "$OUT"

OUT=$(mcp_call search_issues '{"jql":"project = E2E","next_page_token":"p2","max_results":0,"fields":["summary","description"],"expand":"names"}')
assert_json "search page 2 is last" 'r["result"]["structuredContent"]["is_last"] is True and "next_page_token" not in r["result"]["structuredContent"]' "$OUT"
assert_json "search page 2 returns 1 issue" 'r["result"]["structuredContent"]["count"] == 1' "$OUT"
assert_json "55-deep ADF does not trap and is cut by the renderer depth cap" 'r["result"]["structuredContent"]["issues"][0]["key"] == "E2E-3" and "deep" not in r["result"]["structuredContent"]["issues"][0]["fields"]["description"]' "$OUT"
assert_json "expand=names surfaces names" 'r["result"]["structuredContent"]["names"]["summary"] == "Summary"' "$OUT"
REQ=$(last_request)
assert_json "search forwards nextPageToken" 'r["body"]["nextPageToken"] == "p2"' "$REQ"
assert_json "search clamps maxResults 0 -> 1" 'r["body"]["maxResults"] == 1' "$REQ"
assert_json "search forwards explicit fields" 'r["body"]["fields"] == ["summary","description"]' "$REQ"
assert_json "search forwards expand" 'r["body"]["expand"] == "names"' "$REQ"

OUT=$(mcp_call search_issues '{"jql":"project = E2E","next_page_token":"absurd-depth"}')
assert_contains "300-deep JSON is a clean decode error, not a trap" 'unreadable answer' "$OUT"
assert_contains "300-deep JSON is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call search_issues '{"jql":"BADFIELD = 1"}')
assert_contains "search 400 passes JQL error through" "Field 'BADFIELD' does not exist" "$OUT"
assert_contains "search 400 is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call search_issues '{"jql":"ORDER BY key DESC"}')
assert_contains "unbounded JQL 400 passes through" 'unbounded' "$OUT"
OUT=$(mcp_call search_issues '{"jql":"project = RATE"}')
assert_contains "search 429 reports Retry-After" '"retry_after_seconds":7' "$OUT"
assert_contains "search 429 says rate limited" 'Rate limited' "$OUT"
assert_contains "search 429 carries the reason" 'burst-limit' "$OUT"
OUT=$(mcp_call search_issues '{"jql":"   "}')
assert_contains "empty JQL rejected before dialing" 'jql must not be empty' "$OUT"
OUT=$(mcp_call search_issues '{"jql":"summary ~ \"a\\\"; DROP TABLE issues; --\" AND text ~ \"x\ny\""}')
assert_json "injection-shaped JQL is passed through verbatim (Jira parses it)" 'r["result"]["structuredContent"]["count"] == 2' "$OUT"
REQ=$(last_request)
assert_json "JQL quotes survive, newlines become spaces" '"DROP TABLE issues; --" in r["body"]["jql"] and "\n" not in r["body"]["jql"]' "$REQ"
BIG=$(python3 -c 'print("ü" * 10001)')
OUT=$(mcp_call search_issues "{\"jql\":\"$BIG\"}")
assert_contains "10001-char JQL rejected" 'longer than 10000' "$OUT"
OUT=$(mcp_call search_issues '{"jql":"project = E2E","max_results":"lots"}')
assert_not_contains "max_results as a string is a clean error" '"isError":false' "$OUT"
OUT=$(mcp_call search_issues '{}')
assert_not_contains "missing jql is a clean error" '"isError":false' "$OUT"

echo "== count_issues =="
OUT=$(mcp_call count_issues '{"jql":"project = E2E"}')
assert_json "count returns the approximate count" 'r["result"]["structuredContent"]["count"] == 42' "$OUT"
REQ=$(last_request)
assert_json "count uses POST /search/approximate-count" 'r["path"] == "/search/approximate-count" and r["body"]["jql"] == "project = E2E"' "$REQ"
OUT=$(mcp_call count_issues '{"jql":"BADFIELD = 1"}')
assert_contains "count 400 passes JQL error through" "BADFIELD" "$OUT"
OUT=$(mcp_call count_issues '{"jql":""}')
assert_contains "count rejects empty JQL" 'jql must not be empty' "$OUT"

echo "== scoped-token route + JIRA_PROJECTS_FILTER (cloud instance) =="
OUT=$(mcp_call_on "$CLOUD" check_auth '{}')
assert_json "cloud instance authenticates through the gateway prefix" 'r["result"]["structuredContent"]["status"] == "ok" and r["result"]["structuredContent"]["route"] == "gateway"' "$OUT"
assert_json "cloud instance base_url carries /ex/jira/<cloudId>" 'r["result"]["structuredContent"]["base_url"].endswith("/ex/jira/abc-123")' "$OUT"
assert_json "cloud instance reports the parsed projects filter" 'r["result"]["structuredContent"]["projects_filter"] == ["PROJ","E2E","OPS"]' "$OUT"
REQ=$(last_request)
assert_json "fixture saw the cloud id in the path" 'r["cloudId"] == "abc-123" and r["authUser"] == "cloud@example.com"' "$REQ"
OUT=$(mcp_call_on "$CLOUD" search_issues '{"jql":"assignee = currentUser() order by updated desc"}')
assert_json "filtered search succeeds" 'r["result"]["structuredContent"]["count"] == 2' "$OUT"
REQ=$(last_request)
assert_json "projects filter wraps JQL before ORDER BY" 'r["body"]["jql"] == "(assignee = currentUser()) AND project in (PROJ, E2E, OPS) order by updated desc"' "$REQ"
OUT=$(mcp_call_on "$CLOUD" search_issues '{"jql":"summary ~ \"order by\" AND status = Open ORDER BY key"}')
REQ=$(last_request)
assert_json "projects filter ignores ORDER BY inside quotes" 'r["body"]["jql"] == "(summary ~ \"order by\" AND status = Open) AND project in (PROJ, E2E, OPS) ORDER BY key"' "$REQ"
OUT=$(mcp_call_on "$CLOUD" search_issues '{"jql":"ORDER BY created"}')
REQ=$(last_request)
assert_json "ORDER BY-only JQL becomes bounded by the filter" 'r["body"]["jql"] == "project in (PROJ, E2E, OPS) ORDER BY created"' "$REQ"
OUT=$(mcp_call_on "$CLOUD" count_issues '{"jql":"type = Bug"}')
REQ=$(last_request)
assert_json "count applies the projects filter" 'r["body"]["jql"] == "(type = Bug) AND project in (PROJ, E2E, OPS)"' "$REQ"
OUT=$(mcp_call_on "$CLOUD" list_projects '{}')
assert_json "list_projects honours the filter (E2E, OPS only)" 'sorted(p["key"] for p in r["result"]["structuredContent"]["projects"]) == ["E2E","OPS"]' "$OUT"
REQ=$(last_request)
assert_json "list_projects sends the keys filter upstream" 'r["query"]["keys"] == ["PROJ","E2E","OPS"]' "$REQ"

echo "== get_issue =="
OUT=$(mcp_call get_issue '{"issue_key":"E2E-7"}')
assert_json "get_issue returns the issue" 'r["result"]["structuredContent"]["key"] == "E2E-7" and r["result"]["structuredContent"]["fields"]["summary"] == "fetched issue E2E-7"' "$OUT"
assert_json "get_issue renders the description" '"- first item" in r["result"]["structuredContent"]["fields"]["description"]' "$OUT"
assert_json "get_issue adds a browse URL" 'r["result"]["structuredContent"]["url"] == "https://fixture.atlassian.net/browse/E2E-7"' "$OUT"
REQ=$(last_request)
assert_json "get_issue default fields exclude comment/attachment/worklog" 'r["query"]["fields"] == "*navigable,-comment,-attachment,-worklog"' "$REQ"
assert_json "get_issue percent-encodes the fields query" '"fields=%2Anavigable%2C-comment" in r["rawQuery"]' "$REQ"
OUT=$(mcp_call get_issue '{"issue_key":"E2E-7","include_comments":true,"expand":"changelog, renderedFields"}')
assert_json "include_comments renders ADF comment bodies" '"first comment @E2E Runner" in r["result"]["structuredContent"]["fields"]["comment"]["comments"][0]["body"]' "$OUT"
assert_json "legacy string comment bodies pass through" 'r["result"]["structuredContent"]["fields"]["comment"]["comments"][1]["body"] == "legacy plain-string body"' "$OUT"
assert_json "expand=changelog surfaces the changelog" 'r["result"]["structuredContent"]["changelog"]["total"] == 1' "$OUT"
assert_json "expand=renderedFields surfaces HTML" '"rendered html" in r["result"]["structuredContent"]["renderedFields"]["description"]' "$OUT"
REQ=$(last_request)
assert_json "include_comments adds comment to fields" 'r["query"]["fields"] == "*navigable,-attachment,-worklog,comment"' "$REQ"
assert_json "expand whitespace is stripped" 'r["query"]["expand"] == "changelog,renderedFields"' "$REQ"
OUT=$(mcp_call get_issue '{"issue_key":"10001","fields":["summary"]}')
assert_json "numeric issue id accepted" 'r["result"]["structuredContent"]["key"] == "10001"' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"NOPE-1"}')
assert_contains "404 explains missing-or-forbidden" 'Issue NOPE-1 does not exist or you do not have permission' "$OUT"
assert_contains "404 is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"FORB-1"}')
assert_contains "403 explains the permission" 'Permission denied' "$OUT"
assert_contains "403 mentions scopes" 'read:jira-work' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"RATE-1"}')
assert_contains "429 on get_issue reports Retry-After" '"retry_after_seconds":7' "$OUT"
assert_contains "429 carries the points-budget reason" 'points-budget' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"HTML-1"}')
assert_contains "HTML body is detected" 'HTML page instead of JSON' "$OUT"
assert_contains "HTML body hints at ATLASSIAN_SITE" 'ATLASSIAN_SITE' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"BOOM-1"}')
assert_contains "500 says retry once" 'retry once' "$OUT"
assert_json "500 is retryable" 'r["result"]["structuredContent"]["error"]["retryable"] is True' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"PROXY-1"}')
assert_contains "502 gateway message surfaces" 'Bad Gateway from proxy' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"GONE-1"}')
assert_contains "410 hints at a stale base URL" 'ATLASSIAN_BASE_URL' "$OUT"
OUT=$(mcp_call get_issue '{"issue_key":"CONF-1"}')
assert_contains "409 explains the conflict" 'concurrently' "$OUT"
for BAD in 'A/../B' 'E2E-1?x=1' '../myself' 'E2E-' '-1' 'e2e 1' 'E2E-1#frag' 'É2E-1' '%2e%2e/x'; do
  OUT=$(mcp_call get_issue "{\"issue_key\":\"$BAD\"}")
  assert_contains "issue_key '$BAD' rejected by the validator" 'not an issue key' "$OUT"
done
OUT=$(mcp_call get_issue '{}')
assert_not_contains "missing issue_key is a clean error" '"isError":false' "$OUT"

echo "== create_issue =="
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task","summary":"  Fix the ☃ thing  ","description":"para one\nline two\n\n\npara two 日本語","labels":["e2e","fixture","e2e"],"priority":"High","assignee_account_id":"5b10ac8d82e05b22cc7d4ef5","parent_key":"E2E-1","extra_fields":{"customfield_10016":5}}')
assert_json "create_issue returns the new key" 'r["result"]["structuredContent"]["key"] == "E2E-101" and r["result"]["structuredContent"]["ok"] is True' "$OUT"
assert_json "create_issue returns a browse URL" 'r["result"]["structuredContent"]["url"] == "https://fixture.atlassian.net/browse/E2E-101"' "$OUT"
W=$(last_write)
assert_json "create sent POST /issue" 'r["method"] == "POST" and r["path"] == "/issue"' "$W"
assert_json "create trims the summary" 'r["body"]["fields"]["summary"] == "Fix the ☃ thing"' "$W"
assert_json "create sends project by key and issuetype by name" 'r["body"]["fields"]["project"] == {"key":"E2E"} and r["body"]["fields"]["issuetype"] == {"name":"Task"}' "$W"
assert_json "create wraps description into an ADF doc" 'r["body"]["fields"]["description"]["type"] == "doc" and r["body"]["fields"]["description"]["version"] == 1' "$W"
assert_json "ADF: blank lines split paragraphs" 'len(r["body"]["fields"]["description"]["content"]) == 2' "$W"
assert_json "ADF: single newline becomes hardBreak" '[n["type"] for n in r["body"]["fields"]["description"]["content"][0]["content"]] == ["text","hardBreak","text"]' "$W"
assert_json "ADF: unicode text preserved" 'r["body"]["fields"]["description"]["content"][1]["content"][0]["text"] == "para two 日本語"' "$W"
assert_json "create dedupes labels" 'r["body"]["fields"]["labels"] == ["e2e","fixture"]' "$W"
assert_json "create sends priority/assignee/parent/extra fields" 'r["body"]["fields"]["priority"] == {"name":"High"} and r["body"]["fields"]["assignee"] == {"id":"5b10ac8d82e05b22cc7d4ef5"} and r["body"]["fields"]["parent"] == {"key":"E2E-1"} and r["body"]["fields"]["customfield_10016"] == 5' "$W"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"10001","summary":"by id"}')
W=$(last_write)
assert_json "numeric issue_type is sent as id" 'r["body"]["fields"]["issuetype"] == {"id":"10001"}' "$W"
assert_json "no description means no description field" '"description" not in r["body"]["fields"]' "$W"
BIG=$(python3 -c 'print("ü" * 10000)')
OUT=$(mcp_call create_issue "{\"project_key\":\"E2E\",\"issue_type\":\"Task\",\"summary\":\"$BIG\"}")
assert_contains "10 kB unicode summary rejected (255 limit)" 'longer than 255' "$OUT"
OUT=$(mcp_call create_issue "{\"project_key\":\"E2E\",\"issue_type\":\"Task\",\"summary\":\"ok\",\"description\":\"$BIG\"}")
assert_json "10 kB unicode description accepted" 'r["result"]["structuredContent"]["key"] == "E2E-101"' "$OUT"
HUGE=$(python3 -c 'print("x" * 40000)')
OUT=$(mcp_call create_issue "{\"project_key\":\"E2E\",\"issue_type\":\"Task\",\"summary\":\"ok\",\"description\":\"$HUGE\"}")
assert_contains "40 kB description rejected" 'longer than 32767' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task","summary":"x","labels":["has space"]}')
assert_contains "label with a space rejected" 'cannot contain spaces' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task","summary":"x","extra_fields":{"project":{"key":"OTHER"}}}')
assert_contains "extra_fields may not override project" 'may not set `project`' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task","summary":"x","extra_fields":{"summary":"y"}}')
assert_contains "extra_fields may not override summary" 'may not set `summary`' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task","summary":"x","priority":"Bogus"}')
assert_contains "400 field errors are rendered per field" "priority: Priority name 'Bogus' is not valid" "$OUT"
assert_contains "400 on create hints at get_create_fields" 'get_create_fields' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task","summary":"x","extra_fields":{"customfield_99999":1}}')
assert_contains "unknown custom field error passes through" 'not on the appropriate screen' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"NOPE","issue_type":"Task","summary":"x"}')
assert_contains "bad project 400 passes through" 'Specify a valid project ID or key' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E/../x","issue_type":"Task","summary":"x"}')
assert_contains "traversal in project_key rejected" 'not a project key' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task","summary":"   "}')
assert_contains "blank summary rejected" 'summary must not be empty' "$OUT"
OUT=$(mcp_call create_issue '{"project_key":"E2E","issue_type":"Task"}')
assert_not_contains "missing summary param is a clean error" '"isError":false' "$OUT"

echo "== update_issue =="
OUT=$(mcp_call update_issue '{"issue_key":"E2E-5","summary":"new summary","add_labels":["urgent"],"remove_labels":["stale"],"assignee_account_id":"","notify_users":false}')
assert_json "update_issue returns ok on 204" 'r["result"]["structuredContent"] == {"ok": True, "issue_key": "E2E-5", "url": "https://fixture.atlassian.net/browse/E2E-5"}' "$OUT"
W=$(last_write)
assert_json "update sent PUT /issue/E2E-5" 'r["method"] == "PUT" and r["path"] == "/issue/E2E-5"' "$W"
assert_json "update forwards notifyUsers=false, returnIssue=false" 'r["query"]["notifyUsers"] == "false" and r["query"]["returnIssue"] == "false"' "$W"
assert_json "update sends summary and unassigns with null" 'r["body"]["fields"]["summary"] == "new summary" and r["body"]["fields"]["assignee"] is None' "$W"
assert_json "update sends label add/remove operations" 'r["body"]["update"]["labels"] == [{"add":"urgent"},{"remove":"stale"}]' "$W"
OUT=$(mcp_call update_issue '{"issue_key":"E2E-5","description":"one\n\ntwo","return_issue":true}')
assert_json "return_issue=true returns the issue" 'r["result"]["structuredContent"]["ok"] is True and r["result"]["structuredContent"]["fields"]["description"] == "one\ntwo"' "$OUT"
W=$(last_write)
assert_json "update wraps description in ADF" 'r["body"]["fields"]["description"]["type"] == "doc" and len(r["body"]["fields"]["description"]["content"]) == 2' "$W"
OUT=$(mcp_call update_issue '{"issue_key":"E2E-5"}')
assert_contains "update with no change rejected" 'nothing to update' "$OUT"
OUT=$(mcp_call update_issue '{"issue_key":"E2E-5","labels":["a"],"add_labels":["b"]}')
assert_contains "labels and add_labels together rejected" 'not both' "$OUT"
OUT=$(mcp_call update_issue '{"issue_key":"E2E-5","extra_fields":{"customfield_99999":1}}')
assert_contains "update 400 passes the field error" 'customfield_99999' "$OUT"
OUT=$(mcp_call update_issue '{"issue_key":"NOPE-1","summary":"x"}')
assert_contains "update 404 explains missing-or-forbidden" 'does not exist or you do not have permission' "$OUT"
OUT=$(mcp_call update_issue '{"issue_key":"E2E-5","assignee_account_id":"x\"; drop"}')
assert_contains "malformed accountId rejected" 'not an Atlassian accountId' "$OUT"

echo "== add_comment =="
OUT=$(mcp_call add_comment '{"issue_key":"E2E-5","body":"hello\nworld ☃"}')
assert_json "add_comment returns the new id" 'r["result"]["structuredContent"]["id"] == "10777" and r["result"]["structuredContent"]["ok"] is True' "$OUT"
assert_json "add_comment returns a focused URL" 'r["result"]["structuredContent"]["url"] == "https://fixture.atlassian.net/browse/E2E-5?focusedCommentId=10777"' "$OUT"
W=$(last_write)
assert_json "comment body sent as ADF" 'r["body"]["body"]["type"] == "doc" and r["body"]["body"]["content"][0]["content"][2]["text"] == "world ☃"' "$W"
assert_json "comment without visibility omits it" '"visibility" not in r["body"]' "$W"
OUT=$(mcp_call add_comment '{"issue_key":"E2E-5","body":"secret","visibility_type":"role","visibility_value":"Administrators"}')
W=$(last_write)
assert_json "comment visibility forwarded" 'r["body"]["visibility"] == {"type":"role","value":"Administrators"}' "$W"
OUT=$(mcp_call add_comment '{"issue_key":"E2E-5","body":"x","visibility_type":"bogus","visibility_value":"y"}')
assert_contains "bad visibility_type rejected" 'visibility_type must be' "$OUT"
OUT=$(mcp_call add_comment '{"issue_key":"E2E-5","body":"x","visibility_type":"role"}')
assert_contains "visibility_type without value rejected" 'given together' "$OUT"
OUT=$(mcp_call add_comment '{"issue_key":"E2E-5","body":"x","visibility_type":"role","visibility_value":"no-such-role"}')
assert_contains "400 visibility error passes through" "Role 'no-such-role' does not exist" "$OUT"
OUT=$(mcp_call add_comment '{"issue_key":"E2E-5","body":"  "}')
assert_contains "empty comment rejected" 'body must not be empty' "$OUT"
OUT=$(mcp_call add_comment "{\"issue_key\":\"E2E-5\",\"body\":\"$HUGE\"}")
assert_contains "40 kB comment rejected" 'longer than 32767' "$OUT"
OUT=$(mcp_call add_comment '{"issue_key":"NOPE-1","body":"x"}')
assert_contains "comment on missing issue is 404" 'Issue NOPE-1 does not exist' "$OUT"

echo "== get_comments =="
OUT=$(mcp_call get_comments '{"issue_key":"E2E-5"}')
assert_json "get_comments returns both comments newest first" 'r["result"]["structuredContent"]["total"] == 2 and r["result"]["structuredContent"]["comments"][0]["id"] == "10501"' "$OUT"
assert_json "get_comments renders ADF bodies" '"first comment @E2E Runner" in r["result"]["structuredContent"]["comments"][1]["body"]' "$OUT"
assert_json "get_comments keeps visibility" 'r["result"]["structuredContent"]["comments"][1]["visibility"]["value"] == "Administrators"' "$OUT"
assert_json "get_comments has_more false" 'r["result"]["structuredContent"]["has_more"] is False' "$OUT"
REQ=$(last_request)
assert_json "get_comments defaults startAt=0 maxResults=50 orderBy=-created" 'r["query"] == {"startAt":"0","maxResults":"50","orderBy":"-created"}' "$REQ"
OUT=$(mcp_call get_comments '{"issue_key":"E2E-5","start_at":-5,"max_results":500,"order_by":"created"}')
assert_json "get_comments order_by created oldest first" 'r["result"]["structuredContent"]["comments"][0]["id"] == "10500"' "$OUT"
REQ=$(last_request)
assert_json "get_comments clamps start_at -5 -> 0 and max_results 500 -> 100" 'r["query"]["startAt"] == "0" and r["query"]["maxResults"] == "100" and r["query"]["orderBy"] == "created"' "$REQ"
OUT=$(mcp_call get_comments '{"issue_key":"E2E-5","order_by":"updated; drop"}')
assert_contains "bad order_by rejected" 'order_by must be' "$OUT"
OUT=$(mcp_call get_comments '{"issue_key":"NOPE-1"}')
assert_contains "get_comments 404" 'Issue NOPE-1 does not exist' "$OUT"

echo "== get_transitions =="
OUT=$(mcp_call get_transitions '{"issue_key":"E2E-5"}')
assert_json "get_transitions lists 3 transitions" 'r["result"]["structuredContent"]["count"] == 3 and r["result"]["structuredContent"]["transitions"][1]["name"] == "Resolve" and r["result"]["structuredContent"]["transitions"][1]["to"]["name"] == "Done"' "$OUT"
assert_json "get_transitions without include_fields has no screen fields" '"requiredFields" not in r["result"]["structuredContent"]["transitions"][1]' "$OUT"
REQ=$(last_request)
assert_json "get_transitions sends no expand by default" '"expand" not in r["query"]' "$REQ"
OUT=$(mcp_call get_transitions '{"issue_key":"E2E-5","include_fields":true}')
assert_json "include_fields lists required screen fields" 'r["result"]["structuredContent"]["transitions"][1]["requiredFields"] == ["resolution"]' "$OUT"
assert_json "include_fields lists allowed values" '[f for f in r["result"]["structuredContent"]["transitions"][1]["screenFields"] if f["id"] == "resolution"][0]["allowedValues"][0]["name"] == "Done"' "$OUT"
REQ=$(last_request)
assert_json "include_fields sends expand=transitions.fields" 'r["query"]["expand"] == "transitions.fields"' "$REQ"
OUT=$(mcp_call get_transitions '{"issue_key":"NOPE-1"}')
assert_contains "get_transitions 404" 'Issue NOPE-1 does not exist' "$OUT"

echo "== transition_issue =="
OUT=$(mcp_call transition_issue '{"issue_key":"E2E-5","transition":"11"}')
assert_json "transition by id succeeds" 'r["result"]["structuredContent"]["ok"] is True and r["result"]["structuredContent"]["transition"]["name"] == "Start Progress"' "$OUT"
W=$(last_write)
assert_json "transition POSTs the id" 'r["path"] == "/issue/E2E-5/transitions" and r["body"] == {"transition":{"id":"11"}}' "$W"
OUT=$(mcp_call transition_issue '{"issue_key":"E2E-5","transition":"resolve","comment":"closing\nnow","fields":{"resolution":{"name":"Done"}}}')
assert_json "transition by name (case-insensitive) with fields + comment" 'r["result"]["structuredContent"]["transition"]["id"] == "21"' "$OUT"
W=$(last_write)
assert_json "transition sends screen fields" 'r["body"]["fields"] == {"resolution":{"name":"Done"}}' "$W"
assert_json "transition comment is ADF" 'r["body"]["update"]["comment"][0]["add"]["body"]["type"] == "doc"' "$W"
OUT=$(mcp_call transition_issue '{"issue_key":"E2E-5","transition":"in progress"}')
assert_json "transition by target status name" 'r["result"]["structuredContent"]["transition"]["id"] == "11"' "$OUT"
OUT=$(mcp_call transition_issue '{"issue_key":"E2E-5","transition":"Resolve"}')
assert_contains "transition screen 400 passes the field error" 'resolution: Resolution is required' "$OUT"
assert_contains "transition 400 hints at include_fields" 'include_fields=true' "$OUT"
OUT=$(mcp_call transition_issue '{"issue_key":"E2E-5","transition":"Nope"}')
assert_json "unknown transition lists the available ones" 'r["result"]["content"][0]["text"].startswith("No transition \"Nope\" is available on E2E-5")' "$OUT"
assert_contains "unknown transition names alternatives" '"Start Progress"' "$OUT"
OUT=$(mcp_call transition_issue '{"issue_key":"NOPE-1","transition":"Done"}')
assert_contains "transition on missing issue is 404" 'Issue NOPE-1 does not exist' "$OUT"
OUT=$(mcp_call transition_issue '{"issue_key":"E2E-5","transition":""}')
assert_contains "empty transition rejected" 'transition must be' "$OUT"

echo "== assign_issue =="
OUT=$(mcp_call assign_issue '{"issue_key":"E2E-5","account_id":"712020:8d5a4bc1-0f9e-4e3f-9a0e-1234567890ab"}')
assert_json "assign succeeds" 'r["result"]["structuredContent"]["ok"] is True and r["result"]["structuredContent"]["result"] == "assigned"' "$OUT"
W=$(last_write)
assert_json "assign PUTs the accountId" 'r["path"] == "/issue/E2E-5/assignee" and r["body"] == {"accountId":"712020:8d5a4bc1-0f9e-4e3f-9a0e-1234567890ab"}' "$W"
OUT=$(mcp_call assign_issue '{"issue_key":"E2E-5"}')
assert_json "no account_id unassigns" 'r["result"]["structuredContent"]["result"] == "unassigned"' "$OUT"
W=$(last_write)
assert_json "unassign sends accountId null" 'r["body"] == {"accountId": None}' "$W"
OUT=$(mcp_call assign_issue '{"issue_key":"E2E-5","account_id":"-1"}')
W=$(last_write)
assert_json "-1 sends the default-assignee sentinel" 'r["body"] == {"accountId":"-1"}' "$W"
OUT=$(mcp_call assign_issue '{"issue_key":"E2E-5","account_id":"bad-user"}')
assert_contains "assign 400 passes through" 'cannot be assigned issues' "$OUT"
assert_contains "assign 400 hints at assignable search" 'assignable_to_issue' "$OUT"
OUT=$(mcp_call assign_issue '{"issue_key":"E2E-5","account_id":"x\"; DROP"}')
assert_contains "malformed account_id rejected" 'not an Atlassian accountId' "$OUT"
OUT=$(mcp_call assign_issue '{"issue_key":"NOPE-1"}')
assert_contains "assign 404" 'Issue NOPE-1 does not exist' "$OUT"

echo "== list_projects =="
OUT=$(mcp_call list_projects '{}')
assert_json "list_projects returns 3 projects" 'r["result"]["structuredContent"]["count"] == 3 and r["result"]["structuredContent"]["projects"][0]["key"] == "E2E"' "$OUT"
assert_json "list_projects compacts the lead" 'r["result"]["structuredContent"]["projects"][0]["lead"]["displayName"] == "E2E Runner"' "$OUT"
assert_json "list_projects adds browse URLs" 'r["result"]["structuredContent"]["projects"][0]["url"] == "https://fixture.atlassian.net/browse/E2E"' "$OUT"
assert_json "list_projects is_last on a single page" 'r["result"]["structuredContent"]["is_last"] is True and r["result"]["structuredContent"]["next_start_at"] is None' "$OUT"
REQ=$(last_request)
assert_json "list_projects default query" 'r["query"] == {"startAt":"0","maxResults":"50","orderBy":"key","expand":"lead,description"}' "$REQ"
OUT=$(mcp_call list_projects '{"query":"ops","max_results":1000}')
assert_json "list_projects query filters" 'r["result"]["structuredContent"]["count"] == 1 and r["result"]["structuredContent"]["projects"][0]["key"] == "OPS"' "$OUT"
REQ=$(last_request)
assert_json "list_projects clamps max_results 1000 -> 100" 'r["query"]["maxResults"] == "100" and r["query"]["query"] == "ops"' "$REQ"
OUT=$(mcp_call list_projects '{"start_at":1,"max_results":1}')
assert_json "list_projects pagination" 'r["result"]["structuredContent"]["count"] == 1 and r["result"]["structuredContent"]["is_last"] is False and r["result"]["structuredContent"]["next_start_at"] == 2' "$OUT"
OUT=$(mcp_call list_projects '{"type_key":"service_desk"}')
assert_json "list_projects type_key filters" 'r["result"]["structuredContent"]["projects"][0]["key"] == "SD"' "$OUT"
OUT=$(mcp_call list_projects '{"type_key":"bogus"}')
assert_contains "bad type_key rejected" 'type_key must be' "$OUT"
OUT=$(mcp_call list_projects '{"query":"日本 a&b=c/d?e"}')
assert_json "unicode/reserved query characters are percent-encoded" '"query=%E6%97%A5%E6%9C%AC%20a%26b%3Dc%2Fd%3Fe" in r["rawQuery"]' "$(last_request)"
assert_json "and decode back upstream" 'r["query"]["query"] == "日本 a&b=c/d?e"' "$(last_request)"
OUT=$(mcp_call list_projects "{\"query\":\"$BIG\"}")
assert_contains "10 kB query rejected" 'at most 512 characters' "$OUT"

echo "== get_project =="
OUT=$(mcp_call get_project '{"project_key":"E2E"}')
assert_json "get_project returns issue types, components, versions" 'len(r["result"]["structuredContent"]["issueTypes"]) == 3 and r["result"]["structuredContent"]["components"][1]["name"] == "UI" and r["result"]["structuredContent"]["versions"][0]["released"] is True' "$OUT"
assert_json "get_project keeps style" 'r["result"]["structuredContent"]["style"] == "classic"' "$OUT"
REQ=$(last_request)
assert_json "get_project expands issueTypes,lead,description" 'r["path"] == "/project/E2E" and r["query"]["expand"] == "issueTypes,lead,description"' "$REQ"
OUT=$(mcp_call get_project '{"project_key":"NOPE"}')
assert_contains "get_project 404 hints at list_projects" 'Project NOPE was not found' "$OUT"
assert_contains "get_project 404 mentions case sensitivity" 'case-sensitive' "$OUT"
OUT=$(mcp_call get_project '{"project_key":"E2E/../x"}')
assert_contains "get_project traversal rejected" 'not a project key' "$OUT"
OUT=$(mcp_call get_project '{"project_key":"10000"}')
assert_json "get_project by numeric id" 'r["result"]["structuredContent"]["key"] == "E2E"' "$OUT"

echo "== list_issue_types =="
OUT=$(mcp_call list_issue_types '{"project_key":"E2E","max_results":1000}')
assert_json "list_issue_types returns 3 types" 'r["result"]["structuredContent"]["count"] == 3 and [t["name"] for t in r["result"]["structuredContent"]["issue_types"]] == ["Task","Bug","Sub-task"]' "$OUT"
assert_json "list_issue_types keeps subtask flag" 'r["result"]["structuredContent"]["issue_types"][2]["subtask"] is True' "$OUT"
REQ=$(last_request)
assert_json "list_issue_types uses per-project createmeta and clamps to 200" 'r["path"] == "/issue/createmeta/E2E/issuetypes" and r["query"]["maxResults"] == "200"' "$REQ"
OUT=$(mcp_call list_issue_types '{"project_key":"NOPE"}')
assert_contains "list_issue_types 404 mentions Create Issues permission" 'Create Issues permission' "$OUT"

echo "== get_create_fields =="
OUT=$(mcp_call get_create_fields '{"project_key":"E2E","issue_type":"task"}')
assert_json "get_create_fields resolves the type name" 'r["result"]["structuredContent"]["issue_type"] == {"id":"10001","name":"Task"}' "$OUT"
assert_json "get_create_fields lists required ids" 'r["result"]["structuredContent"]["required"] == ["summary","issuetype"]' "$OUT"
assert_json "get_create_fields lists allowed values" 'r["result"]["structuredContent"]["fields"][2]["allowedValues"][0]["name"] == "Highest"' "$OUT"
assert_json "get_create_fields keeps custom field schema" 'r["result"]["structuredContent"]["fields"][4]["custom"].endswith("jsw-story-points")' "$OUT"
REQ=$(last_request)
assert_json "get_create_fields queries the per-type createmeta" 'r["path"] == "/issue/createmeta/E2E/issuetypes/10001" and r["query"] == {"startAt":"0","maxResults":"200"}' "$REQ"
OUT=$(mcp_call get_create_fields '{"project_key":"E2E","issue_type":"10002"}')
assert_json "get_create_fields by numeric id" '"environment" in r["result"]["structuredContent"]["required"]' "$OUT"
OUT=$(mcp_call get_create_fields '{"project_key":"E2E","issue_type":"Epic"}')
assert_json "unknown issue type lists alternatives" 'r["result"]["content"][0]["text"] == "Issue type \"Epic\" is not available in project E2E. Available: Task, Bug, Sub-task."' "$OUT"
OUT=$(mcp_call get_create_fields '{"project_key":"NOPE","issue_type":"Task"}')
assert_contains "get_create_fields 404 on unknown project" 'Project NOPE was not found' "$OUT"

echo "== search_users =="
OUT=$(mcp_call search_users '{"query":"mia"}')
assert_json "search_users returns users with accountIds" 'r["result"]["structuredContent"]["count"] == 3 and r["result"]["structuredContent"]["users"][1]["accountId"] == "5b10a2844c20165700ede21g"' "$OUT"
assert_json "search_users scope all" 'r["result"]["structuredContent"]["scope"] == "all"' "$OUT"
REQ=$(last_request)
assert_json "search_users default path and page size" 'r["path"] == "/user/search" and r["query"] == {"query":"mia","maxResults":"20"}' "$REQ"
OUT=$(mcp_call search_users '{"query":"nobody"}')
assert_json "search_users empty result explains the permission trap" 'r["result"]["structuredContent"]["count"] == 0 and "Browse users and groups" in r["result"]["structuredContent"]["note"]' "$OUT"
OUT=$(mcp_call search_users '{"query":"bob","assignable_to_project":"E2E","max_results":500}')
REQ=$(last_request)
assert_json "assignable_to_project uses the assignable endpoint" 'r["path"] == "/user/assignable/search" and r["query"]["project"] == "E2E" and r["query"]["maxResults"] == "50"' "$REQ"
OUT=$(mcp_call search_users '{"query":"bob","assignable_to_issue":"E2E-1"}')
REQ=$(last_request)
assert_json "assignable_to_issue sends issueKey" 'r["query"]["issueKey"] == "E2E-1"' "$REQ"
OUT=$(mcp_call search_users '{"query":"bob","assignable_to_issue":"E2E-1","assignable_to_project":"E2E"}')
assert_contains "both assignable scopes rejected" 'at most one of' "$OUT"
OUT=$(mcp_call search_users '{"query":""}')
assert_contains "empty user query rejected" 'query must not be empty' "$OUT"
OUT=$(mcp_call search_users '{"query":"Müller & Co"}')
assert_json "user query is percent-encoded" '"query=M%C3%BCller%20%26%20Co" in r["rawQuery"]' "$(last_request)"
OUT=$(mcp_call search_users '{"query":"x","assignable_to_project":"../admin"}')
assert_contains "traversal in assignable_to_project rejected" 'not a project key' "$OUT"

echo "== server still healthy after the adversarial cases =="
OUT=$(mcp_call get_myself '{}')
assert_json "get_myself still works" 'r["result"]["structuredContent"]["accountId"] == "5b10ac8d82e05b22cc7d4ef5"' "$OUT"
if grep -qiE 'panicked|trap' "$E2E_TMP/server.log"; then
  fail "no panics or traps in the server log" "$(grep -iE 'panicked|trap' "$E2E_TMP/server.log" | head -3)"
else
  pass "no panics or traps in the server log"
fi

if [ "${E2E_LIVE:-0}" = "1" ]; then
  echo "== live smoke (E2E_LIVE=1, reads only) =="
  if [ -z "${ATLASSIAN_SITE:-}" ] || [ -z "${ATLASSIAN_EMAIL:-}" ] || [ -z "${ATLASSIAN_API_TOKEN:-}" ]; then
    fail "live smoke needs ATLASSIAN_SITE, ATLASSIAN_EMAIL and ATLASSIAN_API_TOKEN exported" ""
  else
    LIVE_ENV=(--env "ATLASSIAN_SITE=$ATLASSIAN_SITE" --env "ATLASSIAN_EMAIL=$ATLASSIAN_EMAIL"
              --env "ATLASSIAN_API_TOKEN=$ATLASSIAN_API_TOKEN")
    [ -n "${ATLASSIAN_CLOUD_ID:-}" ] && LIVE_ENV+=(--env "ATLASSIAN_CLOUD_ID=$ATLASSIAN_CLOUD_ID")
    "$WASMTIME" serve -Sp3,cli,http "${LIVE_ENV[@]}" --addr "127.0.0.1:${LIVE_PORT}" "$WASM" >"$E2E_TMP/live.log" 2>&1 &
    LIVE_PID=$!
    mcp_wait_ready "$LIVE_PORT"
    LIVE="http://127.0.0.1:${LIVE_PORT}/"
    OUT=$(mcp_call_on "$LIVE" check_auth '{}')
    assert_json "live: check_auth ok" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
    OUT=$(mcp_call_on "$LIVE" list_projects '{"max_results":1}')
    assert_json "live: list_projects returns a project" 'r["result"]["structuredContent"]["count"] >= 0' "$OUT"
    OUT=$(mcp_call_on "$LIVE" search_issues '{"jql":"assignee = currentUser() ORDER BY updated DESC","max_results":1}')
    assert_json "live: search_issues answers" '"issues" in r["result"]["structuredContent"]' "$OUT"
  fi
fi

guard_tests
mcp_harness_report
