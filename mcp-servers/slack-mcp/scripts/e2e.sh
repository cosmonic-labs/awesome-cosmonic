#!/usr/bin/env bash
# End-to-end tests for slack-mcp. Framework checks (protocol, spec enforcement,
# discovery route, skills over MCP, robustness, Host guard) come from the shared
# harness in ../../scripts/mcp_e2e_lib.sh; tool cases live below.
#
# The suite is hermetic: a threaded Python fixture impersonates
# https://slack.com/api/<method> (auth checks, scripted error ids, HTTP 429 with
# Retry-After, a 503, echo of every request under /__last/<method>) and the
# component is pointed at it with SLACK_BASE_URL. Five wasmtime instances run:
#   primary   — token + SLACK_TEAM_ID (most cases)
#   guard     — no secret (missing-secret path + Host guard)
#   fenced    — SLACK_CHANNEL_IDS=C0FENCE01,C0FENCE02 (list shortcut + write fence)
#   read-only — SLACK_READ_ONLY=true with a token from another workspace
#               (write refusal + SLACK_TEAM_ID mismatch)
#   bad-token — a token Slack rejects (invalid-credential path)
# E2E_LIVE=1 with a real SLACK_BOT_TOKEN (and optional SLACK_TEAM_ID) in the
# environment adds two read-only calls against slack.com.
#
# Usage: scripts/e2e.sh [--no-build]
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9172}
GUARD_PORT=${GUARD_PORT:-9173}
FIXTURE_PORT=${FIXTURE_PORT:-9174}
FENCE_PORT=${FENCE_PORT:-$((PORT + 3))}
RO_PORT=${RO_PORT:-$((PORT + 4))}
BAD_PORT=${BAD_PORT:-$((PORT + 5))}
LIVE_PORT=${LIVE_PORT:-$((PORT + 6))}
WASM=${WASM:-target/wasm32-wasip2/release/slack_mcp.wasm}
SKILL_NAME=slack-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIXTURE="http://127.0.0.1:${FIXTURE_PORT}"
FENCE_BASE="http://127.0.0.1:${FENCE_PORT}/"
RO_BASE="http://127.0.0.1:${RO_PORT}/"
BAD_BASE="http://127.0.0.1:${BAD_PORT}/"
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"

EXTRA_PIDS=()
cleanup_all() {
  for pid in ${EXTRA_PIDS[@]+"${EXTRA_PIDS[@]}"}; do kill "$pid" 2>/dev/null; done
  mcp_harness_cleanup
}
trap cleanup_all EXIT

# start_instance <port> <logname> [wasmtime args...]
start_instance() {
  local port="$1" name="$2"
  shift 2
  echo "starting $name instance on :${port}..."
  "$WASMTIME" serve -Sp3,cli,http "$@" --addr "127.0.0.1:${port}" "$WASM" \
    >"$E2E_TMP/$name.log" 2>&1 &
  EXTRA_PIDS+=($!)
  mcp_wait_ready "$port"
}

# fx_last <method> — what the fixture last saw for /api/<method>.
fx_last() { curl -sS --max-time 10 "${FIXTURE}/__last/$1"; }
# fx_count — total /api hits so far.
fx_count() { curl -sS --max-time 10 "${FIXTURE}/__count"; }

# The concurrency test in framework_tests fires this tool 8x in parallel —
# an outbound tool, so concurrent outbound (inter-task-wakeup) is exercised.
FIRST_TOOL_NAME=check_auth
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"status":"ok"'

mcp_build_if_needed "${1:-}"

# ---------------------------------------------------------------------------
# Slack fixture. Threaded (the concurrency test fires 8 requests at once).
# ---------------------------------------------------------------------------
python3 - "$FIXTURE_PORT" <<'EOF' >"$E2E_TMP/fixture.log" 2>&1 &
import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

LOCK = threading.Lock()
STATE = {"count": 0, "last": {}}
VALID = {"xoxb-e2e-token": "T0E2E", "xoxb-otherteam": "T0OTHER"}
SCOPES = "channels:read,channels:history,chat:write,reactions:write,users:read,users.profile:read,channels:join"
LONG_TEXT = "x" * 5000

def channel(cid, name, **kw):
    o = {"id": cid, "name": name, "is_channel": True, "is_private": False,
         "is_archived": False, "is_general": name == "general", "is_member": True,
         "num_members": 42, "created": 1700000000,
         "topic": {"value": "Topic of " + name}, "purpose": {"value": "Purpose of " + name}}
    o.update(kw)
    return o

CHANNELS = {c["id"]: c for c in [
    channel("C0GENERAL", "general"),
    channel("C0RANDOM1", "random", is_member=False, num_members=7),
    channel("C0FENCE01", "fenced-one"),
    channel("C0FENCE02", "fenced-archived", is_archived=True),
    channel("C0OTHER01", "other"),
    channel("C0ALREADY", "already"),
    channel("C0NOTIN01", "not-in", is_member=False),
    channel("C0ARCHIV1", "archived", is_archived=True),
    channel("C0PRIV001", "private", is_private=True),
    channel("C0SCOPE01", "scope"),
    channel("C0RATE001", "rate"),
]}
PARENT = {"type": "message", "user": "U0BOB0001", "text": "thread parent",
          "ts": "1712345678.000200", "thread_ts": "1712345678.000200",
          "reply_count": 2, "reply_users_count": 1, "latest_reply": "1712345678.000250"}
HISTORY = [
    {"type": "message", "user": "U0ALICE01",
     "text": "newest ✨ message with <@U0BOB0001> and &lt;tags&gt; — 日本語",
     "ts": "1712345678.000300",
     "reactions": [{"name": "thumbsup", "count": 2, "users": ["U0ALICE01", "U0BOB0001"]}]},
    PARENT,
    {"type": "message", "bot_id": "B0BOT0001", "subtype": "bot_message", "username": "deploybot",
     "text": LONG_TEXT, "ts": "1712345678.000100", "attachments": [{"text": "a"}],
     "edited": {"user": "B0BOT0001", "ts": "1712345678.000150"}},
]
HISTORY_PAGE2 = [{"type": "message", "user": "U0ALICE01", "text": "older", "ts": "1712345600.000100"}]
REPLIES = [PARENT,
           {"type": "message", "user": "U0ALICE01", "text": "reply one", "ts": "1712345678.000210",
            "thread_ts": "1712345678.000200", "parent_user_id": "U0BOB0001"},
           {"type": "message", "user": "U0ALICE01", "text": "reply two 🎉", "ts": "1712345678.000250",
            "thread_ts": "1712345678.000200", "parent_user_id": "U0BOB0001"}]
