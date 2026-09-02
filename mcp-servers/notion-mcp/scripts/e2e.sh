#!/usr/bin/env bash
# End-to-end tests for notion-mcp. Framework checks (protocol, spec enforcement,
# discovery route, skills over MCP, robustness, Host guard) come from the shared
# harness in ../../scripts/mcp_e2e_lib.sh; tool cases live below.
#
# Hermetic: scripts/fixture.py impersonates api.notion.com on FIXTURE_PORT and
# NOTION_BASE_URL points every instance at it. Four wasmtime instances run:
#   :PORT          token + default version (2026-03-11)   — most cases
#   :GUARD_PORT    no token                                — missing-secret path + Host guard
#   :READONLY_PORT token + NOTION_READ_ONLY=true           — gated writes
#   :LEGACY_PORT   token + NOTION_VERSION=2025-09-03       — field-name shim
#
# Usage: scripts/e2e.sh [--no-build]        E2E_LIVE=1 adds two read-only calls
#                                           against api.notion.com using $NOTION_TOKEN.
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9580}
GUARD_PORT=${GUARD_PORT:-9581}
FIXTURE_PORT=${FIXTURE_PORT:-9582}
READONLY_PORT=${READONLY_PORT:-9583}
LEGACY_PORT=${LEGACY_PORT:-9584}
WASM=${WASM:-target/wasm32-wasip2/release/notion_mcp.wasm}
SKILL_NAME=notion-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIXTURE_URL="http://127.0.0.1:${FIXTURE_PORT}"
READONLY_BASE="http://127.0.0.1:${READONLY_PORT}/"
LEGACY_BASE="http://127.0.0.1:${LEGACY_PORT}/"
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"
READONLY_PID=""
LEGACY_PID=""

cleanup_all() {
  [ -n "$READONLY_PID" ] && kill "$READONLY_PID" 2>/dev/null
  [ -n "$LEGACY_PID" ] && kill "$LEGACY_PID" 2>/dev/null
  mcp_harness_cleanup
}
trap cleanup_all EXIT

# Canned ids served by the fixture; the trailing digits of the 0xxx ids pick
# an upstream error status.
PAGE_ID="11111111-2222-3333-4444-555555555555"
PAGE_URL="https://www.notion.so/My-Roadmap-11111111222233334444555555555555?pvs=4"
DB_URL="https://www.notion.so/workspace/dbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdb?v=0123456789abcdef0123456789abcdef"
DS_ID="aaaaaaaabbbbccccddddeeeeeeeeeeee"
ID_401="00000000-0000-0000-0000-000000000401"
ID_403="00000000-0000-0000-0000-000000000403"
ID_404="00000000-0000-0000-0000-000000000404"
ID_409="00000000-0000-0000-0000-000000000409"
ID_429="00000000-0000-0000-0000-000000000429"
ID_500="00000000-0000-0000-0000-000000000500"

# The concurrency test in framework_tests fires this tool 8x in parallel.
FIRST_TOOL_NAME=get_self
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='Fixture Workspace'

mcp_build_if_needed "${1:-}"

echo "starting fixture on :${FIXTURE_PORT}..."
python3 scripts/fixture.py "$FIXTURE_PORT" >"$E2E_TMP/fixture.log" 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "${FIXTURE_URL}/_log" && break
  sleep 0.2
done

COMMON=(--env "NOTION_BASE_URL=${FIXTURE_URL}" --env RUST_LOG=info)
mcp_harness_start "${COMMON[@]}" --env NOTION_TOKEN=test-token --env NOTION_VERSION=2026-03-11
# Guard instance: no NOTION_TOKEN (missing-secret path).
mcp_harness_start_guard "${COMMON[@]}"

echo "starting read-only instance on :${READONLY_PORT}..."
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env NOTION_TOKEN=test-token --env NOTION_READ_ONLY=true \
  --addr "127.0.0.1:${READONLY_PORT}" "$WASM" >"$E2E_TMP/readonly.log" 2>&1 &
READONLY_PID=$!
mcp_wait_ready "$READONLY_PORT"

echo "starting legacy-version instance on :${LEGACY_PORT}..."
"$WASMTIME" serve -Sp3,cli,http "${COMMON[@]}" --env NOTION_TOKEN=test-token --env NOTION_VERSION=2025-09-03 \
  --addr "127.0.0.1:${LEGACY_PORT}" "$WASM" >"$E2E_TMP/legacy.log" 2>&1 &
LEGACY_PID=$!
mcp_wait_ready "$LEGACY_PORT"

ALL_TOOLS=(check_auth get_self search get_page get_page_content get_block_children create_page
  create_data_source_item update_page update_page_markdown append_blocks get_database
  get_data_source query_data_source list_comments create_comment list_users)

framework_tests "${ALL_TOOLS[@]}"
discovery_tests get_page_content
skills_tests "$SKILL_NAME" references/TOOLS.md references/ENDPOINTS.md references/MARKDOWN.md references/PROPERTIES.md

