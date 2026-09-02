#!/usr/bin/env bash
# End-to-end tests for atlassian-confluence-mcp. Framework checks (protocol,
# spec enforcement, discovery route, skills over MCP, robustness, Host guard)
# come from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases
# live below and run against a hermetic Python fixture that impersonates
# Confluence Cloud (v2 + the v1 search/user/label endpoints).
#
# Instances under test (all wasmtime, all pointed at the fixture):
#   PORT         primary: full credentials, no gates
#   GUARD_PORT   guard: no ATLASSIAN_API_TOKEN (missing-secret path) + Host guard
#   WRITER_PORT  CONFLUENCE_ALLOW_DELETE=true + CONFLUENCE_SPACES_FILTER=ENG,DOCS
#   RO_PORT      CONFLUENCE_READ_ONLY=true + ATLASSIAN_CLOUD_ID (gateway route)
#
# Usage: scripts/e2e.sh [--no-build]
#   E2E_LIVE=1 with ATLASSIAN_SITE / ATLASSIAN_EMAIL / ATLASSIAN_API_TOKEN
#   exported adds three read-only calls against the real site.
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9223}
GUARD_PORT=${GUARD_PORT:-9224}
FIXTURE_PORT=${FIXTURE_PORT:-9225}
WRITER_PORT=${WRITER_PORT:-9226}
RO_PORT=${RO_PORT:-9227}
LIVE_PORT=${LIVE_PORT:-9228}
WASM=${WASM:-target/wasm32-wasip2/release/atlassian_confluence_mcp.wasm}
SKILL_NAME=atlassian-confluence-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

EXTRA_PIDS=()
cleanup_extra() {
  local pid
  for pid in "${EXTRA_PIDS[@]}"; do kill "$pid" 2>/dev/null; done
}
trap 'cleanup_extra; mcp_harness_cleanup' EXIT

# start_extra <port> [wasmtime args...] — another instance with its own env.
start_extra() {
  local port="$1"; shift
  "$WASMTIME" serve -Sp3,cli,http "$@" --addr "127.0.0.1:${port}" "$WASM" \
    >"$E2E_TMP/extra-${port}.log" 2>&1 &
  EXTRA_PIDS+=($!)
  mcp_wait_ready "$port"
}

# assert_sc <name> <python-expr over sc> <sse-body> — structuredContent check.
assert_sc() {
  assert_json "$1" "(lambda sc: ($2))(r[\"result\"][\"structuredContent\"])" "$3"
}
# The fixture's record of the last upstream request / its counters.
last() { curl -sS --max-time 10 "http://127.0.0.1:${FIXTURE_PORT}/__last"; }
counters() { curl -sS --max-time 10 "http://127.0.0.1:${FIXTURE_PORT}/__counters"; }
# assert_last <name> <python-expr over r (the last request)>
assert_last() { assert_json "$1" "$2" "$(last)"; }
# assert_eq <name> <expected> <actual>
assert_eq() {
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1" "expected [$2], got [$3]"; fi
}
# counter <key>
counter() { counters | python3 -c "import json,sys; print(json.load(sys.stdin)[\"$1\"])"; }

# The concurrency test in framework_tests fires this tool 8x in parallel.
FIRST_TOOL_NAME=get_current_user
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"accountId"'

mcp_build_if_needed "${1:-}"

# ---------------------------------------------------------------------------
# Fixture: a threaded HTTP server impersonating Confluence Cloud.
# ---------------------------------------------------------------------------
python3 - "$FIXTURE_PORT" <<'EOF' >"$E2E_TMP/fixture.log" 2>&1 &
import sys, json, base64, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit, parse_qs, quote

PORT = int(sys.argv[1])
EXPECTED_AUTH = "Basic " + base64.b64encode(b"e2e@example.com:fixture-token").decode()
LOCK = threading.Lock()
STATE = {"last": None, "counters": {"429": 0, "put": 0, "post_pages": 0, "delete": 0, "label": 0, "comment": 0, "requests": 0}}