USERS = [
    {"id": "U0ALICE01", "name": "alice", "real_name": "Alice Liddell", "is_bot": False, "is_admin": True,
     "deleted": False, "tz": "Europe/London",
     "profile": {"display_name": "alice", "title": "Engineer", "real_name": "Alice Liddell"}},
    {"id": "U0BOB0001", "name": "bob", "real_name": "Bob Builder", "is_bot": False, "deleted": True,
     "tz": "America/New_York", "profile": {"display_name": "bob", "title": ""}},
]
USERS_PAGE2 = [{"id": "U0CAROL01", "name": "carol", "real_name": "Carol", "is_bot": True, "deleted": False,
                "profile": {"display_name": "carol-bot", "title": "bot"}}]
PROFILE = {"real_name": "Alice Liddell", "display_name": "alice", "first_name": "Alice", "last_name": "Liddell",
           "title": "Engineer", "status_text": "🚀 déployé", "status_emoji": ":rocket:", "status_expiration": 0,
           "pronouns": "she/her", "email": "alice@example.com", "phone": "", "tz": "Europe/London",
           "image_192": "https://example.com/a192.png", "image_512": "https://example.com/a512.png"}

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, status, obj, headers=None, raw=None, ctype="application/json; charset=utf-8"):
        body = raw if raw is not None else json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def read_body(self):
        # wasmtime's outbound client streams POST bodies with chunked
        # transfer-encoding; a real Slack endpoint handles both forms.
        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            data = b""
            while True:
                line = self.rfile.readline().strip()
                if not line:
                    break
                size = int(line.split(b";")[0], 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    break
                data += self.rfile.read(size)
                self.rfile.readline()
            return data
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def do_GET(self):
        self.handle_any("GET")

    def do_POST(self):
        self.handle_any("POST")

    def handle_any(self, verb):
        u = urlparse(self.path)
        if u.path == "/__count":
            with LOCK:
                return self.send(200, {"count": STATE["count"]})
        if u.path == "/__reset":
            with LOCK:
                STATE["count"] = 0
                STATE["last"] = {}
            return self.send(200, {"ok": True})
        if u.path.startswith("/__last/"):
            with LOCK:
                return self.send(200, STATE["last"].get(u.path[8:], {}))
        if not u.path.startswith("/api/"):
            return self.send(404, {"ok": False, "error": "not_found"})
        method = u.path[5:]
        query = {k: v[0] for k, v in parse_qs(u.query, keep_blank_values=True).items()}
        raw = self.read_body()
        ctype = self.headers.get("Content-Type", "")
        body = None
        if raw:
            try:
                body = json.loads(raw.decode("utf-8"))
            except Exception:
                body = {"_unparseable": raw.decode("utf-8", "replace")}
        auth = self.headers.get("Authorization", "")
        with LOCK:
            STATE["count"] += 1
            STATE["last"][method] = {"http_method": verb, "query": query, "json_body": body,
                                     "content_type": ctype, "authorization_present": bool(auth),
                                     "user_agent": self.headers.get("User-Agent", "")}
        if not auth.startswith("Bearer "):
            return self.send(200, {"ok": False, "error": "not_authed"})
        token = auth[7:]
        if token == "xoxb-inactive":
            return self.send(200, {"ok": False, "error": "account_inactive"})
        if token not in VALID:
            return self.send(200, {"ok": False, "error": "invalid_auth"})
        team = VALID[token]
        hdrs = {"X-OAuth-Scopes": SCOPES}
        warn = {} if verb == "GET" or "charset" in ctype.lower() else {"warning": "missing_charset"}
        ok = lambda extra: self.send(200, {"ok": True, **warn, **extra}, hdrs)
        err = lambda code, **extra: self.send(200, {"ok": False, "error": code, **extra}, hdrs)
        cursor = query.get("cursor", "")
        chan = query.get("channel") or (body or {}).get("channel", "")

        if method == "auth.test":
            return ok({"url": "https://e2e.slack.com/", "team": "E2E Workspace", "user": "cosmonic-mcp",
                       "team_id": team, "user_id": "U0BOTUSER", "bot_id": "B0BOT0001"})
        if method == "conversations.list":
            if cursor == "expired":
                return err("invalid_cursor")
            if "private_channel" in query.get("types", "") and cursor == "noscope":
                return err("invalid_types")
            if cursor == "page2":
                return ok({"channels": [CHANNELS["C0RANDOM1"]], "response_metadata": {"next_cursor": ""}})
            return ok({"channels": [CHANNELS["C0GENERAL"], CHANNELS["C0PRIV001"]],
                       "response_metadata": {"next_cursor": "page2"}})
        if method == "conversations.info":
            if chan == "C0MISSING1" or chan not in CHANNELS:
                return err("channel_not_found")
            return ok({"channel": CHANNELS[chan]})
        if method == "conversations.history":
            if chan == "C0NOTIN01":
                return err("not_in_channel")
            if chan == "C0SCOPE01":
                return err("missing_scope", needed="channels:history", provided="chat:write")
            if chan == "C0RATE001":
                return self.send(429, {"ok": False, "error": "ratelimited"}, {"Retry-After": "7"})
            if chan not in CHANNELS:
                return err("channel_not_found")
            if cursor == "expired":
                return err("invalid_cursor")
            if cursor == "page2":
                return ok({"messages": HISTORY_PAGE2, "has_more": False, "response_metadata": {"next_cursor": ""}})
            return ok({"messages": HISTORY, "has_more": True, "response_metadata": {"next_cursor": "page2"}})
        if method == "conversations.replies":
            if chan not in CHANNELS:
                return err("channel_not_found")
            if query.get("ts") == "1.000001":
                return err("thread_not_found")
            if cursor == "expired":
                return err("invalid_cursor")
            return ok({"messages": REPLIES, "has_more": False, "response_metadata": {"next_cursor": ""}})
        if method == "conversations.join":
            if chan == "C0ARCHIV1":
                return err("is_archived")
            if chan == "C0PRIV001":
                return err("method_not_supported_for_channel_type")
            if chan == "C0SCOPE01":
                return err("missing_scope", needed="channels:join", provided="channels:read")
            if chan not in CHANNELS:
                return err("channel_not_found")
            extra = {"warning": "already_in_channel"} if chan == "C0ALREADY" else {}
            return ok({"channel": CHANNELS[chan], **extra})
        if method == "users.list":
            if "limit" not in query:
                return err("limit_required")
            if cursor == "expired":
                return err("invalid_cursor")
            if cursor == "page2":
                return ok({"members": USERS_PAGE2, "response_metadata": {"next_cursor": ""}})
            return ok({"members": USERS, "response_metadata": {"next_cursor": "page2"}})
        if method == "users.profile.get":
            if query.get("user") == "U0NOPE001":
                return err("user_not_found")
            return ok({"profile": PROFILE})
        if method == "chat.postMessage":
            text = (body or {}).get("text", "")
            if text == "FIXTURE_5XX":
                return self.send(503, None, raw=b"<html>service unavailable</html>", ctype="text/html")
            if chan == "C0NOTIN01":
                return err("not_in_channel")
            if chan == "C0ARCHIV1":
                return err("is_archived")
            if chan == "C0RATE001":
                return self.send(429, {"ok": False, "error": "ratelimited"}, {"Retry-After": "3"})
            if not text:
                return err("no_text")
            if len(text) > 40000:
                return err("msg_too_long")
            return ok({"channel": chan, "ts": "1712345699.000100",
                       "message": {"text": text, "thread_ts": (body or {}).get("thread_ts")}})
        if method == "reactions.add":
            name = (body or {}).get("name", "")
            if name == "already":
                return err("already_reacted")
            if name == "bogus":
                return err("invalid_name")
            if chan == "C0ARCHIV1":
                return err("is_archived")
            return ok({})
        return err("unknown_method")

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
EOF
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "${FIXTURE}/__count" && break
  sleep 0.2
done

COMMON=(--env "SLACK_BASE_URL=${FIXTURE}" --env MCP_OUTBOUND_TIMEOUT_MS=10000)
mcp_harness_start "${COMMON[@]}" --env SLACK_BOT_TOKEN=xoxb-e2e-token --env SLACK_TEAM_ID=T0E2E
# Guard instance: NO SLACK_BOT_TOKEN — the missing-secret path.
mcp_harness_start_guard "${COMMON[@]}" --env SLACK_TEAM_ID=T0E2E
start_instance "$FENCE_PORT" fenced "${COMMON[@]}" --env SLACK_BOT_TOKEN=xoxb-e2e-token \
  --env SLACK_TEAM_ID=T0E2E --env "SLACK_CHANNEL_IDS=C0FENCE01, C0FENCE02"
start_instance "$RO_PORT" readonly "${COMMON[@]}" --env SLACK_BOT_TOKEN=xoxb-otherteam \
  --env SLACK_TEAM_ID=T0E2E --env SLACK_READ_ONLY=true
start_instance "$BAD_PORT" badtoken "${COMMON[@]}" --env SLACK_BOT_TOKEN=xoxb-bad --env SLACK_TEAM_ID=T0E2E

ALL_TOOLS=(check_auth list_channels get_channel_info get_channel_history get_thread_replies get_users get_user_profile post_message reply_to_thread add_reaction join_channel)
framework_tests "${ALL_TOOLS[@]}"
discovery_tests check_auth
skills_tests "$SKILL_NAME" "references/TOOLS.md" "references/ERRORS.md" "references/SCOPES.md"

echo "== discovery: credentials block =="
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / carries the credentials block" '"credentials"' "$ROOT"
assert_contains "GET / names the secret ref" '"ref": "slack-mcp-bot-token"' "$ROOT"
assert_contains "GET / reports the token as configured" '"status": "configured"' "$ROOT"
assert_contains "GET / points validate at check_auth" '"validate": "check_auth"' "$ROOT"
assert_not_contains "GET / never leaks the token value" 'xoxb-e2e-token' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD_BASE")
assert_contains "GET / on the guard reports the token as missing" '"status": "missing"' "$ROOT"

echo "== check_auth =="
OUT=$(mcp_call check_auth '{}')
assert_contains "check_auth ok" '"status":"ok"' "$OUT"
assert_contains "check_auth reports the workspace id" '"team_id":"T0E2E"' "$OUT"
assert_contains "check_auth reports the bot user id" '"user_id":"U0BOTUSER"' "$OUT"
assert_contains "check_auth cross-checks SLACK_TEAM_ID" '"team_id_match":true' "$OUT"
assert_contains "check_auth lists granted scopes from X-OAuth-Scopes" '"channels:history"' "$OUT"
assert_contains "check_auth has no missing required scopes" '"missing_required_scopes":[]' "$OUT"
assert_contains "check_auth reports the write policy" '"read_only":false' "$OUT"
assert_contains "check_auth readable text names the workspace" 'E2E Workspace' "$OUT"
LAST=$(fx_last auth.test)
assert_json "auth.test is a POST with the bearer header" 'r["http_method"] == "POST" and r["authorization_present"]' "$LAST"
assert_contains "auth.test JSON post carries charset" 'charset=utf-8' "$LAST"
OUT=$(mcp_call_on "$RO_BASE" check_auth '{}')
assert_contains "check_auth flags a SLACK_TEAM_ID mismatch" '"team_id_match":false' "$OUT"
assert_contains "check_auth mismatch remediation names the env var and the real id" 'SLACK_TEAM_ID=T0OTHER' "$OUT"
assert_contains "check_auth reports read_only on the read-only instance" '"read_only":true' "$OUT"
OUT=$(mcp_call_on "$FENCE_BASE" check_auth '{}')
assert_contains "check_auth reports the channel fence" '"channel_fence":["C0FENCE01","C0FENCE02"]' "$OUT"
OUT=$(mcp_call_on "$BAD_BASE" check_auth '{}')
assert_contains "check_auth invalid token is a tool error" '"isError":true' "$OUT"
assert_contains "check_auth invalid token reports status invalid" '"status":"invalid"' "$OUT"
assert_contains "check_auth invalid token surfaces Slack's code" 'invalid_auth' "$OUT"
assert_contains "check_auth invalid token names the secret ref" 'slack-mcp-bot-token' "$OUT"
assert_contains "check_auth invalid token says where to get a token" 'api.slack.com/apps' "$OUT"

echo "== list_channels =="
OUT=$(mcp_call list_channels '{}')
assert_contains "list_channels returns channels" '"id":"C0GENERAL"' "$OUT"
assert_contains "list_channels flags membership" '"is_member":true' "$OUT"
assert_contains "list_channels flattens topic" '"topic":"Topic of general"' "$OUT"
assert_contains "list_channels returns next_cursor" '"next_cursor":"page2"' "$OUT"
assert_contains "list_channels readable text" '#general' "$OUT"
LAST=$(fx_last conversations.list)
assert_json "conversations.list defaults: limit=100, public, exclude_archived, team_id" \
  'r["query"]["limit"] == "100" and r["query"]["types"] == "public_channel" and r["query"]["exclude_archived"] == "true" and r["query"]["team_id"] == "T0E2E" and "cursor" not in r["query"]' "$LAST"
OUT=$(mcp_call list_channels '{"limit":999}')
assert_json "list_channels clamps limit=999 to 200" 'r["query"]["limit"] == "200"' "$(fx_last conversations.list)"
OUT=$(mcp_call list_channels '{"limit":0}')
assert_json "list_channels clamps limit=0 to 1" 'r["query"]["limit"] == "1"' "$(fx_last conversations.list)"
OUT=$(mcp_call list_channels '{"limit":-9223372036854775808}')
assert_json "list_channels clamps i64::MIN to 1" 'r["query"]["limit"] == "1"' "$(fx_last conversations.list)"
OUT=$(mcp_call list_channels '{"cursor":"page2"}')
assert_contains "list_channels second page has an empty next_cursor" '"next_cursor":""' "$OUT"
assert_contains "list_channels second page content" '"id":"C0RANDOM1"' "$OUT"
assert_json "list_channels forwards the cursor" 'r["query"]["cursor"] == "page2"' "$(fx_last conversations.list)"
OUT=$(mcp_call list_channels '{"cursor":"expired"}')
assert_contains "list_channels invalid_cursor is mapped" 'invalid_cursor' "$OUT"
assert_contains "list_channels invalid_cursor says restart" 'Restart from the first page' "$OUT"
OUT=$(mcp_call list_channels '{"types":"public_channel,private_channel"}')
assert_json "list_channels forwards private_channel types" 'r["query"]["types"] == "public_channel,private_channel"' "$(fx_last conversations.list)"
assert_contains "list_channels with private types shows a private channel" '"is_private":true' "$OUT"
OUT=$(mcp_call list_channels '{"types":"public_channel,private_channel","cursor":"noscope"}')
assert_contains "list_channels invalid_types is mapped with the scope hint" 'groups:read' "$OUT"
OUT=$(mcp_call list_channels '{"types":"im"}')
assert_not_contains "list_channels rejects types=im" '"isError":false' "$OUT"
OUT=$(mcp_call list_channels '{"limit":"10"}')
assert_not_contains "list_channels rejects a string limit" '"isError":false' "$OUT"
OUT=$(mcp_call list_channels '{"cursor":"page2\"; DROP TABLE channels; --"}')
assert_contains "list_channels refuses an injection-shaped cursor" 'cursor contains' "$OUT"
OUT=$(mcp_call list_channels '{"cursor":"日本語"}')
assert_contains "list_channels refuses a non-ASCII cursor" 'cursor contains' "$OUT"
BIGCURSOR=$(python3 -c "print('A' * 5000)")
OUT=$(mcp_call list_channels "{\"cursor\":\"$BIGCURSOR\"}")
assert_contains "list_channels refuses a 5000-byte cursor" 'cursor is 5000 bytes' "$OUT"
OUT=$(mcp_call_on "$FENCE_BASE" list_channels '{"limit":5}')
assert_contains "fenced list_channels returns the configured channel" '"id":"C0FENCE01"' "$OUT"
assert_not_contains "fenced list_channels drops the archived configured channel" '"id":"C0FENCE02"' "$OUT"
assert_contains "fenced list_channels uses conversations.info" '"source":"conversations.info"' "$OUT"
assert_contains "fenced list_channels has no cursor" '"next_cursor":""' "$OUT"

echo "== get_channel_info =="
OUT=$(mcp_call get_channel_info '{"channel_id":"C0GENERAL"}')
assert_contains "get_channel_info returns the channel" '"name":"general"' "$OUT"
assert_contains "get_channel_info reports member count" '"num_members":42' "$OUT"
assert_contains "get_channel_info readable text says membership" 'bot is a member' "$OUT"
assert_json "conversations.info asks for the member count" 'r["query"]["include_num_members"] == "true" and r["query"]["channel"] == "C0GENERAL"' "$(fx_last conversations.info)"
OUT=$(mcp_call get_channel_info '{"channel_id":"C0RANDOM1"}')
assert_contains "get_channel_info flags a non-member channel" 'NOT a member' "$OUT"
OUT=$(mcp_call get_channel_info '{"channel_id":"#general"}')
assert_contains "get_channel_info refuses a #name" 'not a name' "$OUT"
assert_contains "get_channel_info #name hint points at list_channels" 'list_channels' "$OUT"
OUT=$(mcp_call get_channel_info '{"channel_id":"C0MISSING1"}')
assert_contains "get_channel_info channel_not_found is mapped" 'channel_not_found' "$OUT"
assert_contains "get_channel_info channel_not_found hint" 'never a #name' "$OUT"
OUT=$(mcp_call get_channel_info '{"channel_id":"c0general"}')
assert_contains "get_channel_info refuses a lowercase id" 'not a Slack ID' "$OUT"
BEFORE=$(fx_count)
BIGID=$(python3 -c "print('C' * 3000)")
OUT=$(mcp_call get_channel_info "{\"channel_id\":\"$BIGID\"}")
assert_contains "get_channel_info refuses a 3000-char id" 'not a Slack ID' "$OUT"
assert_contains "get_channel_info bad id never reaches Slack" "$BEFORE" "$(fx_count)"
OUT=$(mcp_call get_channel_info '{"channel_id":"C0GENERAL&limit=999"}')
assert_contains "get_channel_info refuses a query-injection id" 'not a Slack ID' "$OUT"
OUT=$(mcp_call get_channel_info '{"channel_id":""}')
assert_contains "get_channel_info refuses an empty id" 'channel_id is required' "$OUT"
OUT=$(mcp_call get_channel_info '{}')
assert_not_contains "get_channel_info missing channel_id is an error" '"isError":false' "$OUT"

echo "== get_channel_history =="
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL"}')
assert_contains "history returns messages" '"ts":"1712345678.000300"' "$OUT"
assert_contains "history keeps thread metadata" '"reply_count":2' "$OUT"
assert_json "history summarizes reactions" 'r["result"]["structuredContent"]["messages"][0]["reactions"][0] == {"name": "thumbsup", "count": 2}' "$OUT"
assert_contains "history round-trips unicode text" '日本語' "$OUT"
assert_contains "history keeps Slack entity escapes verbatim" '&lt;tags&gt;' "$OUT"
assert_contains "history truncates a 5000-char message on a char boundary" '"text_truncated":true' "$OUT"
assert_contains "history flags attachments" '"has_attachments":true' "$OUT"
assert_contains "history flags edited messages" '"edited":true' "$OUT"
assert_contains "history keeps bot messages" '"bot_id":"B0BOT0001"' "$OUT"
assert_contains "history reports has_more" '"has_more":true' "$OUT"
assert_contains "history reports next_cursor" '"next_cursor":"page2"' "$OUT"
assert_contains "history is newest first" '"order":"newest first"' "$OUT"
assert_contains "history readable text marks threads" '(thread, 2 replies)' "$OUT"
assert_json "conversations.history default limit is 10" 'r["query"]["limit"] == "10" and r["query"]["channel"] == "C0GENERAL" and "cursor" not in r["query"]' "$(fx_last conversations.history)"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","limit":999}')
assert_json "history clamps limit=999 to 200" 'r["query"]["limit"] == "200"' "$(fx_last conversations.history)"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","limit":0}')
assert_json "history clamps limit=0 to 1" 'r["query"]["limit"] == "1"' "$(fx_last conversations.history)"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","oldest":"1712345600.000000","latest":"1712345700.000000","inclusive":true,"limit":50}')
assert_json "history forwards oldest/latest/inclusive" 'r["query"]["oldest"] == "1712345600.000000" and r["query"]["latest"] == "1712345700.000000" and r["query"]["inclusive"] == "true"' "$(fx_last conversations.history)"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","cursor":"page2"}')
assert_contains "history second page has has_more=false" '"has_more":false' "$OUT"
assert_json "history forwards the cursor" 'r["query"]["cursor"] == "page2"' "$(fx_last conversations.history)"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","cursor":"expired"}')
assert_contains "history invalid_cursor is mapped" 'invalid_cursor' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","oldest":"yesterday"}')
assert_contains "history refuses a non-ts oldest" 'not a Slack message timestamp' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","latest":"1712345678.5e3"}')
assert_contains "history refuses a float-ish latest" 'not a Slack message timestamp' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0NOTIN01"}')
assert_contains "history not_in_channel is mapped" 'not_in_channel' "$OUT"
assert_contains "history not_in_channel suggests an invite" '/invite' "$OUT"
assert_contains "history not_in_channel suggests join_channel" 'join_channel' "$OUT"
assert_contains "history not_in_channel is not retryable" '"retryable":false' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0SCOPE01"}')
assert_contains "history missing_scope is mapped" 'missing_scope' "$OUT"
assert_contains "history missing_scope shows needed and provided" 'needs scope channels:history, token has chat:write' "$OUT"
assert_contains "history missing_scope says to reinstall" 'REINSTALL' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0RATE001"}')
assert_contains "history HTTP 429 is mapped" 'rate limited' "$OUT"
assert_contains "history 429 surfaces Retry-After" 'Retry-After: 7 s' "$OUT"
assert_contains "history 429 is retryable" '"retryable":true' "$OUT"
assert_contains "history 429 carries the ratelimited code" '"error":"ratelimited"' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0MISSING1"}')
assert_contains "history channel_not_found is mapped" 'channel_not_found' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"@alice"}')
assert_contains "history refuses an @name" 'not a name' "$OUT"
OUT=$(mcp_call get_channel_history '{"channel_id":"C0GENERAL","limit":1.5}')
assert_not_contains "history rejects a fractional limit" '"isError":false' "$OUT"