echo "== discovery: credentials block =="
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / lists the notion-mcp-token credential" '"ref": "notion-mcp-token"' "$ROOT"
assert_contains "GET / reports the token as configured (presence only)" '"status": "configured"' "$ROOT"
assert_not_contains "GET / never leaks the token value" 'test-token' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD_BASE")
assert_contains "GET / on the guard instance reports the token missing" '"status": "missing"' "$ROOT"

echo "== check_auth / get_self =="
OUT=$(mcp_call check_auth '{}')
assert_json "check_auth reports status ok" 'r["result"]["structuredContent"]["status"] == "ok"' "$OUT"
assert_json "check_auth names the workspace" 'r["result"]["structuredContent"]["identity"]["workspace_name"] == "Fixture Workspace"' "$OUT"
assert_json "check_auth carries ref/env/obtainUrl" 'r["result"]["structuredContent"]["ref"] == "notion-mcp-token" and r["result"]["structuredContent"]["env"] == "NOTION_TOKEN" and "notion.so/profile/integrations" in r["result"]["structuredContent"]["obtainUrl"]' "$OUT"
OUT=$(mcp_call get_self '{}')
assert_json "get_self returns the bot user" 'r["result"]["structuredContent"]["type"] == "bot" and r["result"]["structuredContent"]["name"] == "Fixture Bot"' "$OUT"
assert_json "get_self is not an error" 'r["result"].get("isError") is False' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" get_self '{}')
assert_contains "missing NOTION_TOKEN is an actionable tool error" 'NOTION_TOKEN is not set' "$OUT"
assert_contains "missing-token error names the secret ref" 'notion-mcp-token' "$OUT"
assert_contains "missing-token error names where to get the credential" 'notion.so/profile/integrations' "$OUT"
assert_contains "missing-token error is a tool error (isError)" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" check_auth '{}')
assert_json "check_auth on the guard instance reports status missing" 'r["result"]["structuredContent"]["status"] == "missing" and "cosmonic_set_secret" in r["result"]["structuredContent"]["remediation"]' "$OUT"
OUT=$(mcp_call get_page "{\"page_id\":\"$ID_401\"}")
assert_contains "401 maps to the re-register hint" 'Internal Integration Secret' "$OUT"
assert_contains "401 keeps the upstream message" 'API token is invalid' "$OUT"
assert_not_contains "401 error never echoes the token" 'test-token' "$OUT"

echo "== search =="
OUT=$(mcp_call search '{"query":"Roadmap","page_size":1000}')
assert_json "search clamps page_size 1000 -> 100" 'r["result"]["structuredContent"]["page_size"] == 100' "$OUT"
assert_json "search returns compact rows incl. a data_source" 'any(x["object"] == "data_source" and x["database_id"].startswith("dbdbdbdb") for x in r["result"]["structuredContent"]["results"])' "$OUT"
assert_json "search surfaces request_status when incomplete" 'r["result"]["structuredContent"]["request_status"]["incomplete_reason"] == "query_result_limit_reached"' "$OUT"
OUT=$(mcp_call search '{"page_size":0}')
assert_json "search clamps page_size 0 -> 1 and paginates" 'r["result"]["structuredContent"]["page_size"] == 1 and r["result"]["structuredContent"]["next_cursor"] == "cursor-page-2" and r["result"]["structuredContent"]["has_more"] is True' "$OUT"
OUT=$(mcp_call search '{"object":"database"}')
assert_contains "search rejects object=database with a data_source hint" 'tables are data sources' "$OUT"
assert_contains "search object=database refusal is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call search '{"object":"page","sort_direction":"sideways"}')
assert_contains "search rejects a bad sort_direction" 'sort_direction must be' "$OUT"
OUT=$(mcp_call search '{"query":"☃ snow \"; DROP TABLE pages; -- <script>alert(1)</script>","object":"page","sort_direction":"descending"}')
assert_json "search passes unicode/injection-shaped queries through as JSON" 'any("DROP TABLE pages" in x["title"] and "☃" in x["title"] for x in r["result"]["structuredContent"]["results"])' "$OUT"
OUT=$(mcp_call search "$(python3 -c 'import json; print(json.dumps({"query": "x" * 2001}))')")
assert_contains "search rejects a 2001-char query" 'longer than 2000' "$OUT"