PAGE_STORAGE = (
    '<h1>Title &amp; More</h1>'
    '<p>Intro with <strong>bold</strong> and a <a href="https://example.com/x">link</a>.</p>'
    '<h2>List</h2><ul><li>one</li><li>two &lt;three&gt;</li></ul>'
    '<table><tbody><tr><th>Col A</th><th>Col B</th></tr><tr><td>1</td><td>2</td></tr></tbody></table>'
    '<ac:structured-macro ac:name="code"><ac:parameter ac:name="language">python</ac:parameter>'
    '<ac:plain-text-body><![CDATA[print("hi <world>")]]></ac:plain-text-body></ac:structured-macro>'
    '<ac:structured-macro ac:name="toc" />'
    '<ac:structured-macro ac:name="info"><ac:rich-text-body><p>Note body</p></ac:rich-text-body></ac:structured-macro>'
    '<p><ac:image><ri:attachment ri:filename="diagram.png" /></ac:image></p>'
    '<ac:task-list><ac:task><ac:task-id>1</ac:task-id><ac:task-status>complete</ac:task-status>'
    '<ac:task-body>done thing</ac:task-body></ac:task></ac:task-list>'
    '<p>Emoji ☃ 日本語 &nbsp; end</p>'
)
LONG_STORAGE = "<p>" + ("☃" * 5000) + "</p>"
PAGE_ADF = {"type": "doc", "version": 1, "content": [
    {"type": "heading", "attrs": {"level": 1}, "content": [{"type": "text", "text": "ADF Title"}]},
    {"type": "paragraph", "content": [
        {"type": "text", "text": "Hello "},
        {"type": "text", "text": "bold", "marks": [{"type": "strong"}]},
        {"type": "text", "text": " link", "marks": [{"type": "link", "attrs": {"href": "https://example.com/adf"}}]}]},
    {"type": "codeBlock", "attrs": {"language": "rust"}, "content": [{"type": "text", "text": "fn main() {}"}]},
    {"type": "table", "content": [
        {"type": "tableRow", "content": [
            {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "H1"}]}]},
            {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "H2"}]}]}]},
        {"type": "tableRow", "content": [
            {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "a"}]}]},
            {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "b"}]}]}]}]},
    {"type": "bulletList", "content": [{"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "item"}]}]}]},
]}
SPACES = {"ENG": (100, "Engineering"), "DOCS": (101, "Documentation"), "HR": (102, "People")}

def space(key):
    sid, name = SPACES[key]
    return {"id": str(sid), "key": key, "name": name, "type": "global", "status": "current",
            "authorId": "5b10a2844c20165700ede21g", "createdAt": "2020-01-01T00:00:00.000Z",
            "homepageId": str(sid * 10),
            "description": {"plain": {"value": name + " space", "representation": "plain"}},
            "_links": {"webui": "/spaces/" + key}}

def page(pid, title, version=7, body=None, child_position=None, labels=False):
    p = {"id": str(pid), "status": "current", "title": title, "spaceId": "100", "parentId": "1000",
         "parentType": "page", "position": 1, "authorId": "5b10a2844c20165700ede21g",
         "createdAt": "2026-01-01T00:00:00.000Z",
         "version": {"createdAt": "2026-08-30T10:00:00.000Z", "message": "", "number": version,
                     "minorEdit": False, "authorId": "5b10a2844c20165700ede21g"},
         "_links": {"webui": "/spaces/ENG/pages/%s/%s" % (pid, quote(title)),
                    "editui": "/pages/resumedraft.action?draftId=%s" % pid,
                    "tinyui": "/x/abc", "base": "https://fixture.atlassian.net/wiki"}}
    if body is not None:
        p["body"] = body
    if child_position is not None:
        p["childPosition"] = child_position
    if labels:
        p["labels"] = {"results": [{"id": "1", "name": "runbook", "prefix": "global"}],
                       "meta": {"hasMore": False, "cursor": ""}}
    return p

def comment(cid, body, parent=None, inline=False):
    c = {"id": str(cid), "status": "current", "title": "Re: Kubernetes Runbook", "pageId": "123",
         "version": {"createdAt": "2026-08-31T00:00:00.000Z", "message": "", "number": 1,
                     "minorEdit": False, "authorId": "5b10a2844c20165700ede21g"},
         "body": {"storage": {"representation": "storage", "value": body}},
         "_links": {"webui": "/spaces/ENG/pages/123?focusedCommentId=%s" % cid}}
    if parent is not None:
        c["parentCommentId"] = str(parent)
    if inline:
        c["resolutionStatus"] = "open"
        c["properties"] = {"inlineOriginalSelection": "selected text", "inlineMarkerRef": "m1"}
    return c

def listing(results, next_path=None):
    out = {"results": results, "_links": {"base": "https://fixture.atlassian.net/wiki"}}
    if next_path:
        out["_links"]["next"] = next_path
    return out

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass

    def send(self, status, body, ctype="application/json", extra=None):
        data = body if isinstance(body, bytes) else (json.dumps(body).encode() if not isinstance(body, str) else body.encode())
        self.send_response(status)
        if data:
            self.send_header("Content-Type", ctype)
        if status != 204:
            # RFC 9110 8.6: a 204 carries no Content-Length.
            self.send_header("Content-Length", str(len(data)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if data:
            self.wfile.write(data)

    def record(self, method, path, raw_query, query, body_bytes, prefix):
        try:
            body = json.loads(body_bytes) if body_bytes else None
        except Exception:
            body = body_bytes.decode("utf-8", "replace")
        rec = {"method": method, "path": path, "raw_query": raw_query,
               "query": {k: v for k, v in query.items()}, "gateway_prefix": prefix,
               "headers": {k.lower(): v for k, v in self.headers.items()
                           if k.lower() in ("authorization", "accept", "content-type", "user-agent", "x-atlassian-token",
                                            "content-length", "transfer-encoding")},
               "body": body, "body_len": len(body_bytes)}
        with LOCK:
            STATE["last"] = rec
            STATE["counters"]["requests"] += 1

    def bump(self, key):
        with LOCK:
            STATE["counters"][key] += 1
            return STATE["counters"][key]

    def handle_any(self, method):
        parts = urlsplit(self.path)
        path, raw_query = parts.path, parts.query
        query = parse_qs(raw_query, keep_blank_values=True)
        length = int(self.headers.get("Content-Length") or 0)
        body_bytes = self.rfile.read(length) if length else b""
        if path == "/__last":
            with LOCK:
                return self.send(200, STATE["last"] or {})
        if path == "/__counters":
            with LOCK:
                return self.send(200, STATE["counters"])
        if path == "/__reset":
            with LOCK:
                for k in STATE["counters"]:
                    STATE["counters"][k] = 0
            return self.send(200, {"ok": True})
        prefix = ""
        if path.startswith("/ex/confluence/"):
            rest = path[len("/ex/confluence/"):]
            cid, _, tail = rest.partition("/")
            prefix = "/ex/confluence/" + cid
            path = "/" + tail
        self.record(method, path, raw_query, query, body_bytes, prefix)
        if self.headers.get("Authorization") != EXPECTED_AUTH:
            return self.send(401, "Basic authentication with passwords is deprecated. For more information, see: https://developer.atlassian.com/cloud/confluence/deprecation-notice-basic-auth/", "text/plain")
        q = lambda k, d=None: (query.get(k) or [d])[0]
        body = None
        if body_bytes:
            try:
                body = json.loads(body_bytes)
            except Exception:
                body = None
        v2 = "/wiki/api/v2"
        v1 = "/wiki/rest/api"

        # ---- v1 -------------------------------------------------------------
        if method == "GET" and path == v1 + "/user/current":
            return self.send(200, {"type": "known", "accountId": "5b10a2844c20165700ede21g", "accountType": "atlassian",
                                   "email": "e2e@example.com", "publicName": "E2E", "displayName": "E2E Tester",
                                   "timeZone": "UTC", "isExternalCollaborator": False})
        if method == "GET" and path == v1 + "/search":
            cql = q("cql", "")
            if "user." in cql:
                return self.send(400, {"statusCode": 400, "data": {"authorized": False, "valid": True, "errors": [], "successful": False},
                                       "message": "Could not parse cql : " + cql, "reason": "Bad Request"})
            limit = int(q("limit", "25"))
            results = [
                {"content": {"id": "123", "type": "page", "status": "current", "title": "Kubernetes Runbook",
                             "_expandable": {"space": "/rest/api/space/ENG"},
                             "_links": {"webui": "/spaces/ENG/pages/123/Kubernetes+Runbook"}},
                 "title": "Kubernetes @@@hl@@@Runbook@@@endhl@@@",
                 "excerpt": "How to @@@hl@@@upgrade@@@endhl@@@ the   cluster",
                 "url": "/spaces/ENG/pages/123/Kubernetes+Runbook",
                 "resultGlobalContainer": {"title": "Engineering", "displayUrl": "/spaces/ENG"},
                 "entityType": "content", "lastModified": "2026-08-30T10:00:00.000Z"},
                {"content": {"id": "777", "type": "attachment", "status": "current", "title": "spec.pdf",
                             "_links": {"webui": "/pages/viewpageattachments.action?pageId=123&preview=%2F123%2F777%2Fspec.pdf"}},
                 "title": "spec.pdf", "excerpt": "", "url": "/download/attachments/123/spec.pdf",
                 "resultGlobalContainer": {"title": "Engineering", "displayUrl": "/spaces/ENG"},
                 "entityType": "content", "lastModified": "2026-08-01T00:00:00.000Z"}]
            return self.send(200, {"results": results, "start": 0, "limit": limit, "size": 2, "totalSize": 42,
                                   "cqlQuery": cql, "searchDuration": 12,
                                   "_links": {"base": "https://fixture.atlassian.net/wiki", "context": "/wiki",
                                              "next": "/rest/api/search?cursor=CUR2&cql=" + quote(cql), "self": "https://fixture.atlassian.net/wiki/rest/api/search"}})
        if method == "POST" and path == v1 + "/content/123/label":
            self.bump("label")
            names = [x.get("name") for x in (body or [])]
            return self.send(200, {"results": [{"prefix": "global", "name": n, "id": str(i + 1), "label": n} for i, n in enumerate(names)],
                                   "start": 0, "limit": 200, "size": len(names), "_links": {}})

        # ---- v2: pages ------------------------------------------------------
        if method == "GET" and path == v2 + "/pages":
            title = q("title", "Kubernetes Runbook")
            return self.send(200, listing([page(123, title)], "/wiki/api/v2/pages?cursor=NEXT%2Bx%2Fy%3D&limit=25"))
        if method == "POST" and path == v2 + "/pages":
            self.bump("post_pages")
            if len(body_bytes) > 5 * 1024 * 1024:
                return self.send(413, {"errors": [{"status": 413, "code": "PAYLOAD_TOO_LARGE", "title": "Request entity too large"}]})
            if (body or {}).get("title") == "dup":
                return self.send(400, {"errors": [{"status": 400, "code": "INVALID_REQUEST_PARAMETER",
                                                   "title": "A page with this title already exists: A page already exists with the same TITLE in this space"}]})
            return self.send(200, page(900, (body or {}).get("title", ""), version=1, body={"storage": (body or {}).get("body")}))
        if path.startswith(v2 + "/pages/"):
            rest = path[len(v2 + "/pages/"):]
            pid, _, sub = rest.partition("/")
            if sub == "" and method == "GET":
                if pid == "404":
                    return self.send(404, {"errors": [{"status": 404, "code": "NOT_FOUND", "title": "Page not found", "detail": None}]})
                if pid == "401":
                    return self.send(401, "Basic authentication with passwords is deprecated.", "text/plain")
                if pid == "403":
                    return self.send(403, {"errors": [{"status": 403, "code": "FORBIDDEN", "title": "Forbidden", "detail": "no view permission"}]})
                if pid == "500":
                    return self.send(500, "<!DOCTYPE html><html><body><h1>Oops</h1><p>Something went wrong</p></body></html>", "text/html")
                if pid == "429":
                    n = self.bump("429")
                    if n == 1:
                        return self.send(429, {"errors": [{"status": 429, "code": "TOO_MANY_REQUESTS", "title": "Rate limited"}]},
                                         extra={"Retry-After": "1", "X-RateLimit-Reason": "user-limit", "X-RateLimit-Remaining": "0"})
                if pid == "4290":
                    return self.send(429, {"errors": [{"status": 429, "code": "TOO_MANY_REQUESTS", "title": "Rate limited"}]},
                                     extra={"Retry-After": "120", "RateLimit-Reason": "tenant-limit"})
                if pid not in ("123", "124", "126", "429"):
                    return self.send(404, {"errors": [{"status": 404, "code": "NOT_FOUND", "title": "Page not found"}]})
                fmt = q("body-format", "storage")
                version = int(q("version", "0") or 0)
                if pid == "124":
                    body = {"storage": {"representation": "storage", "value": LONG_STORAGE}}
                    return self.send(200, page(124, "Long page", body=body))
                if version:
                    body = {"storage": {"representation": "storage", "value": "<p>Old body v%d</p>" % version}}
                    return self.send(200, page(pid, "Kubernetes Runbook", version=version, body=body))
                if fmt == "atlas_doc_format":
                    body = {"atlas_doc_format": {"representation": "atlas_doc_format", "value": json.dumps(PAGE_ADF)}}
                elif fmt == "view":
                    body = {"view": {"representation": "view", "value": "<h1>Title</h1><p>Rendered view</p>"}}
                else:
                    body = {"storage": {"representation": "storage", "value": PAGE_STORAGE}}
                return self.send(200, page(pid, "Kubernetes Runbook", body=body, labels=q("include-labels") == "true"))
            if sub == "" and method == "PUT":
                self.bump("put")
                if pid == "404":
                    return self.send(404, {"errors": [{"status": 404, "code": "NOT_FOUND", "title": "Page not found"}]})
                need = 9 if pid == "126" else 8
                number = ((body or {}).get("version") or {}).get("number")
                if number != need:
                    return self.send(409, {"errors": [{"status": 409, "code": "CONFLICT",
                                                       "title": "Version must be incremented on update. Current version is: %d" % (need - 1)}]})
                return self.send(200, page(pid, (body or {}).get("title", ""), version=number, body={"storage": (body or {}).get("body")}))
            if sub == "" and method == "DELETE":
                self.bump("delete")
                if pid == "404":
                    return self.send(404, {"errors": [{"status": 404, "code": "NOT_FOUND", "title": "Page not found"}]})
                return self.send(204, b"")
            if pid == "404":
                return self.send(404, {"errors": [{"status": 404, "code": "NOT_FOUND", "title": "Page not found"}]})
            if sub == "children" and method == "GET":
                return self.send(200, listing([page(124, "Child A", child_position=0), page(125, "Child B", child_position=1)],
                                              "/wiki/api/v2/pages/123/children?cursor=KIDS2&limit=25"))
            if sub == "footer-comments" and method == "GET":
                return self.send(200, listing([comment(1, "<p>First <em>comment</em></p>"), comment(2, "<p>Reply</p>", parent=1)],
                                              "/wiki/api/v2/pages/123/footer-comments?cursor=CMT2"))
            if sub == "inline-comments" and method == "GET":
                return self.send(200, listing([comment(3, "<p>Inline note</p>", inline=True)]))
            if sub == "labels" and method == "GET":
                return self.send(200, listing([{"id": "1", "name": "runbook", "prefix": "global"}, {"id": "2", "name": "k8s", "prefix": "global"}],
                                              "/wiki/api/v2/pages/123/labels?cursor=LBL2"))
            if sub == "attachments" and method == "GET":
                dl = "/download/attachments/123/spec.pdf?version=1&modificationDate=1&cacheVersion=1&api=v2"
                return self.send(200, listing([{"id": "att777", "status": "current", "title": "spec.pdf", "createdAt": "2026-08-01T00:00:00.000Z",
                                                "pageId": "123", "mediaType": "application/pdf", "mediaTypeDescription": "PDF Document",
                                                "comment": "design", "fileId": "f1", "fileSize": 12345,
                                                "webuiLink": "/pages/viewpageattachments.action?pageId=123&preview=%2F123%2F777%2Fspec.pdf",
                                                "downloadLink": dl,
                                                "version": {"number": 1, "authorId": "5b10a2844c20165700ede21g", "createdAt": "2026-08-01T00:00:00.000Z"},
                                                "_links": {"webui": "/pages/viewpageattachments.action?pageId=123&preview=%2F123%2F777%2Fspec.pdf", "download": dl}}],
                                              "/wiki/api/v2/pages/123/attachments?cursor=ATT2"))
        # ---- v2: spaces -----------------------------------------------------
        if method == "GET" and path == v2 + "/spaces":
            keys = [k for k in (q("keys", "") or "").split(",") if k]
            results = [space(k) for k in (keys or list(SPACES)) if k in SPACES]
            return self.send(200, listing(results, None if keys else "/wiki/api/v2/spaces?cursor=SP2&limit=25"))
        if method == "GET" and path.startswith(v2 + "/spaces/"):
            sid = path[len(v2 + "/spaces/"):]
            for key, (num, _) in SPACES.items():
                if str(num) == sid:
                    return self.send(200, space(key))
            return self.send(404, {"errors": [{"status": 404, "code": "NOT_FOUND", "title": "Space not found"}]})
        # ---- v2: comments ---------------------------------------------------
        if method == "POST" and path == v2 + "/footer-comments":
            self.bump("comment")
            c = comment(555, ((body or {}).get("body") or {}).get("value", ""))
            c["pageId"] = (body or {}).get("pageId")
            if (body or {}).get("parentCommentId"):
                c["parentCommentId"] = body["parentCommentId"]
            return self.send(201, c)
        return self.send(404, {"errors": [{"status": 404, "code": "NOT_FOUND", "title": "No route " + method + " " + path}]})

    def do_GET(self): self.handle_any("GET")
    def do_POST(self): self.handle_any("POST")
    def do_PUT(self): self.handle_any("PUT")
    def do_DELETE(self): self.handle_any("DELETE")

ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
EOF
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "http://127.0.0.1:${FIXTURE_PORT}/__counters" && break
  sleep 0.2
done

COMMON=(--env ATLASSIAN_SITE=fixture --env ATLASSIAN_EMAIL=e2e@example.com
        --env "ATLASSIAN_BASE_URL=http://127.0.0.1:${FIXTURE_PORT}")
mcp_harness_start "${COMMON[@]}" --env ATLASSIAN_API_TOKEN=fixture-token
# Guard instance: no ATLASSIAN_API_TOKEN (missing-secret path).
mcp_harness_start_guard "${COMMON[@]}"
echo "starting writer instance on :${WRITER_PORT} (ALLOW_DELETE + SPACES_FILTER)..."
start_extra "$WRITER_PORT" "${COMMON[@]}" --env ATLASSIAN_API_TOKEN=fixture-token \
  --env CONFLUENCE_ALLOW_DELETE=true --env CONFLUENCE_SPACES_FILTER=ENG,DOCS
echo "starting read-only instance on :${RO_PORT} (READ_ONLY + CLOUD_ID gateway route)..."
start_extra "$RO_PORT" "${COMMON[@]}" --env ATLASSIAN_API_TOKEN=fixture-token \
  --env CONFLUENCE_READ_ONLY=true --env ATLASSIAN_CLOUD_ID=1234abcd-0000-4000-8000-000000000001
WRITER="http://127.0.0.1:${WRITER_PORT}/"
RO="http://127.0.0.1:${RO_PORT}/"
GUARD="http://127.0.0.1:${GUARD_PORT}/"

framework_tests check_auth get_current_user search list_pages get_page get_page_children \
  list_spaces get_space create_page update_page delete_page get_comments add_comment \
  get_labels add_label list_attachments
discovery_tests get_page
skills_tests "$SKILL_NAME" "references/TOOLS.md" "references/CQL.md"

echo "== discovery: credentials block =="
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_json "GET / lists the atlassian-api-token credential" 'r["credentials"][0]["ref"]=="atlassian-api-token" and r["credentials"][0]["env"]=="ATLASSIAN_API_TOKEN"' "$ROOT"
assert_json "GET / reports the token as configured on the primary" 'r["credentials"][0]["status"]=="configured"' "$ROOT"
assert_json "GET / names check_auth as the validator" 'r["credentials"][0]["validate"]=="check_auth"' "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD")
assert_json "GET / reports the token as missing on the guard" 'r["credentials"][0]["status"]=="missing"' "$ROOT"

echo "== check_auth / get_current_user =="
OUT=$(mcp_call check_auth '{}')
assert_sc "check_auth reports status ok" 'sc["status"]=="ok" and sc["route"]=="site"' "$OUT"
assert_sc "check_auth returns the identity" 'sc["account"]["accountId"]=="5b10a2844c20165700ede21g" and sc["email"]=="e2e@example.com"' "$OUT"
assert_sc "check_auth reports the gates" 'sc["read_only"] is False and sc["allow_delete"] is False and sc["spaces_filter"]==[]' "$OUT"
assert_last "Basic auth header is base64(email:token)" 'r["headers"]["authorization"]=="Basic ZTJlQGV4YW1wbGUuY29tOmZpeHR1cmUtdG9rZW4="'
assert_last "User-Agent and Accept headers are set" '"atlassian-confluence-mcp/" in r["headers"]["user-agent"] and r["headers"]["accept"]=="application/json"'
assert_last "check_auth dials GET /wiki/rest/api/user/current" 'r["method"]=="GET" and r["path"]=="/wiki/rest/api/user/current"'

OUT=$(mcp_call_on "$GUARD" check_auth '{}')
assert_contains "check_auth without the secret is a tool error" '"isError":true' "$OUT"
assert_sc "check_auth without the secret reports status missing" 'sc["status"]=="missing"' "$OUT"
assert_contains "missing-secret error names the env var" 'ATLASSIAN_API_TOKEN is not set' "$OUT"
assert_contains "missing-secret error names the secret ref" 'atlassian-api-token' "$OUT"
assert_contains "missing-secret error says where to get a token" 'id.atlassian.com/manage-profile/security/api-tokens' "$OUT"
assert_contains "missing-secret error names the Jira sibling sharing the ref" 'atlassian-jira-mcp' "$OUT"

OUT=$(mcp_call_on "$RO" check_auth '{}')
assert_sc "check_auth on the gateway route reports route=gateway" 'sc["status"]=="ok" and sc["route"]=="gateway" and sc["read_only"] is True' "$OUT"
assert_last "gateway route prefixes /ex/confluence/<cloudId>" 'r["gateway_prefix"]=="/ex/confluence/1234abcd-0000-4000-8000-000000000001" and r["path"]=="/wiki/rest/api/user/current"'

OUT=$(mcp_call get_current_user '{}')
assert_sc "get_current_user returns accountId and email" 'sc["accountId"]=="5b10a2844c20165700ede21g" and sc["email"]=="e2e@example.com" and sc["displayName"]=="E2E Tester"' "$OUT"
OUT=$(mcp_call_on "$GUARD" get_current_user '{}')
assert_contains "get_current_user on the guard instance reports the missing secret" 'ATLASSIAN_API_TOKEN is not set' "$OUT"
assert_sc "missing-secret detail is structured (kind not_configured)" 'sc["error"]["kind"]=="not_configured" and "ATLASSIAN_API_TOKEN" in sc["error"]["missing"]' "$OUT"

echo "== search =="
OUT=$(mcp_call search '{"text":"k8s"}')
assert_sc "search(text) returns hits with id/spaceKey/url" 'sc["results"][0]["id"]=="123" and sc["results"][0]["spaceKey"]=="ENG" and sc["results"][0]["url"]=="https://fixture.atlassian.net/wiki/spaces/ENG/pages/123/Kubernetes+Runbook"' "$OUT"
assert_sc "search strips highlight markers and collapses whitespace" 'sc["results"][0]["excerpt"]=="How to upgrade the cluster"' "$OUT"
assert_sc "search reports totalSize, next_cursor and effective limit" 'sc["totalSize"]==42 and sc["next_cursor"]=="CUR2" and sc["limit"]==25 and sc["count"]==2' "$OUT"
assert_sc "search handles attachment hits" 'sc["results"][1]["type"]=="attachment" and sc["results"][1]["title"]=="spec.pdf"' "$OUT"
assert_last "search(text) builds a quoted CQL query" 'r["query"]["cql"]==["text ~ \"k8s\" AND type = page ORDER BY lastmodified DESC"] and r["query"]["excerpt"]==["highlight"]'
OUT=$(mcp_call search '{"cql":"type = page","limit":999}')
assert_sc "search limit 999 is clamped to 100" 'sc["limit"]==100' "$OUT"
assert_last "search sends the clamped limit upstream" 'r["query"]["limit"]==["100"]'
OUT=$(mcp_call search '{"cql":"type = page","limit":-1}')
assert_contains "search limit -1 is refused before dialing" 'non-negative' "$OUT"
assert_contains "search limit -1 is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call search '{"cql":"title ~ \"Größe & \\\"quoted\\\"\" AND type = page"}')
assert_last "search round-trips unicode, & and quotes in CQL" 'r["query"]["cql"]==["title ~ \"Größe & \\\"quoted\\\"\" AND type = page"]'
assert_last "search percent-encodes & and quotes in the raw query" '"%26" in r["raw_query"] and "%22" in r["raw_query"] and "&cql=" not in r["raw_query"].split("cql=")[1]'
OUT=$(mcp_call search '{"text":"\") OR type = attachment --"}')
assert_last "search escapes injection-shaped text inside the CQL string" 'r["query"]["cql"]==["text ~ \"\\\") OR type = attachment --\" AND type = page ORDER BY lastmodified DESC"]'
assert_sc "search with injection-shaped text still succeeds" 'sc["count"]==2' "$OUT"
OUT=$(mcp_call search '{"text":"k8s","space_key":"ENG"}')
assert_last "search space_key wraps the query before ORDER BY" 'r["query"]["cql"]==["(text ~ \"k8s\" AND type = page) AND space = \"ENG\" ORDER BY lastmodified DESC"]'
OUT=$(mcp_call search '{"cql":"type = page","cursor":"abc+def/ghi="}')
assert_last "search passes the cursor through percent-encoded" '"cursor=abc%2Bdef%2Fghi%3D" in r["raw_query"]'
OUT=$(mcp_call search '{"cql":"user.fullname = \"x\""}')
assert_contains "search maps the CQL parse error" 'Could not parse cql' "$OUT"
assert_contains "search CQL error carries the quoting hint" 'quote values (space = \"ENG\")' "$OUT"
assert_sc "search CQL error is structured (400 bad_request)" 'sc["error"]["kind"]=="bad_request" and sc["error"]["status"]==400 and sc["error"]["retryable"] is False' "$OUT"
OUT=$(mcp_call search '{}')
assert_contains "search without cql or text is refused" 'pass `cql`' "$OUT"
OUT=$(mcp_call search '{"cql":"'"$(python3 -c 'print("a"*20000)')"'"}')
assert_contains "search refuses a 20k-char CQL" 'longer than' "$OUT"
OUT=$(mcp_call_on "$WRITER" search '{"text":"k8s"}')
assert_last "spaces filter wraps search in space in (...)" 'r["query"]["cql"]==["(text ~ \"k8s\" AND type = page) AND space in (\"ENG\",\"DOCS\") ORDER BY lastmodified DESC"]'
BEFORE=$(counter requests)
OUT=$(mcp_call_on "$WRITER" search '{"text":"k8s","space_key":"HR"}')
assert_contains "spaces filter refuses a space_key outside the filter" 'outside CONFLUENCE_SPACES_FILTER' "$OUT"
assert_eq "filtered search did not dial upstream" "$BEFORE" "$(counter requests)"

echo "== list_pages =="
OUT=$(mcp_call list_pages '{"space_id":"100","title":"Kubernetes Runbook"}')
assert_sc "list_pages returns the page and decodes next_cursor" 'sc["results"][0]["id"]=="123" and sc["results"][0]["version"]["number"]==7 and sc["next_cursor"]=="NEXT+x/y=" and sc["spaceId"]=="100"' "$OUT"
assert_last "list_pages sends space-id, title, status=current" 'r["query"]["space-id"]==["100"] and r["query"]["title"]==["Kubernetes Runbook"] and r["query"]["status"]==["current"] and r["query"]["limit"]==["25"]'
assert_sc "list_pages builds absolute page urls" 'sc["results"][0]["url"].startswith("https://fixture.atlassian.net/wiki/spaces/ENG/pages/123/")' "$OUT"
OUT=$(mcp_call list_pages '{"space_id":"ENG"}')
assert_sc "list_pages resolves a space key to its id" 'sc["spaceId"]=="100" and sc["spaceKey"]=="ENG"' "$OUT"
OUT=$(mcp_call list_pages '{"limit":0}')
assert_sc "list_pages limit 0 is clamped to 1" 'sc["limit"]==1' "$OUT"
OUT=$(mcp_call list_pages '{"limit":999}')
assert_sc "list_pages limit 999 is clamped to 250" 'sc["limit"]==250' "$OUT"
assert_last "list_pages sends the clamped limit" 'r["query"]["limit"]==["250"]'
OUT=$(mcp_call list_pages '{"sort":"bogus"}')
assert_contains "list_pages rejects an unknown sort" 'sort must be one of' "$OUT"
OUT=$(mcp_call list_pages '{"sort":"-modified-date","status":"trashed"}')
assert_last "list_pages passes sort and status" 'r["query"]["sort"]==["-modified-date"] and r["query"]["status"]==["trashed"]'
OUT=$(mcp_call list_pages '{"status":"bogus"}')
assert_contains "list_pages rejects an unknown status" 'status must be' "$OUT"
OUT=$(mcp_call list_pages '{"space_id":"100","title":"Über 🚀 & co"}')
assert_last "list_pages round-trips a unicode/emoji title" 'r["query"]["title"]==["Über 🚀 & co"]'
OUT=$(mcp_call list_pages '{"space_id":"../../etc"}')
assert_contains "list_pages refuses a traversal-shaped space id" 'neither a numeric space id nor a space key' "$OUT"
OUT=$(mcp_call_on "$WRITER" list_pages '{}')
assert_contains "spaces filter makes list_pages require space_id" 'pass space_id' "$OUT"
OUT=$(mcp_call_on "$WRITER" list_pages '{"space_id":"HR"}')
assert_contains "spaces filter refuses list_pages outside the filter (key)" 'outside CONFLUENCE_SPACES_FILTER' "$OUT"
OUT=$(mcp_call_on "$WRITER" list_pages '{"space_id":"102"}')
assert_contains "spaces filter refuses list_pages outside the filter (numeric id)" 'outside CONFLUENCE_SPACES_FILTER' "$OUT"
OUT=$(mcp_call_on "$WRITER" list_pages '{"space_id":"DOCS"}')
assert_sc "spaces filter allows a listed space" 'sc["spaceId"]=="101"' "$OUT"

echo "== get_page =="
OUT=$(mcp_call get_page '{"page_id":"123"}')
assert_sc "get_page returns metadata, version and labels" 'sc["id"]=="123" and sc["version"]["number"]==7 and sc["labels"][0]["name"]=="runbook" and sc["format"]=="text" and sc["body_truncated"] is False' "$OUT"
assert_last "get_page requests storage with labels and version" 'r["path"]=="/wiki/api/v2/pages/123" and r["query"]["body-format"]==["storage"] and r["query"]["include-labels"]==["true"] and r["query"]["include-version"]==["true"]'
assert_sc "storage→text renders headings and entities" '"# Title & More" in sc["body"] and "## List" in sc["body"]' "$OUT"
assert_sc "storage→text renders inline marks and links" '"**bold**" in sc["body"] and "[link](https://example.com/x)" in sc["body"]' "$OUT"
assert_sc "storage→text renders lists and escaped text" '"- one" in sc["body"] and "- two <three>" in sc["body"]' "$OUT"
assert_sc "storage→text renders tables as pipe tables" '"| Col A | Col B |" in sc["body"] and "| --- | --- |" in sc["body"] and "| 1 | 2 |" in sc["body"]' "$OUT"
assert_sc "storage→text renders code macros as fences" '"```python" in sc["body"] and "print(\"hi <world>\")" in sc["body"]' "$OUT"
assert_sc "storage→text marks macros and keeps rich-text bodies" '"[macro:toc]" in sc["body"] and "[macro:info]" in sc["body"] and "Note body" in sc["body"]' "$OUT"
assert_sc "storage→text renders images and tasks" '"![diagram.png]" in sc["body"] and "- [x] done thing" in sc["body"]' "$OUT"
assert_sc "storage→text keeps multibyte text and decodes &nbsp;" '"Emoji ☃ 日本語 end" in sc["body"]' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"https://fixture.atlassian.net/wiki/spaces/ENG/pages/123/Kubernetes+Runbook"}')
assert_last "get_page extracts the id from a page URL" 'r["path"]=="/wiki/api/v2/pages/123"'
OUT=$(mcp_call get_page '{"page_id":"https://fixture.atlassian.net/wiki/pages/viewpage.action?pageId=123"}')
assert_last "get_page extracts the id from a legacy pageId URL" 'r["path"]=="/wiki/api/v2/pages/123"'
OUT=$(mcp_call get_page '{"page_id":"123","format":"storage"}')
assert_sc "get_page format=storage returns raw XHTML" '"<ac:structured-macro ac:name=\"code\">" in sc["body"] and sc["format"]=="storage"' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"123","format":"atlas_doc_format"}')
assert_last "get_page format=atlas_doc_format requests ADF" 'r["query"]["body-format"]==["atlas_doc_format"]'
assert_sc "ADF→text renders heading, marks, link" '"# ADF Title" in sc["body"] and "**bold**" in sc["body"] and "[ link](https://example.com/adf)" in sc["body"]' "$OUT"
assert_sc "ADF→text renders code, table, list" '"```rust" in sc["body"] and "| H1 | H2 |" in sc["body"] and "| a | b |" in sc["body"] and "- item" in sc["body"]' "$OUT"
assert_sc "ADF result carries the parsed ADF" 'sc["adf"]["type"]=="doc"' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"123","format":"view"}')
assert_sc "get_page format=view returns rendered HTML" '"<h1>Title</h1>" in sc["body"]' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"123","version":3}')
assert_last "get_page version=3 requests that version" 'r["query"]["version"]==["3"]'
assert_sc "get_page version=3 returns the historical body" '"Old body v3" in sc["body"] and sc["version"]["number"]==3' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"123","version":0}')
assert_contains "get_page rejects version 0" 'positive version number' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"124","max_chars":5}')
assert_sc "get_page max_chars is clamped to 1000 and truncates on a char boundary" 'sc["body_truncated"] is True and sc["body_chars"]==5000 and sc["body"].endswith("…[truncated]") and len(sc["body"])==1000+len("…[truncated]")' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"124","max_chars":6000}')
assert_sc "get_page max_chars above the body length does not truncate" 'sc["body_truncated"] is False and len(sc["body"])==5000' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"abc"}')
assert_contains "get_page rejects a non-numeric id before dialing" 'neither a numeric page id nor a page URL' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"123","format":"pdf"}')
assert_contains "get_page rejects an unknown format" 'format must be one of' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"404"}')
assert_contains "get_page 404 is a tool error with the v2 hint" 'is visible to this account: it does not exist, is trashed or a draft' "$OUT"
assert_sc "get_page 404 detail is structured" 'sc["error"]["kind"]=="not_found" and sc["error"]["status"]==404 and "NOT_FOUND" in sc["error"]["codes"]' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"401"}')
assert_contains "401 surfaces the upstream text" 'Basic authentication with passwords is deprecated' "$OUT"
assert_contains "401 carries the credential remediation" 'atlassian-api-token' "$OUT"
assert_contains "401 says not to retry" 'Do not retry with the same credentials' "$OUT"
assert_sc "401 detail is structured (unauthorized)" 'sc["error"]["kind"]=="unauthorized" and sc["error"]["retryable"] is False' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"403"}')
assert_contains "403 explains permissions" 'Permission denied' "$OUT"
assert_contains "403 surfaces the upstream detail" 'no view permission' "$OUT"
curl -s -o /dev/null "http://127.0.0.1:${FIXTURE_PORT}/__reset"
OUT=$(mcp_call get_page '{"page_id":"429"}')
assert_sc "429 with a short Retry-After is retried once and succeeds" 'sc["id"]=="429"' "$OUT"
assert_eq "the fixture saw exactly two attempts" "2" "$(counter 429)"
OUT=$(mcp_call get_page '{"page_id":"4290"}')
assert_contains "429 with a long Retry-After is surfaced" 'Rate limited (tenant-limit)' "$OUT"
assert_sc "429 detail carries retry_after_seconds" 'sc["error"]["kind"]=="rate_limited" and sc["error"]["retry_after_seconds"]==120 and sc["error"]["retryable"] is True' "$OUT"
OUT=$(mcp_call get_page '{"page_id":"500"}')
assert_contains "500 with an HTML body is mapped, not dumped" 'HTML page instead of JSON' "$OUT"
assert_sc "500 HTML detail is structured" 'sc["error"]["kind"]=="html_response"' "$OUT"
OUT=$(mcp_call_on "$GUARD" get_page '{"page_id":"123"}')
assert_contains "get_page on the guard instance reports the missing secret" 'ATLASSIAN_API_TOKEN is not set' "$OUT"