echo "== get_thread_replies =="
OUT=$(mcp_call get_thread_replies '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200"}')
assert_contains "replies returns the thread" '"text":"reply one"' "$OUT"
assert_contains "replies round-trips emoji" 'reply two 🎉' "$OUT"
assert_contains "replies reports count" '"count":3' "$OUT"
assert_contains "replies is oldest first with the parent at 0" 'parent is element 0' "$OUT"
assert_contains "replies echoes thread_ts" '"thread_ts":"1712345678.000200"' "$OUT"
assert_json "conversations.replies sends ts and default limit 100" 'r["query"]["ts"] == "1712345678.000200" and r["query"]["limit"] == "100" and r["query"]["channel"] == "C0GENERAL"' "$(fx_last conversations.replies)"
OUT=$(mcp_call get_thread_replies '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200","cursor":"next","limit":500}')
assert_json "replies forwards cursor and clamps limit" 'r["query"]["cursor"] == "next" and r["query"]["limit"] == "200"' "$(fx_last conversations.replies)"
OUT=$(mcp_call get_thread_replies '{"channel_id":"C0GENERAL","thread_ts":"1.000001"}')
assert_contains "replies thread_not_found is mapped" 'thread_not_found' "$OUT"
assert_contains "replies thread_not_found hint mentions the parent ts" "parent" "$OUT"
OUT=$(mcp_call get_thread_replies '{"channel_id":"C0GENERAL","thread_ts":"abc"}')
assert_contains "replies refuses a non-ts thread_ts" 'not a Slack message timestamp' "$OUT"
OUT=$(mcp_call get_thread_replies '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200; rm -rf /"}')
assert_contains "replies refuses an injection-shaped ts" 'not a Slack message timestamp' "$OUT"
OUT=$(mcp_call get_thread_replies '{"channel_id":"C0GENERAL","thread_ts":"99999999999999999999999.1"}')
assert_contains "replies refuses an over-long ts" 'not a Slack message timestamp' "$OUT"
OUT=$(mcp_call get_thread_replies '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200","cursor":"expired"}')
assert_contains "replies invalid_cursor is mapped" 'invalid_cursor' "$OUT"