echo "== get_page =="
OUT=$(mcp_call get_page "{\"page_id\":\"$PAGE_URL\",\"filter_properties\":[\"title\",\"s1\"]}")
assert_json "get_page normalizes a notion.so URL to the hyphenated id" 'r["result"]["structuredContent"]["id"] == "11111111-2222-3333-4444-555555555555"' "$OUT"
assert_json "get_page forwards filter_properties (repeated, encoded)" 'r["result"]["structuredContent"]["title"].endswith("fp=[\"title\", \"s1\"]")' "$OUT"
assert_json "get_page flattens select/date/relation/number/people/formula/rollup/unique_id" '(lambda p: p["Status"] == "In progress" and p["Due"]["start"] == "2026-09-30" and p["Project"] == ["66666666-7777-8888-9999-aaaaaaaaaaaa"] and p["Points"] == 3 and p["Owner"][0]["name"] == "Ada" and p["Estimate"] == 6 and p["Total"] == 9 and p["Ref"] == "TASK-42" and p["Tags"] == ["alpha", "beta ☃"])(r["result"]["structuredContent"]["properties"])' "$OUT"
OUT=$(mcp_call get_page "{\"page_id\":\"$PAGE_ID\",\"raw\":true}")
assert_json "get_page raw=true returns Notion property objects" 'r["result"]["structuredContent"]["properties"]["Status"]["type"] == "select"' "$OUT"
OUT=$(mcp_call get_page "{\"page_id\":\"$ID_404\"}")
assert_contains "404 maps to the not-shared hint" 'not shared with the integration' "$OUT"
assert_contains "404 hint mentions Connections" 'Connections' "$OUT"
OUT=$(mcp_call get_page "{\"page_id\":\"$ID_403\"}")
assert_contains "403 names the missing capability" 'Read content' "$OUT"
assert_contains "403 points at the Capabilities tab" 'Capabilities tab' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"../../etc/passwd"}')
assert_contains "get_page rejects a traversal-shaped id locally" 'is not a Notion id' "$OUT"
OUT=$(mcp_call get_page "$(python3 -c 'import json; print(json.dumps({"page_id": "a" * 5000}))')")
assert_contains "get_page rejects a 5000-char id" 'longer than 2048' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"https://www.notion.so/x/zz111111222233334444555555555555?v=1"}')
assert_contains "get_page rejects a URL whose tail is not hex" 'is not a Notion id' "$OUT"
OUT=$(mcp_call get_page '{"page_id":12345}')
assert_not_contains "get_page with an ill-typed page_id is an error, not a result" '"isError":false' "$OUT"
OUT=$(mcp_call get_page "$(python3 -c 'import json; print(json.dumps({"page_id": "11111111222233334444555555555555", "filter_properties": ["p%d" % i for i in range(101)]}))')")
assert_contains "get_page rejects 101 filter_properties" 'at most 100' "$OUT"

echo "== get_page_content =="
OUT=$(mcp_call get_page_content "{\"page_id\":\"$PAGE_ID\"}")
assert_contains "get_page_content forces Notion-Version 2026-03-11" 'Fixture page (Notion-Version 2026-03-11)' "$OUT"
assert_json "get_page_content returns the Markdown as the text block" 'r["result"]["content"][0]["text"].startswith("# Fixture page")' "$OUT"
assert_json "get_page_content reports upstream truncated/unknown_block_ids" 'r["result"]["structuredContent"]["truncated"] is False and r["result"]["structuredContent"]["unknown_block_ids"] == [] and r["result"]["structuredContent"]["local_truncated"] is False' "$OUT"
OUT=$(mcp_call get_page_content "{\"page_id\":\"$PAGE_ID\",\"max_chars\":1000,\"include_transcript\":true}")
assert_json "get_page_content max_chars truncates locally on a line boundary (multibyte-safe)" 'r["result"]["structuredContent"]["local_truncated"] is True and r["result"]["structuredContent"]["chars_returned"] <= 1000 and r["result"]["structuredContent"]["markdown"].endswith("☃")' "$OUT"
assert_contains "get_page_content marks local truncation" 'truncated locally at 1000' "$OUT"
OUT=$(mcp_call get_page_content "{\"page_id\":\"$PAGE_ID\",\"max_chars\":5}")
assert_json "get_page_content clamps max_chars up to 1000" 'r["result"]["structuredContent"]["chars_returned"] <= 1000 and r["result"]["structuredContent"]["chars_returned"] > 500' "$OUT"
OUT=$(mcp_call get_page_content "{\"page_id\":\"$ID_429\"}")
assert_contains "429 surfaces Retry-After" 'retry_after_seconds=7' "$OUT"
assert_contains "429 surfaces the rate-limit reason" 'integration_rate_limit' "$OUT"
OUT=$(mcp_call_on "$LEGACY_BASE" get_page_content "{\"page_id\":\"$PAGE_ID\"}")
assert_contains "legacy instance still forces 2026-03-11 for markdown reads" 'Notion-Version 2026-03-11' "$OUT"

echo "== get_block_children =="
OUT=$(mcp_call get_block_children "{\"block_id\":\"$PAGE_ID\",\"page_size\":999,\"start_cursor\":\"cur sor/1&x=☃\"}")
assert_json "get_block_children clamps page_size and encodes the cursor" '"page_size=100 cursor=cur sor/1&x=☃" in r["result"]["structuredContent"]["blocks"][0]["text"]' "$OUT"
assert_json "get_block_children summarizes to_do/code/child_page" '(lambda b: b[2]["checked"] is True and b[3]["language"] == "python" and b[4]["title"] == "Sub page" and b[4]["has_children"] is True)(r["result"]["structuredContent"]["blocks"])' "$OUT"
assert_json "get_block_children paginates" 'r["result"]["structuredContent"]["next_cursor"] == "blocks-cursor-2"' "$OUT"
OUT=$(mcp_call get_block_children "{\"block_id\":\"$PAGE_ID\",\"raw\":true,\"page_size\":-3}")
assert_json "get_block_children raw=true returns block objects, page_size -3 -> 1" 'r["result"]["structuredContent"]["blocks"][0]["object"] == "block" and r["result"]["structuredContent"]["page_size"] == 1' "$OUT"
OUT=$(mcp_call get_block_children "{\"block_id\":\"$ID_404\"}")
assert_contains "get_block_children 404 hint" 'A page id works as a block id' "$OUT"