echo "== get_page_children =="
OUT=$(mcp_call get_page_children '{"page_id":"123","sort":"child-position"}')
assert_sc "get_page_children lists children with positions" 'sc["count"]==2 and sc["results"][0]["id"]=="124" and sc["results"][0]["childPosition"]==0 and sc["parentId"]=="123" and sc["next_cursor"]=="KIDS2"' "$OUT"
assert_last "get_page_children passes sort and limit" 'r["path"]=="/wiki/api/v2/pages/123/children" and r["query"]["sort"]==["child-position"] and r["query"]["limit"]==["25"]'
OUT=$(mcp_call get_page_children '{"page_id":"123","sort":"size"}')
assert_contains "get_page_children rejects an unknown sort" 'sort must be one of' "$OUT"
OUT=$(mcp_call get_page_children '{"page_id":"404"}')
assert_contains "get_page_children 404 is mapped" 'No page with id 404 is visible to this account' "$OUT"
OUT=$(mcp_call get_page_children '{"page_id":"123","cursor":"'"$(python3 -c 'print("c"*5000)')"'"}')
assert_contains "get_page_children refuses a 5000-char cursor" 'longer than' "$OUT"

echo "== list_spaces / get_space =="
OUT=$(mcp_call list_spaces '{}')
assert_sc "list_spaces lists spaces with ids, keys and homepage" 'sc["count"]==3 and sc["results"][0]["key"]=="ENG" and sc["results"][0]["id"]=="100" and sc["results"][0]["homepageId"]=="1000" and sc["next_cursor"]=="SP2"' "$OUT"
assert_sc "list_spaces builds absolute space urls" 'sc["results"][0]["url"]=="https://fixture.atlassian.net/wiki/spaces/ENG" and sc["results"][0]["description"]=="Engineering space"' "$OUT"
assert_last "list_spaces asks for plain descriptions and status=current" 'r["query"]["description-format"]==["plain"] and r["query"]["status"]==["current"] and "keys" not in r["query"]'
OUT=$(mcp_call list_spaces '{"keys":["ENG","DOCS"],"type":"global","status":"archived","limit":999}')
assert_sc "list_spaces filters by keys" 'sc["count"]==2 and sc["keys"]==["ENG","DOCS"] and sc["limit"]==250' "$OUT"
assert_last "list_spaces joins keys with a comma and passes type/status" 'r["query"]["keys"]==["ENG,DOCS"] and r["query"]["type"]==["global"] and r["query"]["status"]==["archived"] and r["query"]["limit"]==["250"]'
OUT=$(mcp_call list_spaces '{"type":"secret"}')
assert_contains "list_spaces rejects an unknown type" 'type must be one of' "$OUT"
OUT=$(mcp_call list_spaces '{"keys":["ENG;DROP TABLE"]}')
assert_contains "list_spaces rejects an injection-shaped key" 'is not a space key' "$OUT"
OUT=$(mcp_call list_spaces "{\"keys\":$(python3 -c 'import json; print(json.dumps(["K%d" % i for i in range(60)]))')}")
assert_contains "list_spaces refuses more than 50 keys" 'at most 50 keys' "$OUT"
OUT=$(mcp_call_on "$WRITER" list_spaces '{}')
assert_sc "spaces filter limits list_spaces to the filtered keys" 'sc["count"]==2 and sc["keys"]==["ENG","DOCS"] and sc["limit"]==25' "$OUT"
assert_last "spaces filter passes its keys to list_spaces" 'r["query"]["keys"]==["ENG,DOCS"] and r["query"]["limit"]==["25"]'
OUT=$(mcp_call_on "$WRITER" list_spaces '{"keys":["HR"]}')
assert_contains "spaces filter refuses keys outside the filter" 'outside CONFLUENCE_SPACES_FILTER' "$OUT"
OUT=$(mcp_call get_space '{"space":"ENG"}')
assert_sc "get_space by key" 'sc["id"]=="100" and sc["key"]=="ENG" and sc["name"]=="Engineering"' "$OUT"
assert_last "get_space by key looks up ?keys=" 'r["path"]=="/wiki/api/v2/spaces" and r["query"]["keys"]==["ENG"] and r["query"]["limit"]==["1"]'
OUT=$(mcp_call get_space '{"space":"101"}')
assert_sc "get_space by numeric id" 'sc["key"]=="DOCS"' "$OUT"
assert_last "get_space by id dials /spaces/{id}" 'r["path"]=="/wiki/api/v2/spaces/101"'
OUT=$(mcp_call get_space '{"space":"999"}')
assert_contains "get_space 404 is mapped" 'No space with id 999' "$OUT"
OUT=$(mcp_call get_space '{"space":"NOPE"}')
assert_contains "get_space unknown key is a clean error" 'no space with key' "$OUT"
OUT=$(mcp_call get_space '{"space":"bad key!"}')
assert_contains "get_space rejects an invalid key" 'neither a numeric space id nor a space key' "$OUT"
OUT=$(mcp_call get_space '{"space":"~5b10a2844c20165700ede21g"}')
assert_last "get_space accepts a personal space key" 'r["query"]["keys"]==["~5b10a2844c20165700ede21g"]'