echo "== get_users =="
OUT=$(mcp_call get_users '{}')
assert_contains "get_users returns members" '"id":"U0ALICE01"' "$OUT"
assert_contains "get_users flattens display_name" '"display_name":"alice"' "$OUT"
assert_contains "get_users flags deactivated users" '"deleted":true' "$OUT"
assert_contains "get_users returns next_cursor" '"next_cursor":"page2"' "$OUT"
assert_contains "get_users readable text" '@alice (Alice Liddell)' "$OUT"
assert_json "users.list always sends limit (default 100) and team_id" 'r["query"]["limit"] == "100" and r["query"]["team_id"] == "T0E2E"' "$(fx_last users.list)"
OUT=$(mcp_call get_users '{"limit":0}')
assert_json "get_users clamps limit=0 to 1" 'r["query"]["limit"] == "1"' "$(fx_last users.list)"
OUT=$(mcp_call get_users '{"limit":100000}')
assert_json "get_users clamps limit=100000 to 200" 'r["query"]["limit"] == "200"' "$(fx_last users.list)"
OUT=$(mcp_call get_users '{"cursor":"page2"}')
assert_contains "get_users second page" '"id":"U0CAROL01"' "$OUT"
assert_contains "get_users second page flags bots" '"is_bot":true' "$OUT"
assert_contains "get_users second page ends paging" '"next_cursor":""' "$OUT"
OUT=$(mcp_call get_users '{"cursor":"expired"}')
assert_contains "get_users invalid_cursor is mapped" 'invalid_cursor' "$OUT"
OUT=$(mcp_call get_users '{"cursor":" "}')
assert_json "get_users treats a blank cursor as page 1" '"cursor" not in r["query"]' "$(fx_last users.list)"