echo "== create_page =="
OUT=$(mcp_call create_page "{\"parent_page_id\":\"$PAGE_URL\",\"title\":\"Hello ☃\",\"body_markdown\":\"# Body\\n\\nText\",\"icon_emoji\":\"🚀\"}")
assert_json "create_page under a page sets properties.title and sends markdown with 2026-03-11" 'r["result"]["structuredContent"]["title"] == "Hello ☃" and r["result"]["structuredContent"]["url"].endswith("created-2026-03-11-markdown=True") and r["result"]["structuredContent"]["parent"]["id"] == "11111111-2222-3333-4444-555555555555"' "$OUT"
OUT=$(mcp_call create_page "{\"parent_data_source_id\":\"$DS_ID\",\"title\":\"Ship it\",\"properties\":{\"Points\":{\"number\":8}}}")
assert_json "create_page under a data source resolves the title property via the schema" 'r["result"]["structuredContent"]["title"] == "Ship it" and r["result"]["structuredContent"]["parent"]["type"] == "data_source_id" and r["result"]["structuredContent"]["url"].endswith("markdown=False")' "$OUT"
OUT=$(mcp_call create_page "{\"parent_page_id\":\"$PAGE_ID\",\"parent_data_source_id\":\"$DS_ID\",\"title\":\"x\"}")
assert_contains "create_page rejects two parents" 'exactly one of parent_page_id or parent_data_source_id' "$OUT"
OUT=$(mcp_call create_page '{"title":"orphan"}')
assert_contains "create_page rejects no parent" 'exactly one of parent_page_id' "$OUT"
OUT=$(mcp_call create_page "{\"parent_page_id\":\"$PAGE_ID\",\"title\":\"x\",\"icon_emoji\":\"this is far too long for an emoji\"}")
assert_contains "create_page rejects an oversized icon_emoji" 'single emoji' "$OUT"
OUT=$(mcp_call create_page "{\"parent_data_source_id\":\"$ID_404\",\"title\":\"x\"}")
assert_contains "create_page with a bad data source id gets the get_database hint" 'call get_database' "$OUT"