echo "== create_page =="
MD='# Hello\n\nTom & Jerry <script>alert(1)</script>\n\n```python\nprint(1)\n```\n\n- [x] done\n- [ ] todo\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n[bad](javascript:alert(1)) [ok](https://example.com)\n\n![diagram](diagram.png) ![remote](https://example.com/i.png)'
OUT=$(mcp_call create_page "{\"space_id\":\"100\",\"title\":\"New page\",\"body\":\"$MD\"}")
assert_sc "create_page returns the created page" 'sc["id"]=="900" and sc["created"] is True and sc["spaceKey"]=="ENG" and sc["representation"]=="storage"' "$OUT"
assert_last "create_page posts spaceId/status/title/storage body" 'r["method"]=="POST" and r["path"]=="/wiki/api/v2/pages" and r["body"]["spaceId"]=="100" and r["body"]["status"]=="current" and r["body"]["title"]=="New page" and r["body"]["body"]["representation"]=="storage" and "parentId" not in r["body"]'
assert_last "POST bodies are framed with an exact Content-Length, never chunked" 'r["headers"]["content-type"]=="application/json" and int(r["headers"].get("content-length","-1"))==r["body_len"] and "transfer-encoding" not in r["headers"]'
assert_last "markdown→storage renders headings and escapes text" '"<h1>Hello</h1>" in r["body"]["body"]["value"] and "Tom &amp; Jerry &lt;script&gt;alert(1)&lt;/script&gt;" in r["body"]["body"]["value"]'
assert_last "markdown→storage emits the code macro with CDATA" '"<ac:structured-macro ac:name=\"code\"><ac:parameter ac:name=\"language\">python</ac:parameter><ac:plain-text-body><![CDATA[print(1)]]></ac:plain-text-body></ac:structured-macro>" in r["body"]["body"]["value"]'
assert_last "markdown→storage emits task lists and tables" '"<ac:task-list><ac:task><ac:task-id>1</ac:task-id><ac:task-status>complete</ac:task-status><ac:task-body>done</ac:task-body></ac:task>" in r["body"]["body"]["value"] and "<table><tbody><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></tbody></table>" in r["body"]["body"]["value"]'
assert_last "markdown→storage drops unsafe hrefs and keeps safe ones" '"javascript:" not in r["body"]["body"]["value"] and "<a href=\"https://example.com\">ok</a>" in r["body"]["body"]["value"]'
assert_last "markdown→storage maps images to attachments and urls" '"<ac:image ac:alt=\"diagram\"><ri:attachment ri:filename=\"diagram.png\" /></ac:image>" in r["body"]["body"]["value"] and "<ri:url ri:value=\"https://example.com/i.png\" />" in r["body"]["body"]["value"]'
OUT=$(mcp_call create_page '{"space_id":"ENG","title":"Under home","body":"hi","parent_id":"https://fixture.atlassian.net/wiki/spaces/ENG/pages/123/x"}')
assert_last "create_page resolves the space key and the parent URL" 'r["body"]["spaceId"]=="100" and r["body"]["parentId"]=="123" and r["body"]["body"]["value"]=="<p>hi</p>"'
OUT=$(mcp_call create_page '{"space_id":"100","title":"Ünïcödé 🚀 & co","body":"x"}')
assert_last "create_page round-trips a unicode title" 'r["body"]["title"]=="Ünïcödé 🚀 & co"'
# Control characters XML 1.0 forbids (NUL, ESC, FF, VT, DEL, U+FFFE/F) would
# earn a 400 'Error parsing xhtml' from Confluence; the server strips them
# from titles and bodies (tab/LF/CR survive) so the document stays parseable.
XML_OK='__import__("xml.etree.ElementTree").etree.ElementTree.fromstring("<r xmlns:ac=\"urn:ac\" xmlns:ri=\"urn:ri\">" + r["body"]["body"]["value"] + "</r>") is not None'
# json.dumps escapes C0/DEL as \u00xx (tab/LF/CR as \t \n \r), so a regex over
# the dump finds every forbidden character in every string of the payload.
NO_CTRL='(lambda d: __import__("re").search(r"\\u00[0-7][0-9a-f]|\\ufff[ef]", d) is None)(json.dumps(r["body"]))'
OUT=$(mcp_call create_page '{"space_id":"100","title":"Run\u0000book \u001b[31mred\u001b[0m \ufffe","body":"a\u0000b\u0001c\u007fd \u000b \u000c\n\n```sh\n\u001b[31mERROR\u001b[0m done ]]> \u0000\n```\n\ntab\there `x\u0002y` [l\u0003ink](https://example.com/?a\u0004=1 \"ti\u0005tle\")"}')
assert_sc "create_page with control characters in title and body still succeeds" 'sc["created"] is True' "$OUT"
assert_last "control characters are stripped from the title" 'r["body"]["title"]=="Runbook [31mred[0m"'
assert_last "control characters are stripped from paragraphs (tab kept)" '"<p>abcd</p>" in r["body"]["body"]["value"] and "<p>tab\there <code>xy</code> <a href=\"https://example.com/?a=1\" title=\"title\">link</a></p>" in r["body"]["body"]["value"]'
assert_last "control characters are stripped inside the code macro CDATA" '"<![CDATA[[31mERROR[0m done ]]]]><![CDATA[> ]]>" in r["body"]["body"]["value"]'
assert_last "the recorded create_page payload carries no XML-illegal characters" "$NO_CTRL"
assert_last "the recorded storage body parses as XML" "$XML_OK"
OUT=$(mcp_call create_page '{"space_id":"100","title":"Raw ctl","body":"<p>raw\u0000 &amp; \u001bok</p>","body_format":"storage"}')
assert_last "body_format=storage also drops control characters" 'r["body"]["body"]["value"]=="<p>raw &amp; ok</p>"'
assert_last "body_format=storage payload parses as XML" "$XML_OK"
OUT=$(mcp_call create_page '{"space_id":"100","title":"Wiki ctl","body":"h1. Ti\u0007tle","body_format":"wiki"}')
assert_last "body_format=wiki also drops control characters" 'r["body"]["body"]["value"]=="h1. Title"'
OUT=$(mcp_call create_page '{"space_id":"100","title":"\u0000\u001b \u0001","body":"x"}')
assert_contains "a title that is only control characters is rejected as empty" 'title must not be empty' "$OUT"
OUT=$(mcp_call create_page '{"space_id":"100","title":"Wiki","body":"h1. Title","body_format":"wiki"}')
assert_last "create_page body_format=wiki passes wiki markup through" 'r["body"]["body"]["representation"]=="wiki" and r["body"]["body"]["value"]=="h1. Title"'
OUT=$(mcp_call create_page '{"space_id":"100","title":"Raw","body":"<p>raw &amp; ok</p>","body_format":"storage"}')
assert_last "create_page body_format=storage passes XHTML verbatim" 'r["body"]["body"]["representation"]=="storage" and r["body"]["body"]["value"]=="<p>raw &amp; ok</p>"'
OUT=$(mcp_call create_page '{"space_id":"100","title":"dup","body":"x"}')
assert_contains "create_page duplicate title is mapped with the list_pages hint" 'list_pages(space_id, title)' "$OUT"
assert_contains "create_page duplicate title surfaces the upstream text" 'A page with this title already exists' "$OUT"
OUT=$(mcp_call create_page '{"space_id":"100","title":"   ","body":"x"}')
assert_contains "create_page rejects an empty title" 'title must not be empty' "$OUT"
OUT=$(mcp_call create_page "{\"space_id\":\"100\",\"title\":\"$(python3 -c 'print("t"*300)')\",\"body\":\"x\"}")
assert_contains "create_page rejects a 300-char title" 'at most 255' "$OUT"
OUT=$(mcp_call create_page '{"space_id":"100","title":"x","body":"x","body_format":"html"}')
assert_contains "create_page rejects an unknown body_format" 'body_format must be one of' "$OUT"
BEFORE=$(counter post_pages)
# The transport caps requests at 4 MiB, so an over-limit body can only arise
# from conversion growth: 1 MiB of `&` becomes 5 MiB of `&amp;` in storage.
python3 -c 'import sys; sys.stdout.write("{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"create_page\",\"arguments\":{\"space_id\":\"100\",\"title\":\"big\",\"body\":\"" + "&" * (1024 * 1024) + "\"},\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}")' > "$E2E_TMP/big.json"
OUT=$(curl -sS --max-time 60 -X POST "$MCP_BASE" -H "$CT" -H "$ACCEPT" -H "$PV" -H 'Mcp-Method: tools/call' -H 'Mcp-Name: create_page' --data-binary @"$E2E_TMP/big.json")
assert_contains "create_page refuses a body over 4 MiB after conversion" 'body_too_large' "$OUT"
assert_contains "the oversized-body refusal explains the conversion growth" 'after conversion to storage format' "$OUT"
assert_eq "the oversized body was not sent upstream" "$BEFORE" "$(counter post_pages)"
OUT=$(mcp_call create_page '{"space_id":"HR","title":"x","body":"x"}')
assert_sc "create_page in an allowed space works without a filter" 'sc["spaceKey"]=="HR"' "$OUT"
OUT=$(mcp_call_on "$WRITER" create_page '{"space_id":"HR","title":"x","body":"x"}')
assert_contains "spaces filter refuses create_page outside the filter" 'outside CONFLUENCE_SPACES_FILTER' "$OUT"
BEFORE=$(counter post_pages)
OUT=$(mcp_call_on "$RO" create_page '{"space_id":"100","title":"x","body":"x"}')
assert_contains "read-only gate refuses create_page" 'write tools are disabled (CONFLUENCE_READ_ONLY=true)' "$OUT"
assert_sc "read-only refusal is structured" 'sc["error"]["kind"]=="read_only"' "$OUT"
assert_eq "read-only create_page did not dial upstream" "$BEFORE" "$(counter post_pages)"