echo "== get_user_profile =="
OUT=$(mcp_call get_user_profile '{"user_id":"U0ALICE01"}')
assert_contains "get_user_profile returns the profile" '"real_name":"Alice Liddell"' "$OUT"
assert_contains "get_user_profile round-trips unicode status" '🚀 déployé' "$OUT"
assert_contains "get_user_profile returns the email when granted" '"email":"alice@example.com"' "$OUT"
assert_contains "get_user_profile picks the largest avatar" '"avatar_url":"https://example.com/a512.png"' "$OUT"
assert_contains "get_user_profile returns pronouns" '"pronouns":"she/her"' "$OUT"
assert_json "users.profile.get sends user and no include_labels by default" 'r["query"]["user"] == "U0ALICE01" and "include_labels" not in r["query"]' "$(fx_last users.profile.get)"
OUT=$(mcp_call get_user_profile '{"user_id":"U0ALICE01","include_labels":true}')
assert_json "users.profile.get forwards include_labels" 'r["query"]["include_labels"] == "true"' "$(fx_last users.profile.get)"
OUT=$(mcp_call get_user_profile '{"user_id":"U0NOPE001"}')
assert_contains "get_user_profile user_not_found is mapped" 'user_not_found' "$OUT"
assert_contains "get_user_profile user_not_found points at get_users" 'get_users' "$OUT"
OUT=$(mcp_call get_user_profile '{"user_id":"@alice"}')
assert_contains "get_user_profile refuses an @name" 'not a name' "$OUT"
OUT=$(mcp_call get_user_profile '{"user_id":"C0GENERAL"}')
assert_contains "get_user_profile refuses a channel id" 'not a Slack ID' "$OUT"
OUT=$(mcp_call get_user_profile '{"user_id":"W0GRID0001"}')
assert_contains "get_user_profile accepts an Enterprise Grid W… id" '"user_id":"W0GRID0001"' "$OUT"