echo "== create_data_source_item =="
ARGS=$(python3 -c '
import json
print(json.dumps({
  "data_source_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
  "title": "Coerced row",
  "properties": {
    "status": "Done",
    "Tags": ["alpha", "beta ☃"],
    "Due": "2026-09-02",
    "Points": "7.5",
    "Done": True,
    "Project": ["https://www.notion.so/Roadmap-66666666777788889999aaaaaaaaaaaa"],
    "Owner": ["0123456789abcdef0123456789abcdef"],
    "Link": "https://example.com/?q=1&r=2",
    "Notes": "n" * 2500
  }
}))')
OUT=$(mcp_call create_data_source_item "$ARGS")
assert_json "create_data_source_item coerces plain values via the schema" '(lambda p: p["Status"] == "Done" and p["Tags"] == ["alpha", "beta ☃"] and p["Due"]["start"] == "2026-09-02" and p["Points"] == 7.5 and p["Done"] is True and p["Project"] == ["66666666-7777-8888-9999-aaaaaaaaaaaa"] and p["Owner"][0]["id"] == "01234567-89ab-cdef-0123-456789abcdef" and p["Link"].endswith("r=2"))(r["result"]["structuredContent"]["properties"])' "$OUT"
assert_json "create_data_source_item splits 2500-char rich_text into 2 runs and sets the title" 'json.loads(r["result"]["structuredContent"]["properties"]["_runs"]) == {"Notes": 2} and r["result"]["structuredContent"]["title"] == "Coerced row"' "$OUT"
OUT=$(mcp_call create_data_source_item "{\"data_source_id\":\"$DS_ID\",\"properties\":{\"Attachments\":\"x\"}}")
assert_contains "create_data_source_item rejects a files property by type name" 'type `files`' "$OUT"
OUT=$(mcp_call create_data_source_item "{\"data_source_id\":\"$DS_ID\",\"properties\":{\"Nope\":\"x\"}}")
assert_contains "create_data_source_item names available properties on a miss" 'is not a property of this data source; available properties: Attachments' "$OUT"
OUT=$(mcp_call create_data_source_item "{\"data_source_id\":\"$DS_ID\",\"properties\":{\"Points\":\"lots\"}}")
assert_contains "create_data_source_item rejects a non-numeric number" 'expects a number' "$OUT"
OUT=$(mcp_call create_data_source_item "{\"data_source_id\":\"$DS_ID\"}")
assert_contains "create_data_source_item needs a title or a property" 'pass a title and/or at least one property' "$OUT"

echo "== update_page =="
OUT=$(mcp_call update_page "{\"page_id\":\"$PAGE_ID\",\"in_trash\":true,\"icon_emoji\":\"✅\",\"properties\":{\"Points\":{\"number\":1}}}")
assert_json "update_page sends in_trash under 2026-03-11" 'r["result"]["structuredContent"]["title"] == "fields:icon,in_trash,properties" and r["result"]["structuredContent"]["in_trash"] is True' "$OUT"
OUT=$(mcp_call_on "$LEGACY_BASE" update_page "{\"page_id\":\"$PAGE_ID\",\"in_trash\":true}")
assert_json "update_page sends archived under 2025-09-03" 'r["result"]["structuredContent"]["title"] == "fields:archived" and r["result"]["structuredContent"]["in_trash"] is True' "$OUT"
OUT=$(mcp_call update_page "{\"page_id\":\"$PAGE_ID\"}")
assert_contains "update_page with nothing to do is an error" 'nothing to update' "$OUT"
OUT=$(mcp_call update_page "{\"page_id\":\"$ID_409\",\"in_trash\":false}")
assert_contains "409 maps to a retry-once hint" 'retry the same call once' "$OUT"

echo "== update_page_markdown =="
OUT=$(mcp_call update_page_markdown "{\"page_id\":\"$PAGE_ID\",\"mode\":\"update\",\"updates\":[{\"old_str\":\"foo ☃\",\"new_str\":\"bar\",\"replace_all\":true},{\"old_str\":\"x\",\"new_str\":\"\"}]}")
assert_json "update mode sends update_content with replace_all_matches" '"\"update_content\": {\"allow_deleting_content\": false, \"content_updates\": [{\"new_str\": \"bar\", \"old_str\": \"foo ☃\", \"replace_all_matches\": true}, {\"new_str\": \"\", \"old_str\": \"x\", \"replace_all_matches\": false}]}" in r["result"]["structuredContent"]["markdown"]' "$OUT"
assert_json "update mode surfaces unknown_block_ids" 'r["result"]["structuredContent"]["unknown_block_ids"] == ["deadbeef-0000-0000-0000-000000000001"]' "$OUT"
OUT=$(mcp_call update_page_markdown "{\"page_id\":\"$PAGE_ID\",\"mode\":\"replace\",\"new_markdown\":\"# New\\n\\nbody\",\"allow_deleting_content\":true}")
assert_json "replace mode sends replace_content with allow_deleting_content" '"\"replace_content\": {\"allow_deleting_content\": true, \"new_str\": \"# New\\n\\nbody\"}" in r["result"]["structuredContent"]["markdown"] and "\"type\": \"replace_content\"" in r["result"]["structuredContent"]["markdown"]' "$OUT"
OUT=$(mcp_call update_page_markdown "{\"page_id\":\"$PAGE_ID\",\"mode\":\"update\",\"updates\":[{\"old_str\":\"DUPLICATE\",\"new_str\":\"y\"}]}")
assert_contains "duplicate old_str maps to the make-it-unique hint" 'set replace_all=true' "$OUT"
OUT=$(mcp_call update_page_markdown "{\"page_id\":\"$PAGE_ID\",\"mode\":\"delete\"}")
assert_contains "bad mode is rejected" 'mode must be' "$OUT"
OUT=$(mcp_call update_page_markdown "{\"page_id\":\"$PAGE_ID\",\"mode\":\"update\"}")
assert_contains "update mode without updates is rejected" 'needs 1..50 entries' "$OUT"
OUT=$(mcp_call update_page_markdown "$(python3 -c 'import json; print(json.dumps({"page_id": "11111111222233334444555555555555", "mode": "update", "updates": [{"old_str": "a", "new_str": "b"}] * 51}))')")
assert_contains "51 updates are rejected locally" 'got 51' "$OUT"
OUT=$(mcp_call update_page_markdown "{\"page_id\":\"$PAGE_ID\",\"mode\":\"update\",\"updates\":[{\"old_str\":\"\",\"new_str\":\"b\"}]}")
assert_contains "empty old_str is rejected locally" 'old_str is empty' "$OUT"
OUT=$(mcp_call update_page_markdown "{\"page_id\":\"$PAGE_ID\",\"mode\":\"replace\"}")
assert_contains "replace mode without new_markdown is rejected" 'needs `new_markdown`' "$OUT"

echo "== append_blocks =="
ARGS=$(python3 -c '
import json
md = "# Title\n\nPara one\nstill one\n\n- bullet\n* star bullet\n1. first\n2) second\n- [ ] open\n- [x] done\n```python\nprint(1)\n```\n> quoted\n> more\n---\n" + ("z" * 2001)
print(json.dumps({"block_id": "11111111-2222-3333-4444-555555555555", "markdown": md}))')
OUT=$(mcp_call append_blocks "$ARGS")
assert_json "append_blocks converts the markdown subset to the expected block types" '[b["type"] for b in r["result"]["structuredContent"]["blocks"][:11]] == ["heading_1", "paragraph", "bulleted_list_item", "bulleted_list_item", "numbered_list_item", "numbered_list_item", "to_do", "to_do", "code", "quote", "divider"]' "$OUT"
assert_json "append_blocks splits a 2001-char paragraph into 2 rich_text runs" 'any(b["type"] == "runs:1,1,1,1,1,1,1,1,1,1,0,2" for b in r["result"]["structuredContent"]["blocks"])' "$OUT"
assert_json "append_blocks defaults to position end under 2026-03-11" 'any(b["type"] == "position:{\"type\": \"end\"}" for b in r["result"]["structuredContent"]["blocks"])' "$OUT"
assert_json "append_blocks reports sent/appended counts" 'r["result"]["structuredContent"]["sent"] == 12' "$OUT"
OUT=$(mcp_call append_blocks "{\"block_id\":\"$PAGE_ID\",\"markdown\":\"hi\",\"after_block_id\":\"66666666777788889999aaaaaaaaaaaa\"}")
assert_json "append_blocks after_block_id sends the position object" 'any(b["type"] == "position:{\"after_block\": {\"id\": \"66666666-7777-8888-9999-aaaaaaaaaaaa\"}, \"type\": \"after_block\"}" for b in r["result"]["structuredContent"]["blocks"])' "$OUT"
OUT=$(mcp_call append_blocks "{\"block_id\":\"$PAGE_ID\",\"markdown\":\"hi\",\"position\":\"start\"}")
assert_json "append_blocks position=start" 'any(b["type"] == "position:{\"type\": \"start\"}" for b in r["result"]["structuredContent"]["blocks"])' "$OUT"
OUT=$(mcp_call_on "$LEGACY_BASE" append_blocks "{\"block_id\":\"$PAGE_ID\",\"markdown\":\"hi\",\"after_block_id\":\"$PAGE_ID\"}")
assert_json "append_blocks sends after=<id> under 2025-09-03" 'any(b["type"] == "position:{\"after\": \"11111111-2222-3333-4444-555555555555\"}" for b in r["result"]["structuredContent"]["blocks"])' "$OUT"
OUT=$(mcp_call_on "$LEGACY_BASE" append_blocks "{\"block_id\":\"$PAGE_ID\",\"markdown\":\"hi\",\"position\":\"start\"}")
assert_contains "append_blocks position=start is refused under 2025-09-03" 'needs Notion-Version 2026-03-11' "$OUT"
OUT=$(mcp_call append_blocks "{\"block_id\":\"$PAGE_ID\",\"markdown\":\"hi\",\"position\":\"start\",\"after_block_id\":\"$PAGE_ID\"}")
assert_contains "append_blocks rejects start + after_block_id" 'cannot be combined' "$OUT"
OUT=$(mcp_call append_blocks "$(python3 -c 'import json; print(json.dumps({"block_id": "11111111222233334444555555555555", "markdown": "\n".join("- item %d" % i for i in range(101))}))')")
assert_contains "append_blocks rejects 101 blocks before calling upstream" 'converts to 101 blocks' "$OUT"
OUT=$(mcp_call append_blocks "{\"block_id\":\"$PAGE_ID\",\"markdown\":\"   \\n\\n\"}")
assert_contains "append_blocks rejects empty markdown" 'markdown is empty' "$OUT"
OUT=$(mcp_call append_blocks "{\"block_id\":\"$PAGE_ID\",\"markdown\":\"hi\",\"position\":\"middle\"}")
assert_contains "append_blocks rejects an unknown position" 'position must be' "$OUT"

echo "== get_database / get_data_source / query_data_source =="
OUT=$(mcp_call get_database "{\"database_id\":\"$DB_URL\"}")
assert_json "get_database takes a database URL (ignoring ?v=) and lists data sources" 'r["result"]["structuredContent"]["id"] == "dbdbdbdb-dbdb-dbdb-dbdb-dbdbdbdbdbdb" and [d["name"] for d in r["result"]["structuredContent"]["data_sources"]] == ["Tasks", "Archive"] and r["result"]["structuredContent"]["title"] == "Tasks DB"' "$OUT"
OUT=$(mcp_call get_database "{\"database_id\":\"$ID_404\"}")
assert_contains "get_database 404 hint mentions get_data_source" 'use get_data_source or get_page' "$OUT"
OUT=$(mcp_call get_data_source "{\"data_source_id\":\"$DS_ID\"}")
assert_json "get_data_source summarizes the schema with options and title_property" 'r["result"]["structuredContent"]["title_property"] == "Task" and r["result"]["structuredContent"]["database_id"].startswith("dbdbdbdb") and any(p["name"] == "Status" and p["options"] == ["Todo", "In progress", "Done"] for p in r["result"]["structuredContent"]["properties"]) and any(p["name"] == "Project" and p["data_source_id"] for p in r["result"]["structuredContent"]["properties"]) and any(p["name"] == "Estimate" and "prop(" in p["expression"] for p in r["result"]["structuredContent"]["properties"])' "$OUT"
OUT=$(mcp_call get_data_source "{\"data_source_id\":\"$DS_ID\",\"raw\":true}")
assert_json "get_data_source raw=true returns the data source object" 'r["result"]["structuredContent"]["object"] == "data_source"' "$OUT"
OUT=$(mcp_call query_data_source "{\"data_source_id\":\"$DS_ID\",\"page_size\":500,\"filter\":{\"property\":\"Status\",\"select\":{\"equals\":\"Done ☃\"}},\"sorts\":[{\"property\":\"Due\",\"direction\":\"ascending\"}],\"start_cursor\":\"c1\",\"include_trashed\":true,\"filter_properties\":[\"s1\",\"d1\"]}")
assert_json "query_data_source clamps page_size 500 -> 100" 'r["result"]["structuredContent"]["page_size"] == 100 and r["result"]["structuredContent"]["rows"][0]["title"] == "page_size=100"' "$OUT"
assert_json "query_data_source passes filter/sorts/cursor/is_archived/filter_properties through" 'r["result"]["structuredContent"]["rows"][1]["title"] == "filter={\"property\": \"Status\", \"select\": {\"equals\": \"Done ☃\"}} sorts=[{\"direction\": \"ascending\", \"property\": \"Due\"}] cursor=c1 archived=True fp=[\"s1\", \"d1\"]"' "$OUT"
assert_json "query_data_source flattens rows and paginates" 'r["result"]["structuredContent"]["rows"][0]["properties"]["Status"] == "In progress" and r["result"]["structuredContent"]["next_cursor"] == "rows-cursor-2"' "$OUT"
OUT=$(mcp_call query_data_source "{\"data_source_id\":\"$DS_ID\",\"filter\":\"Status = Done\"}")
assert_contains "query_data_source rejects a non-object filter" 'filter must be a Notion filter object' "$OUT"
OUT=$(mcp_call query_data_source "{\"data_source_id\":\"$DS_ID\",\"sorts\":[\"Due\"]}")
assert_contains "query_data_source rejects non-object sorts" 'sorts must be objects' "$OUT"
OUT=$(mcp_call query_data_source "{\"data_source_id\":\"$ID_500\"}")
assert_contains "500 maps to a transient/backoff hint" 'Transient Notion-side failure' "$OUT"
OUT=$(mcp_call query_data_source "{\"data_source_id\":\"$ID_409\"}")
assert_contains "409 on query maps to retry-once" 'retry the same call once' "$OUT"
OUT=$(mcp_call query_data_source "{\"data_source_id\":\"$ID_404\"}")
assert_contains "query 404 hint points at get_database" 'call get_database' "$OUT"

echo "== comments =="
OUT=$(mcp_call list_comments "{\"block_id\":\"$PAGE_URL\",\"page_size\":250}")
assert_json "list_comments passes the normalized block_id and clamps page_size" 'r["result"]["structuredContent"]["comments"][0]["text"] == "block_id=11111111-2222-3333-4444-555555555555 page_size=100"' "$OUT"
assert_json "list_comments summarizes author/display_name/attachments" '(lambda c: c[0]["created_by"]["name"] == "Ada" and c[1]["display_name"] == "Fixture Bot" and c[1]["attachments"] == 1 and c[1]["text"] == "Reply ☃")(r["result"]["structuredContent"]["comments"])' "$OUT"
OUT=$(mcp_call list_comments "{\"block_id\":\"$ID_403\"}")
assert_contains "list_comments 403 names the Read comments capability" 'Read comments' "$OUT"
OUT=$(mcp_call create_comment "{\"text\":\"Looks good ☃\",\"page_id\":\"$PAGE_ID\"}")
assert_json "create_comment on a page returns id/discussion_id" 'r["result"]["structuredContent"]["id"].endswith("new1") and r["result"]["structuredContent"]["parent"]["type"] == "page_id"' "$OUT"
OUT=$(mcp_call create_comment "{\"text\":\"reply\",\"discussion_id\":\"d1d1d1d1000000000000000000000001\"}")
assert_json "create_comment replies in a discussion" 'r["result"]["structuredContent"]["discussion_id"] == "d1d1d1d1-0000-0000-0000-000000000001"' "$OUT"
OUT=$(mcp_call create_comment "{\"text\":\"x\",\"page_id\":\"$PAGE_ID\",\"discussion_id\":\"d1d1d1d1000000000000000000000001\"}")
assert_contains "create_comment rejects two targets" 'exactly one of page_id, block_id, or discussion_id' "$OUT"
OUT=$(mcp_call create_comment '{"text":"x"}')
assert_contains "create_comment rejects no target" 'exactly one of page_id' "$OUT"
OUT=$(mcp_call create_comment "{\"text\":\"   \",\"page_id\":\"$PAGE_ID\"}")
assert_contains "create_comment rejects blank text" 'text must be 1..200000' "$OUT"
OUT=$(mcp_call create_comment "$(python3 -c 'import json; print(json.dumps({"text": "t" * 200001, "page_id": "11111111222233334444555555555555"}))')")
assert_contains "create_comment rejects 200001 chars" 'got 200001' "$OUT"
OUT=$(mcp_call create_comment "$(python3 -c 'import json; print(json.dumps({"text": "t" * 4001, "block_id": "11111111222233334444555555555555"}))')")
assert_json "create_comment splits 4001 chars into 3 runs on a block" 'r["result"]["structuredContent"]["parent"]["type"] == "block_id"' "$OUT"
OUT=$(mcp_call create_comment "{\"text\":\"x\",\"block_id\":\"$ID_403\"}")
assert_contains "create_comment 403 names the Insert comments capability" 'Insert comments' "$OUT"

echo "== list_users =="
OUT=$(mcp_call list_users '{"page_size":1000}')
assert_json "list_users clamps page_size and summarizes people/bots" 'r["result"]["structuredContent"]["users"][0]["name"] == "page_size=100" and r["result"]["structuredContent"]["users"][0]["email"] == "ada@example.com" and r["result"]["structuredContent"]["users"][2]["workspace_name"] == "Fixture Workspace" and "email" not in r["result"]["structuredContent"]["users"][1]' "$OUT"
OUT=$(mcp_call list_users '{"page_size":"ten"}')
assert_not_contains "list_users with an ill-typed page_size is an error" '"isError":false' "$OUT"

echo "== payload cap (500 KB, checked locally) =="
curl -sS -o /dev/null "${FIXTURE_URL}/_reset"
# 200,000 snowmen are under the 400,000-char body limit but 600 KB of UTF-8
# on the wire, so the serialized request trips Notion's 500 KB cap locally.
OUT=$(mcp_call create_page "$(python3 -c 'import json; print(json.dumps({"parent_page_id": "11111111222233334444555555555555", "title": "big", "body_markdown": "☃" * 200000}))')")
assert_contains "create_page refuses a >500 KB multibyte payload before calling upstream" 'Notion accepts at most 512000 bytes' "$OUT"
assert_contains "payload-cap refusal is labelled as not sent" 'Request not sent (payload_too_large)' "$OUT"
assert_not_contains "payload-cap refusal is not explained as a network failure" 'allowedHosts' "$OUT"
LOGGED=$(curl -sS "${FIXTURE_URL}/_log")
assert_contains "payload-cap refusal never reached the upstream" '[]' "$LOGGED"

echo "== read-only deployment (NOTION_READ_ONLY=true) =="
curl -sS -o /dev/null "${FIXTURE_URL}/_reset"
for tool_args in \
  "create_page {\"parent_page_id\":\"$PAGE_ID\",\"title\":\"x\"}" \
  "create_data_source_item {\"data_source_id\":\"$DS_ID\",\"title\":\"x\"}" \
  "update_page {\"page_id\":\"$PAGE_ID\",\"in_trash\":true}" \
  "update_page_markdown {\"page_id\":\"$PAGE_ID\",\"mode\":\"replace\",\"new_markdown\":\"x\"}" \
  "append_blocks {\"block_id\":\"$PAGE_ID\",\"markdown\":\"x\"}" \
  "create_comment {\"text\":\"x\",\"page_id\":\"$PAGE_ID\"}"; do
  tool=${tool_args%% *}
  args=${tool_args#* }
  OUT=$(mcp_call_on "$READONLY_BASE" "$tool" "$args")
  assert_contains "read-only refuses $tool" 'deployed read-only (NOTION_READ_ONLY=true)' "$OUT"
done
LOGGED=$(curl -sS "${FIXTURE_URL}/_log")
assert_contains "read-only refusals never reached the upstream" '[]' "$LOGGED"
OUT=$(mcp_call_on "$READONLY_BASE" get_page "{\"page_id\":\"$PAGE_ID\"}")
assert_json "read-only instance still serves reads" 'r["result"]["structuredContent"]["title"].startswith("Roadmap")' "$OUT"

if [ "${E2E_LIVE:-0}" = "1" ] && [ -n "${NOTION_TOKEN:-}" ]; then
  echo "== live (api.notion.com, read-only) =="
  LIVE_PORT=$((PORT + 5))
  "$WASMTIME" serve -Sp3,cli,http --env "NOTION_TOKEN=${NOTION_TOKEN}" --addr "127.0.0.1:${LIVE_PORT}" "$WASM" >"$E2E_TMP/live.log" 2>&1 &
  LIVE_PID=$!
  mcp_wait_ready "$LIVE_PORT"
  OUT=$(mcp_call_on "http://127.0.0.1:${LIVE_PORT}/" get_self '{}')
  assert_json "live get_self returns a bot user" 'r["result"]["structuredContent"]["type"] == "bot"' "$OUT"
  OUT=$(mcp_call_on "http://127.0.0.1:${LIVE_PORT}/" search '{"page_size":3}')
  assert_json "live search answers" '"results" in r["result"]["structuredContent"]' "$OUT"
  kill "$LIVE_PID" 2>/dev/null
fi

guard_tests
mcp_harness_report