echo "== update_page =="
curl -s -o /dev/null "http://127.0.0.1:${FIXTURE_PORT}/__reset"
OUT=$(mcp_call update_page '{"page_id":"123","title":"Renamed"}')
assert_sc "update_page title-only succeeds with version+1" 'sc["updated"] is True and sc["previous_version"]==7 and sc["version"]["number"]==8 and sc["title"]=="Renamed"' "$OUT"
assert_last "update_page PUTs version 8, the new title and the preserved storage body" 'r["method"]=="PUT" and r["path"]=="/wiki/api/v2/pages/123" and r["body"]["version"]["number"]==8 and r["body"]["version"]["minorEdit"] is False and r["body"]["title"]=="Renamed" and r["body"]["status"]=="current" and r["body"]["id"]=="123" and "<ac:structured-macro ac:name=\"code\">" in r["body"]["body"]["value"]'
OUT=$(mcp_call update_page '{"page_id":"123","body":"New **body**","version_message":"via mcp","minor_edit":true,"expected_version":7}')
assert_sc "update_page with expected_version matching succeeds" 'sc["updated"] is True and sc["version"]["number"]==8' "$OUT"
assert_last "update_page sends the converted body, message and minorEdit, keeps the title" 'r["body"]["body"]["value"]=="<p>New <strong>body</strong></p>" and r["body"]["version"]["message"]=="via mcp" and r["body"]["version"]["minorEdit"] is True and r["body"]["title"]=="Kubernetes Runbook"'
assert_last "PUT bodies are framed with an exact Content-Length, never chunked" 'int(r["headers"].get("content-length","-1"))==r["body_len"] and "transfer-encoding" not in r["headers"]'
OUT=$(mcp_call update_page '{"page_id":"123","title":"Ti\u0000tle\u001b","body":"x\u000cy","version_message":"msg\u0000 \u001b[0m"}')
assert_sc "update_page with control characters succeeds" 'sc["updated"] is True' "$OUT"
assert_last "update_page strips control characters from title, body and version message" 'r["body"]["title"]=="Title" and r["body"]["body"]["value"]=="<p>xy</p>" and r["body"]["version"]["message"]=="msg [0m"'
assert_last "the recorded update_page payload carries no XML-illegal characters" "$NO_CTRL"
PUTS=$(counter put)
OUT=$(mcp_call update_page '{"page_id":"123","title":"x","expected_version":6}')
assert_contains "update_page refuses on expected_version mismatch" 'expected_version mismatch: live version is 7' "$OUT"
assert_sc "version mismatch is structured and not retryable" 'sc["error"]["kind"]=="version_mismatch" and sc["error"]["live_version"]==7 and sc["error"]["retryable"] is False' "$OUT"
assert_eq "no PUT was sent on a version mismatch" "$PUTS" "$(counter put)"
OUT=$(mcp_call update_page '{"page_id":"123"}')
assert_contains "update_page without title or body is refused" 'pass a new title, a new body, or both' "$OUT"
OUT=$(mcp_call update_page '{"page_id":"404","title":"x"}')
assert_contains "update_page on a missing page fails at the read step" 'Nothing was written' "$OUT"
OUT=$(mcp_call update_page '{"page_id":"126","title":"x"}')
assert_contains "update_page 409 surfaces the upstream version message" 'Version must be incremented on update. Current version is: 8' "$OUT"
assert_contains "update_page 409 tells the caller to call again" 'call update_page again' "$OUT"
assert_sc "409 detail is structured (conflict, retryable)" 'sc["error"]["kind"]=="conflict" and sc["error"]["status"]==409 and sc["error"]["retryable"] is True' "$OUT"
OUT=$(mcp_call update_page '{"page_id":"123","body":"h1. Wiki","body_format":"wiki"}')
assert_last "update_page body_format=wiki sends a wiki representation" 'r["body"]["body"]["representation"]=="wiki" and r["body"]["body"]["value"]=="h1. Wiki"'
PUTS=$(counter put)
OUT=$(mcp_call_on "$RO" update_page '{"page_id":"123","title":"x"}')
assert_contains "read-only gate refuses update_page" 'write tools are disabled' "$OUT"
assert_eq "read-only update_page did not PUT" "$PUTS" "$(counter put)"