echo "== post_message =="
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL","text":"hello *world*"}')
assert_contains "post_message posts" '"ts":"1712345699.000100"' "$OUT"
assert_contains "post_message echoes the channel" '"channel":"C0GENERAL"' "$OUT"
assert_contains "post_message builds a permalink hint" 'https://slack.com/archives/C0GENERAL/p1712345699000100' "$OUT"
assert_contains "post_message has no warnings for short text" '"warnings":[]' "$OUT"
LAST=$(fx_last chat.postMessage)
assert_json "chat.postMessage is a JSON POST with channel and text" 'r["http_method"] == "POST" and r["json_body"]["channel"] == "C0GENERAL" and r["json_body"]["text"] == "hello *world*" and "thread_ts" not in r["json_body"]' "$LAST"
assert_contains "chat.postMessage sends charset=utf-8" 'charset=utf-8' "$LAST"
assert_json "chat.postMessage carries the bearer header" 'r["authorization_present"] is True' "$LAST"
assert_contains "chat.postMessage sends a descriptive User-Agent" 'slack-mcp/' "$LAST"
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL","text":"héllo 🚀 — 日本語 \"quoted\" \\ backslash"}')
assert_json "post_message round-trips unicode text byte-exact" 'r["json_body"]["text"] == "héllo 🚀 — 日本語 \"quoted\" \\ backslash"' "$(fx_last chat.postMessage)"
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL","text":"\"; DROP TABLE users; -- <script>alert(1)</script> {{7*7}} ${HOME} %s%n"}')
assert_json "post_message passes injection-shaped text verbatim" 'r["json_body"]["text"] == "\"; DROP TABLE users; -- <script>alert(1)</script> {{7*7}} ${HOME} %s%n"' "$(fx_last chat.postMessage)"
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL","text":"no unfurl","unfurl_links":false,"unfurl_media":false}')
assert_json "post_message forwards unfurl flags" 'r["json_body"]["unfurl_links"] is False and r["json_body"]["unfurl_media"] is False' "$(fx_last chat.postMessage)"
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL","text":""}')
assert_contains "post_message refuses empty text" 'no_text' "$OUT"
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL","text":"   "}')
assert_contains "post_message refuses whitespace-only text" 'text is required' "$OUT"
BEFORE=$(fx_count)
TOOLONG=$(python3 -c "print('y' * 40001)")
OUT=$(mcp_call post_message "{\"channel_id\":\"C0GENERAL\",\"text\":\"$TOOLONG\"}")
assert_contains "post_message refuses 40,001 chars client-side" 'msg_too_long' "$OUT"
assert_contains "post_message over-long text never reaches Slack" "$BEFORE" "$(fx_count)"
MULTI=$(python3 -c "print('☃' * 40000)")
OUT=$(mcp_call post_message "{\"channel_id\":\"C0GENERAL\",\"text\":\"$MULTI\"}")
assert_contains "post_message counts characters, not bytes (40,000 snowmen accepted)" '"ts":"1712345699.000100"' "$OUT"
assert_contains "post_message warns above 4,000 characters" '40000 characters' "$OUT"
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL","text":"FIXTURE_5XX"}')
assert_contains "post_message HTTP 503 is mapped" 'HTTP 503' "$OUT"
assert_contains "post_message 503 is marked retryable" '"retryable":true' "$OUT"
assert_contains "post_message 503 advises one retry" 'retry once' "$OUT"
OUT=$(mcp_call post_message '{"channel_id":"C0NOTIN01","text":"hi"}')
assert_contains "post_message not_in_channel is mapped" 'not_in_channel' "$OUT"
assert_contains "post_message not_in_channel mentions chat:write.public" 'chat:write.public' "$OUT"
OUT=$(mcp_call post_message '{"channel_id":"C0ARCHIV1","text":"hi"}')
assert_contains "post_message is_archived is mapped" 'is_archived' "$OUT"
assert_contains "post_message is_archived says do not retry" 'Do not retry' "$OUT"
OUT=$(mcp_call post_message '{"channel_id":"C0RATE001","text":"hi"}')
assert_contains "post_message 429 surfaces Retry-After" 'Retry-After: 3 s' "$OUT"
OUT=$(mcp_call post_message '{"channel_id":"#general","text":"hi"}')
assert_contains "post_message refuses a #name" 'not a name' "$OUT"
OUT=$(mcp_call post_message '{"channel_id":"C0GENERAL"}')
assert_not_contains "post_message missing text is an error" '"isError":false' "$OUT"
BEFORE=$(fx_count)
OUT=$(mcp_call_on "$RO_BASE" post_message '{"channel_id":"C0GENERAL","text":"hi"}')
assert_contains "read-only instance refuses post_message" 'writes disabled by SLACK_READ_ONLY' "$OUT"
assert_contains "read-only refusal is a tool error" '"isError":true' "$OUT"
assert_contains "read-only refusal never reaches Slack" "$BEFORE" "$(fx_count)"
OUT=$(mcp_call_on "$FENCE_BASE" post_message '{"channel_id":"C0FENCE01","text":"inside the fence"}')
assert_contains "fenced instance posts to a fenced channel" '"ts":"1712345699.000100"' "$OUT"
BEFORE=$(fx_count)
OUT=$(mcp_call_on "$FENCE_BASE" post_message '{"channel_id":"C0OTHER01","text":"outside"}')
assert_contains "fenced instance refuses a channel outside SLACK_CHANNEL_IDS" 'not in SLACK_CHANNEL_IDS' "$OUT"
assert_contains "fence refusal lists the allowed channels" 'C0FENCE01,C0FENCE02' "$OUT"
assert_contains "fence refusal never reaches Slack" "$BEFORE" "$(fx_count)"