echo "== delete_page =="
curl -s -o /dev/null "http://127.0.0.1:${FIXTURE_PORT}/__reset"
OUT=$(mcp_call delete_page '{"page_id":"123","confirm":true}')
assert_contains "delete_page is refused without CONFLUENCE_ALLOW_DELETE" 'delete_page is disabled (set CONFLUENCE_ALLOW_DELETE=true' "$OUT"
assert_sc "delete gate refusal is structured" 'sc["error"]["kind"]=="delete_disabled"' "$OUT"
OUT=$(mcp_call_on "$WRITER" delete_page '{"page_id":"123"}')
assert_contains "delete_page requires confirm=true" 'pass confirm=true' "$OUT"
OUT=$(mcp_call_on "$WRITER" delete_page '{"page_id":"123","confirm":false}')
assert_contains "delete_page with confirm=false is refused" 'pass confirm=true' "$OUT"
assert_eq "no DELETE was sent while gated" "0" "$(counter delete)"
OUT=$(mcp_call_on "$WRITER" delete_page '{"page_id":"https://fixture.atlassian.net/wiki/spaces/ENG/pages/123/x","confirm":true}')
assert_sc "delete_page with the gate open trashes the page" 'sc["deleted"] is True and sc["id"]=="123" and sc["status"]==204' "$OUT"
assert_last "delete_page sends DELETE /wiki/api/v2/pages/123 with no purge params" 'r["method"]=="DELETE" and r["path"]=="/wiki/api/v2/pages/123" and r["raw_query"]==""'
assert_last "a bodiless DELETE is framed with Content-Length: 0, never chunked" 'r["headers"].get("content-length")=="0" and "transfer-encoding" not in r["headers"] and "content-type" not in r["headers"]'
# Back-to-back DELETEs on one keep-alive connection: the second used to fail
# intermittently (HttpProtocolError) when the first was sent chunked.
for _ in 1 2 3; do
  OUT=$(mcp_call_on "$WRITER" delete_page '{"page_id":"123","confirm":true}')
  assert_sc "repeated delete_page on the keep-alive connection succeeds" 'sc["deleted"] is True' "$OUT"
  OUT=$(mcp_call_on "$WRITER" delete_page '{"page_id":"404","confirm":true}')
  assert_contains "delete_page 404 is mapped" 'already trashed' "$OUT"
done
assert_eq "the fixture saw every DELETE (6 + the first)" "7" "$(counter delete)"
OUT=$(mcp_call_on "$RO" delete_page '{"page_id":"123","confirm":true}')
assert_contains "read-only gate refuses delete_page" 'write tools are disabled' "$OUT"

echo "== get_comments / add_comment =="
OUT=$(mcp_call get_comments '{"page_id":"123"}')
assert_sc "get_comments renders footer comments to text" 'sc["count"]==2 and sc["results"][0]["body"]=="First _comment_" and sc["results"][1]["parentCommentId"]=="1" and sc["kind"]=="footer" and sc["pageId"]=="123" and sc["next_cursor"]=="CMT2"' "$OUT"
assert_last "get_comments dials footer-comments with body-format=storage" 'r["path"]=="/wiki/api/v2/pages/123/footer-comments" and r["query"]["body-format"]==["storage"]'
OUT=$(mcp_call get_comments '{"page_id":"123","kind":"inline","sort":"-created-date","limit":999}')
assert_sc "get_comments kind=inline carries resolution and selection" 'sc["results"][0]["resolutionStatus"]=="open" and sc["results"][0]["inlineOriginalSelection"]=="selected text" and sc["limit"]==250' "$OUT"
assert_last "get_comments inline passes sort and clamped limit" 'r["path"]=="/wiki/api/v2/pages/123/inline-comments" and r["query"]["sort"]==["-created-date"] and r["query"]["limit"]==["250"]'
OUT=$(mcp_call get_comments '{"page_id":"123","kind":"threaded"}')
assert_contains "get_comments rejects an unknown kind" 'kind must be one of' "$OUT"
OUT=$(mcp_call get_comments '{"page_id":"404"}')
assert_contains "get_comments 404 is mapped" 'No page with id 404 is visible to this account' "$OUT"
OUT=$(mcp_call add_comment '{"page_id":"123","body":"Hello **there**"}')
assert_sc "add_comment returns the created comment" 'sc["id"]=="555" and sc["created"] is True and sc["pageId"]=="123" and sc["body"]=="Hello **there**"' "$OUT"
assert_last "add_comment posts pageId and a storage body" 'r["method"]=="POST" and r["path"]=="/wiki/api/v2/footer-comments" and r["body"]["pageId"]=="123" and "parentCommentId" not in r["body"] and r["body"]["body"]["value"]=="<p>Hello <strong>there</strong></p>"'
OUT=$(mcp_call add_comment '{"page_id":"123","reply_to_comment_id":"1","body":"reply"}')
assert_sc "add_comment reply returns parentCommentId" 'sc["parentCommentId"]=="1"' "$OUT"
assert_last "add_comment reply sends parentCommentId and omits pageId" 'r["body"]["parentCommentId"]=="1" and "pageId" not in r["body"]'
OUT=$(mcp_call add_comment '{"body":"orphan"}')
assert_contains "add_comment without a target is refused" 'pass page_id' "$OUT"
OUT=$(mcp_call add_comment '{"page_id":"123","body":"   "}')
assert_contains "add_comment rejects a blank body" 'body must be 1..100000' "$OUT"
OUT=$(mcp_call add_comment "{\"page_id\":\"123\",\"body\":\"$(python3 -c 'print("z"*100001)')\"}")
assert_contains "add_comment rejects a 100001-char body" 'body must be 1..100000' "$OUT"
OUT=$(mcp_call add_comment '{"reply_to_comment_id":"../1","body":"x"}')
assert_contains "add_comment rejects a non-numeric reply id" 'not a numeric comment id' "$OUT"
OUT=$(mcp_call add_comment '{"page_id":"123","body":"日本語 ☃ <b>"}')
assert_last "add_comment escapes and round-trips unicode" 'r["body"]["body"]["value"]=="<p>日本語 ☃ &lt;b&gt;</p>"'
OUT=$(mcp_call add_comment '{"page_id":"123","body":"ctl\u0000 \u001b[1mbold\u001b[0m\u007f"}')
assert_last "add_comment strips control characters from the body" 'r["body"]["body"]["value"]=="<p>ctl [1mbold[0m</p>"'
assert_last "the recorded comment body parses as XML" "$XML_OK"
OUT=$(mcp_call_on "$RO" add_comment '{"page_id":"123","body":"x"}')
assert_contains "read-only gate refuses add_comment" 'write tools are disabled' "$OUT"