echo "== reply_to_thread =="
OUT=$(mcp_call reply_to_thread '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200","text":"in thread","reply_broadcast":true}')
assert_contains "reply_to_thread posts" '"ts":"1712345699.000100"' "$OUT"
assert_contains "reply_to_thread echoes thread_ts" '"thread_ts":"1712345678.000200"' "$OUT"
assert_contains "reply_to_thread permalink carries the thread" 'thread_ts=1712345678.000200&cid=C0GENERAL' "$OUT"
assert_json "chat.postMessage carries thread_ts and reply_broadcast" 'r["json_body"]["thread_ts"] == "1712345678.000200" and r["json_body"]["reply_broadcast"] is True and r["json_body"]["text"] == "in thread"' "$(fx_last chat.postMessage)"
OUT=$(mcp_call reply_to_thread '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200","text":"quiet"}')
assert_json "reply_broadcast is omitted by default" '"reply_broadcast" not in r["json_body"]' "$(fx_last chat.postMessage)"
OUT=$(mcp_call reply_to_thread '{"channel_id":"C0GENERAL","thread_ts":"1712345678","text":"x"}')
assert_contains "reply_to_thread accepts an integer-seconds ts" '"thread_ts":"1712345678"' "$OUT"
OUT=$(mcp_call reply_to_thread '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200.1","text":"x"}')
assert_contains "reply_to_thread refuses a malformed ts" 'not a Slack message timestamp' "$OUT"
OUT=$(mcp_call reply_to_thread "{\"channel_id\":\"C0GENERAL\",\"thread_ts\":\"1712345678.000200\",\"text\":\"$TOOLONG\"}")
assert_contains "reply_to_thread refuses over-long text" 'msg_too_long' "$OUT"
OUT=$(mcp_call reply_to_thread '{"channel_id":"C0NOTIN01","thread_ts":"1712345678.000200","text":"x"}')
assert_contains "reply_to_thread not_in_channel is mapped" 'not_in_channel' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" reply_to_thread '{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200","text":"x"}')
assert_contains "read-only instance refuses reply_to_thread" 'writes disabled by SLACK_READ_ONLY' "$OUT"
OUT=$(mcp_call_on "$FENCE_BASE" reply_to_thread '{"channel_id":"C0OTHER01","thread_ts":"1712345678.000200","text":"x"}')
assert_contains "fenced instance refuses reply_to_thread outside the fence" 'not in SLACK_CHANNEL_IDS' "$OUT"

echo "== add_reaction =="
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":":thumbsup:"}')
assert_contains "add_reaction succeeds" '"reaction":"thumbsup"' "$OUT"
assert_not_contains "add_reaction happy path is not flagged already_reacted" '"already_reacted":true' "$OUT"
assert_json "reactions.add strips the colons and sends channel/timestamp/name" 'r["json_body"]["name"] == "thumbsup" and r["json_body"]["channel"] == "C0GENERAL" and r["json_body"]["timestamp"] == "1712345678.000300"' "$(fx_last reactions.add)"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"already"}')
assert_contains "add_reaction already_reacted counts as success" '"already_reacted":true' "$OUT"
assert_contains "add_reaction already_reacted is not an error" '"isError":false' "$OUT"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"bogus"}')
assert_contains "add_reaction invalid_name is mapped" 'invalid_name' "$OUT"
assert_contains "add_reaction invalid_name suggests a short name" 'white_check_mark' "$OUT"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"thumbsup::skin-tone-3"}')
assert_json "add_reaction accepts a skin-tone suffix" 'r["json_body"]["name"] == "thumbsup::skin-tone-3"' "$(fx_last reactions.add)"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"Thumbs Up!"}')
assert_contains "add_reaction refuses a display name" 'not an emoji short name' "$OUT"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"👍"}')
assert_contains "add_reaction refuses a literal emoji" 'not an emoji short name' "$OUT"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"thumbsup::skin-tone-9"}')
assert_contains "add_reaction refuses an out-of-range skin tone" 'not an emoji short name' "$OUT"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":""}')
assert_contains "add_reaction refuses an empty name" 'not an emoji short name' "$OUT"
OUT=$(mcp_call add_reaction '{"channel_id":"C0GENERAL","timestamp":"now","reaction":"eyes"}')
assert_contains "add_reaction refuses a non-ts timestamp" 'not a Slack message timestamp' "$OUT"
OUT=$(mcp_call add_reaction '{"channel_id":"C0ARCHIV1","timestamp":"1712345678.000300","reaction":"eyes"}')
assert_contains "add_reaction is_archived is mapped" 'is_archived' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" add_reaction '{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"eyes"}')
assert_contains "read-only instance refuses add_reaction" 'writes disabled by SLACK_READ_ONLY' "$OUT"
OUT=$(mcp_call_on "$FENCE_BASE" add_reaction '{"channel_id":"C0OTHER01","timestamp":"1712345678.000300","reaction":"eyes"}')
assert_contains "fenced instance refuses add_reaction outside the fence" 'not in SLACK_CHANNEL_IDS' "$OUT"