echo "== get_labels / add_label =="
OUT=$(mcp_call get_labels '{"page_id":"123","prefix":"global"}')
assert_sc "get_labels lists labels" 'sc["count"]==2 and sc["results"][0]["name"]=="runbook" and sc["results"][1]["name"]=="k8s" and sc["next_cursor"]=="LBL2"' "$OUT"
assert_last "get_labels passes prefix" 'r["path"]=="/wiki/api/v2/pages/123/labels" and r["query"]["prefix"]==["global"]'
OUT=$(mcp_call get_labels '{"page_id":"123","prefix":"mine"}')
assert_contains "get_labels rejects an unknown prefix" 'prefix must be one of' "$OUT"
OUT=$(mcp_call add_label '{"page_id":"123","labels":["Runbook","k8s","runbook"]}')
assert_sc "add_label lowercases, dedupes and returns the labels" 'sc["added"]==["runbook","k8s"] and sc["count"]==2 and sc["labels"][1]["name"]=="k8s" and sc["pageId"]=="123"' "$OUT"
assert_last "add_label posts a global label array to the v1 endpoint" 'r["method"]=="POST" and r["path"]=="/wiki/rest/api/content/123/label" and r["body"]==[{"prefix":"global","name":"runbook"},{"prefix":"global","name":"k8s"}]'
BEFORE=$(counter label)
OUT=$(mcp_call add_label '{"page_id":"123","labels":["release notes"]}')
assert_contains "add_label rejects a label with a space" 'labels must not contain spaces' "$OUT"
OUT=$(mcp_call add_label '{"page_id":"123","labels":["a:b"]}')
assert_contains "add_label rejects forbidden punctuation" 'does not allow' "$OUT"
OUT=$(mcp_call add_label '{"page_id":"123","labels":["run\u0000book"]}')
assert_contains "add_label rejects a control character" 'contains a control character' "$OUT"
OUT=$(mcp_call add_label "{\"page_id\":\"123\",\"labels\":$(python3 -c 'import json; print(json.dumps(["l%d" % i for i in range(21)]))')}")
assert_contains "add_label rejects 21 labels" 'labels must contain 1..20' "$OUT"
OUT=$(mcp_call add_label '{"page_id":"123","labels":[]}')
assert_contains "add_label rejects an empty list" 'labels must contain 1..20' "$OUT"
assert_eq "rejected labels never dialed upstream" "$BEFORE" "$(counter label)"
OUT=$(mcp_call add_label '{"page_id":"123","labels":["日本語-ラベル"]}')
assert_last "add_label passes a unicode label through" 'r["body"][0]["name"]=="日本語-ラベル"'
OUT=$(mcp_call_on "$RO" add_label '{"page_id":"123","labels":["x"]}')
assert_contains "read-only gate refuses add_label" 'write tools are disabled' "$OUT"

echo "== list_attachments =="
OUT=$(mcp_call list_attachments '{"page_id":"123"}')
assert_sc "list_attachments lists files with sizes and types" 'sc["count"]==1 and sc["results"][0]["title"]=="spec.pdf" and sc["results"][0]["mediaType"]=="application/pdf" and sc["results"][0]["fileSize"]==12345 and sc["results"][0]["version"]["number"]==1 and sc["next_cursor"]=="ATT2"' "$OUT"
assert_sc "list_attachments builds an absolute /wiki download url" 'sc["results"][0]["download_url"]=="https://fixture.atlassian.net/wiki/download/attachments/123/spec.pdf?version=1&modificationDate=1&cacheVersion=1&api=v2"' "$OUT"
assert_last "list_attachments defaults to limit 50" 'r["path"]=="/wiki/api/v2/pages/123/attachments" and r["query"]["limit"]==["50"]'
OUT=$(mcp_call list_attachments '{"page_id":"123","media_type":"application/pdf","filename":"spec & co.pdf","limit":999}')
assert_last "list_attachments passes mediaType/filename encoded and clamps the limit" 'r["query"]["mediaType"]==["application/pdf"] and r["query"]["filename"]==["spec & co.pdf"] and r["query"]["limit"]==["250"] and "spec%20%26%20co.pdf" in r["raw_query"]'
OUT=$(mcp_call list_attachments '{"page_id":"404"}')
assert_contains "list_attachments 404 is mapped" 'No page with id 404 is visible to this account' "$OUT"
OUT=$(mcp_call list_attachments '{"page_id":"123","limit":-5}')
assert_contains "list_attachments refuses a negative limit" 'non-negative' "$OUT"

echo "== malformed params =="
OUT=$(mcp_call get_page '{}')
assert_contains "get_page without page_id is a clean error" 'missing field `page_id`' "$OUT"
assert_not_contains "get_page without page_id produces no result" '"structuredContent":{"id"' "$OUT"
OUT=$(mcp_call search '{"limit":"ten"}')
assert_contains "search with a string limit is a clean error" '"isError":true' "$OUT"
OUT=$(mcp_call add_label '{"page_id":"123","labels":"notalist"}')
assert_contains "add_label with a non-array labels is a clean error" 'expected a sequence' "$OUT"
OUT=$(mcp_call create_page '{"space_id":100,"title":"x","body":"y"}')
assert_contains "create_page with a numeric space_id type is a clean error" 'expected a string' "$OUT"
OUT=$(mcp_call get_current_user '{}')
assert_contains "server alive after malformed tool params" '"accountId"' "$OUT"

if [ "${E2E_LIVE:-}" = "1" ]; then
  echo "== live (E2E_LIVE=1) =="
  if [ -n "${ATLASSIAN_SITE:-}" ] && [ -n "${ATLASSIAN_EMAIL:-}" ] && [ -n "${ATLASSIAN_API_TOKEN:-}" ]; then
    start_extra "$LIVE_PORT" --env "ATLASSIAN_SITE=${ATLASSIAN_SITE}" --env "ATLASSIAN_EMAIL=${ATLASSIAN_EMAIL}" \
      --env "ATLASSIAN_API_TOKEN=${ATLASSIAN_API_TOKEN}"
    LIVE="http://127.0.0.1:${LIVE_PORT}/"
    OUT=$(mcp_call_on "$LIVE" get_current_user '{}')
    assert_contains "live get_current_user returns an accountId" '"accountId"' "$OUT"
    OUT=$(mcp_call_on "$LIVE" list_spaces '{"limit":1}')
    assert_contains "live list_spaces returns a space" '"key"' "$OUT"
    OUT=$(mcp_call_on "$LIVE" search '{"cql":"type = page","limit":1}')
    assert_contains "live search returns totalSize" '"totalSize"' "$OUT"
  else
    fail "live cases" "E2E_LIVE=1 but ATLASSIAN_SITE/ATLASSIAN_EMAIL/ATLASSIAN_API_TOKEN are not all exported"
  fi
fi

guard_tests
mcp_harness_report