echo "== join_channel =="
OUT=$(mcp_call join_channel '{"channel_id":"C0GENERAL"}')
assert_contains "join_channel joins" 'bot joined C0GENERAL #general' "$OUT"
assert_contains "join_channel reports already_in_channel=false" '"already_in_channel":false' "$OUT"
assert_json "conversations.join is a JSON POST with channel" 'r["http_method"] == "POST" and r["json_body"]["channel"] == "C0GENERAL"' "$(fx_last conversations.join)"
OUT=$(mcp_call join_channel '{"channel_id":"C0ALREADY"}')
assert_contains "join_channel already_in_channel is success" '"already_in_channel":true' "$OUT"
OUT=$(mcp_call join_channel '{"channel_id":"C0ARCHIV1"}')
assert_contains "join_channel is_archived is mapped" 'is_archived' "$OUT"
OUT=$(mcp_call join_channel '{"channel_id":"C0PRIV001"}')
assert_contains "join_channel private channel is mapped" 'method_not_supported_for_channel_type' "$OUT"
assert_contains "join_channel private channel is a policy error" 'workspace policy' "$OUT"
OUT=$(mcp_call join_channel '{"channel_id":"C0SCOPE01"}')
assert_contains "join_channel missing_scope names channels:join" 'needs scope channels:join' "$OUT"
OUT=$(mcp_call join_channel '{"channel_id":"general"}')
assert_contains "join_channel refuses a bare name" 'not a Slack ID' "$OUT"
OUT=$(mcp_call join_channel '{"channel_id":"../../etc/passwd"}')
assert_contains "join_channel refuses a traversal-shaped id" 'not a Slack ID' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" join_channel '{"channel_id":"C0GENERAL"}')
assert_contains "read-only instance refuses join_channel" 'writes disabled by SLACK_READ_ONLY' "$OUT"
OUT=$(mcp_call_on "$FENCE_BASE" join_channel '{"channel_id":"C0OTHER01"}')
assert_contains "fenced instance refuses join_channel outside the fence" 'not in SLACK_CHANNEL_IDS' "$OUT"
OUT=$(mcp_call_on "$FENCE_BASE" join_channel '{"channel_id":"C0FENCE01"}')
assert_contains "fenced instance joins a fenced channel" 'bot joined C0FENCE01' "$OUT"

echo "== invalid credential (bad-token instance) =="
OUT=$(mcp_call_on "$BAD_BASE" list_channels '{}')
assert_contains "bad token: list_channels surfaces invalid_auth" 'invalid_auth' "$OUT"
assert_contains "bad token: error names the secret ref" 'slack-mcp-bot-token' "$OUT"
assert_contains "bad token: error names the env var" 'SLACK_BOT_TOKEN' "$OUT"
assert_contains "bad token: error is not retryable" '"retryable":false' "$OUT"
OUT=$(mcp_call_on "$BAD_BASE" post_message '{"channel_id":"C0GENERAL","text":"hi"}')
assert_contains "bad token: post_message surfaces invalid_auth" 'invalid_auth' "$OUT"

echo "== missing secret (guard instance) =="
GUARD_ARGS=(
  'check_auth|{}'
  'list_channels|{}'
  'get_channel_info|{"channel_id":"C0GENERAL"}'
  'get_channel_history|{"channel_id":"C0GENERAL"}'
  'get_thread_replies|{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200"}'
  'get_users|{}'
  'get_user_profile|{"user_id":"U0ALICE01"}'
  'post_message|{"channel_id":"C0GENERAL","text":"hi"}'
  'reply_to_thread|{"channel_id":"C0GENERAL","thread_ts":"1712345678.000200","text":"hi"}'
  'add_reaction|{"channel_id":"C0GENERAL","timestamp":"1712345678.000300","reaction":"eyes"}'
  'join_channel|{"channel_id":"C0GENERAL"}'
)
BEFORE=$(fx_count)
for entry in "${GUARD_ARGS[@]}"; do
  tool="${entry%%|*}"
  args="${entry#*|}"
  OUT=$(mcp_call_on "$GUARD_BASE" "$tool" "$args")
  assert_contains "guard: $tool reports the missing secret" 'SLACK_BOT_TOKEN is not set' "$OUT"
  assert_contains "guard: $tool names the secret ref" 'slack-mcp-bot-token' "$OUT"
  assert_contains "guard: $tool is a tool error" '"isError":true' "$OUT"
done
OUT=$(mcp_call_on "$GUARD_BASE" check_auth '{}')
assert_contains "guard: check_auth reports status missing" '"status":"missing"' "$OUT"
assert_contains "guard: check_auth remediation has the setup link" 'api.slack.com/apps' "$OUT"
assert_contains "guard: missing secret never reaches Slack" "$BEFORE" "$(fx_count)"

if [ "${E2E_LIVE:-0}" = "1" ] && [ -n "${SLACK_BOT_TOKEN:-}" ]; then
  echo "== live (E2E_LIVE=1, slack.com) =="
  start_instance "$LIVE_PORT" live --env "SLACK_BOT_TOKEN=${SLACK_BOT_TOKEN}" \
    --env "SLACK_TEAM_ID=${SLACK_TEAM_ID:-}"
  OUT=$(mcp_call_on "http://127.0.0.1:${LIVE_PORT}/" check_auth '{}')
  assert_contains "live check_auth ok" '"status":"ok"' "$OUT"
  OUT=$(mcp_call_on "http://127.0.0.1:${LIVE_PORT}/" list_channels '{"limit":1}')
  assert_contains "live list_channels limit=1" '"channels":[' "$OUT"
else
  echo "== live cases skipped (set E2E_LIVE=1 and SLACK_BOT_TOKEN to run them) =="
fi

guard_tests
mcp_harness_report
