#!/usr/bin/env python3
"""Hermetic stand-in for api.notion.com used by scripts/e2e.sh.

    fixture.py <port>

Every route checks `Authorization: Bearer test-token` (else 401 unauthorized,
Notion's exact error shape) and the presence of `Notion-Version` (else 400
missing_version). Replies embed an `_echo` object (method, path, parsed query,
selected headers, parsed body) so the suite can assert clamping, encoding and
version headers; where a tool does not surface the raw response, the fixture
folds the interesting request facts into fields the tool does surface
(titles, block types, markdown text).

Special ids drive error paths: an id ending in 0401/0403/0404/0409/0429/0500
answers with that status in Notion's error format.

Control routes (no auth): GET /_log lists "METHOD path" of every API request
since the last GET /_reset.
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

TOKEN = "test-token"
MARKDOWN_VERSION = "2026-03-11"

PAGE_ID = "11111111-2222-3333-4444-555555555555"
PAGE_B_ID = "66666666-7777-8888-9999-aaaaaaaaaaaa"
DS_ID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
DB_ID = "dbdbdbdb-dbdb-dbdb-dbdb-dbdbdbdbdbdb"
BOT_ID = "b0b0b0b0-b0b0-b0b0-b0b0-b0b0b0b0b0b0"
USER_ID = "01234567-89ab-cdef-0123-456789abcdef"

LOG = []
LOG_LOCK = threading.Lock()


def rich(text):
    return [{"type": "text", "text": {"content": text, "link": None}, "plain_text": text, "href": None}]


def page_object(page_id=PAGE_ID, title="Roadmap", extra=None, in_trash=False):
    props = {
        "Name": {"id": "title", "type": "title", "title": rich(title)},
        "Status": {"id": "s1", "type": "select", "select": {"id": "o1", "name": "In progress", "color": "blue"}},
        "Due": {"id": "d1", "type": "date", "date": {"start": "2026-09-30", "end": None, "time_zone": None}},
        "Project": {"id": "r1", "type": "relation", "relation": [{"id": PAGE_B_ID}], "has_more": False},
        "Points": {"id": "n1", "type": "number", "number": 3},
        "Done": {"id": "c1", "type": "checkbox", "checkbox": False},
        "Tags": {"id": "m1", "type": "multi_select", "multi_select": [{"name": "alpha"}, {"name": "beta ☃"}]},
        "Owner": {"id": "p1", "type": "people", "people": [{"object": "user", "id": USER_ID, "name": "Ada"}]},
        "Estimate": {"id": "f1", "type": "formula", "formula": {"type": "number", "number": 6}},
        "Total": {"id": "ru1", "type": "rollup", "rollup": {"type": "number", "number": 9, "function": "sum"}},
        "Link": {"id": "u1", "type": "url", "url": "https://example.com/x?a=1&b=2"},
        "Ref": {"id": "id1", "type": "unique_id", "unique_id": {"prefix": "TASK", "number": 42}},
    }
    if extra:
        props.update(extra)
    return {
        "object": "page",
        "id": page_id,
        "created_time": "2026-01-01T00:00:00.000Z",
        "last_edited_time": "2026-09-01T12:00:00.000Z",
        "in_trash": in_trash,
        "icon": {"type": "emoji", "emoji": "🚀"},
        "parent": {"type": "data_source_id", "data_source_id": DS_ID},
        "url": "https://www.notion.so/Roadmap-" + page_id.replace("-", ""),
        "properties": props,
    }


def data_source_object():
    return {
        "object": "data_source",
        "id": DS_ID,
        "title": rich("Tasks"),
        "url": "https://www.notion.so/" + DS_ID.replace("-", ""),
        "in_trash": False,
        "parent": {"type": "database_id", "database_id": DB_ID},
        "database_parent": {"type": "database_id", "database_id": DB_ID},
        "properties": {
            "Task": {"id": "title", "name": "Task", "type": "title", "title": {}},
            "Status": {"id": "s1", "name": "Status", "type": "select",
                       "select": {"options": [{"name": "Todo"}, {"name": "In progress"}, {"name": "Done"}]}},
            "Tags": {"id": "m1", "name": "Tags", "type": "multi_select",
                     "multi_select": {"options": [{"name": "alpha"}, {"name": "beta ☃"}]}},
            "Due": {"id": "d1", "name": "Due", "type": "date", "date": {}},
            "Points": {"id": "n1", "name": "Points", "type": "number", "number": {"format": "number"}},
            "Done": {"id": "c1", "name": "Done", "type": "checkbox", "checkbox": {}},
            "Project": {"id": "r1", "name": "Project", "type": "relation",
                        "relation": {"data_source_id": DS_ID, "type": "single_property"}},
            "Estimate": {"id": "f1", "name": "Estimate", "type": "formula", "formula": {"expression": "prop(\"Points\") * 2"}},
            "Total": {"id": "ru1", "name": "Total", "type": "rollup",
                      "rollup": {"function": "sum", "relation_property_name": "Project", "rollup_property_name": "Points"}},
            "Attachments": {"id": "fi1", "name": "Attachments", "type": "files", "files": {}},
            "Notes": {"id": "t1", "name": "Notes", "type": "rich_text", "rich_text": {}},
            "Owner": {"id": "p1", "name": "Owner", "type": "people", "people": {}},
            "Link": {"id": "u1", "name": "Link", "type": "url", "url": {}},
        },
    }


def fixture_markdown(version):
    lines = ["# Fixture page (Notion-Version %s)" % version, "", "- [ ] write tests", "- [x] build", "",
             "<callout icon=\"💡\">Callouts survive</callout>", ""]
    for i in range(60):
        lines.append("Line %02d ☃☃☃☃☃☃☃☃☃☃ snow ☃☃☃☃☃☃☃☃☃☃ more snow ☃☃☃☃☃☃☃☃☃☃" % i)
    return "\n".join(lines)


def dumps(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False)


def response_props(props):
    """Request property-value objects -> the shape Notion returns (adds `type`,
    `plain_text` on rich text runs)."""
    out = {}
    for name, value in (props or {}).items():
        if not isinstance(value, dict) or not value:
            out[name] = value
            continue
        kind = next(iter(value.keys()))
        inner = value[kind]
        if kind in ("title", "rich_text") and isinstance(inner, list):
            inner = [dict(run, plain_text=run.get("text", {}).get("content", "")) for run in inner]
        out[name] = {"id": name.lower(), "type": kind, kind: inner}
    return out


def notion_error(status, code, message, extra=None):
    body = {"object": "error", "status": status, "code": code, "message": message, "request_id": "req-fixture"}
    if extra:
        body.update(extra)
    return status, body, {}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    # -- plumbing ----------------------------------------------------------
    def _send(self, status, body, headers=None):
        data = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(data)

    def _read_body(self):
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            raw = b""
            while True:
                size = int(self.rfile.readline().strip().split(b";")[0] or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    break
                raw += self.rfile.read(size)
                self.rfile.readline()
        else:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
        try:
            return json.loads(raw.decode("utf-8")) if raw else None
        except ValueError:
            return raw.decode("utf-8", "replace")

    def _echo(self, method, path, query, body):
        return {
            "method": method,
            "path": path,
            "query": query,
            "headers": {
                "notion-version": self.headers.get("Notion-Version"),
                "content-type": self.headers.get("Content-Type"),
                "user-agent": self.headers.get("User-Agent"),
                "authorization_present": bool(self.headers.get("Authorization")),
            },
            "body": body,
        }

    def _handle(self, method):
        parts = urlsplit(self.path)
        path = parts.path
        query = parse_qs(parts.query, keep_blank_values=True)
        if path == "/_log":
            with LOG_LOCK:
                return self._send(200, list(LOG))
        if path == "/_reset":
            with LOG_LOCK:
                LOG.clear()
            return self._send(200, {"ok": True})
        with LOG_LOCK:
            LOG.append("%s %s" % (method, path))
        body = self._read_body() if method in ("POST", "PATCH") else None
        auth = self.headers.get("Authorization")
        if auth != "Bearer " + TOKEN:
            status, payload, headers = notion_error(401, "unauthorized", "API token is invalid.")
            return self._send(status, payload, headers)
        version = self.headers.get("Notion-Version")
        if not version:
            status, payload, headers = notion_error(
                400, "missing_version", "Notion-Version header failed validation: Notion-Version header should be defined, instead was `undefined`.")
            return self._send(status, payload, headers)
        status, payload, headers = self.route(method, path, query, body, version)
        if isinstance(payload, dict):
            payload["_echo"] = self._echo(method, path, query, body)
        self._send(status, payload, headers)

    def do_GET(self):
        self._handle("GET")

    def do_POST(self):
        self._handle("POST")

    def do_PATCH(self):
        self._handle("PATCH")

    # -- routes ------------------------------------------------------------
    def route(self, method, path, query, body, version):
        segments = [s for s in path.split("/") if s]
        if segments[:1] != ["v1"] or len(segments) < 2:
            return notion_error(400, "invalid_request_url", "Invalid request URL")
        resource = segments[1]
        obj_id = segments[2] if len(segments) > 2 else None
        tail = segments[3] if len(segments) > 3 else None

        special = self.special(obj_id)
        if special:
            return special

        if resource == "users" and method == "GET":
            if obj_id == "me":
                return 200, {
                    "object": "user", "id": BOT_ID, "name": "Fixture Bot", "type": "bot",
                    "bot": {"owner": {"type": "workspace", "workspace": True},
                            "workspace_name": "Fixture Workspace", "workspace_id": "ws-1",
                            "workspace_limits": {"max_file_upload_size_in_bytes": 5242880}},
                }, {}
            if obj_id is None:
                page_size = query.get("page_size", ["?"])[0]
                return 200, {
                    "object": "list", "has_more": False, "next_cursor": None, "type": "user",
                    "results": [
                        {"object": "user", "id": USER_ID, "type": "person", "name": "page_size=%s" % page_size,
                         "person": {"email": "ada@example.com"}},
                        {"object": "user", "id": "a2a2a2a2-a2a2-a2a2-a2a2-a2a2a2a2a2a2", "type": "person", "name": "Grace ☃", "person": {}},
                        {"object": "user", "id": BOT_ID, "type": "bot", "name": "Fixture Bot", "bot": {"workspace_name": "Fixture Workspace"}},
                    ],
                }, {}

        if resource == "search" and method == "POST":
            body = body if isinstance(body, dict) else {}
            page_size = body.get("page_size")
            results = [
                page_object(PAGE_ID, "Roadmap"),
                page_object(PAGE_B_ID, "Meeting notes ☃ %s" % dumps(body.get("query"))),
                data_source_object(),
            ]
            if page_size == 1:
                return 200, {"object": "list", "results": results[:1], "has_more": True,
                             "next_cursor": "cursor-page-2", "type": "page_or_data_source",
                             "request_status": {"type": "complete"}}, {}
            return 200, {"object": "list", "results": results, "has_more": False, "next_cursor": None,
                         "type": "page_or_data_source",
                         "request_status": {"type": "incomplete", "incomplete_reason": "query_result_limit_reached"}}, {}

        if resource == "pages":
            if method == "POST":
                body = body if isinstance(body, dict) else {}
                parent = body.get("parent") or {}
                if "page_id" not in parent and "data_source_id" not in parent:
                    return notion_error(400, "validation_error", "body failed validation: body.parent should be defined, instead was `undefined`.")
                props = body.get("properties") or {}
                title = ""
                for value in props.values():
                    if isinstance(value, dict) and "title" in value:
                        title = "".join(item.get("text", {}).get("content", "") for item in value["title"])
                runs = {}
                for name, value in props.items():
                    if isinstance(value, dict) and "rich_text" in value:
                        runs[name] = len(value["rich_text"])
                created = page_object("cccccccc-cccc-cccc-cccc-cccccccccccc", title or "(untitled)")
                created["properties"] = response_props(props)
                created["properties"]["_runs"] = {"type": "rich_text", "rich_text": rich(dumps(runs))}
                created["parent"] = dict(parent)
                created["url"] = "https://www.notion.so/created-%s-markdown=%s" % (version, "markdown" in body)
                return 200, created, {}
            if obj_id and tail == "markdown":
                if version != MARKDOWN_VERSION:
                    return notion_error(400, "invalid_request", "Unsupported request: page markdown requires Notion-Version %s." % MARKDOWN_VERSION)
                if method == "GET":
                    return 200, {"object": "page_markdown", "id": obj_id, "markdown": fixture_markdown(version),
                                 "truncated": False, "unknown_block_ids": []}, {}
                if method == "PATCH":
                    body = body if isinstance(body, dict) else {}
                    updates = (body.get("update_content") or {}).get("content_updates") or []
                    if any(u.get("old_str") == "DUPLICATE" for u in updates):
                        return notion_error(400, "validation_error", "old_str matched 2 times; expected exactly one match.")
                    return 200, {"object": "page_markdown", "id": obj_id,
                                 "markdown": "echo:" + dumps(body),
                                 "truncated": False, "unknown_block_ids": ["deadbeef-0000-0000-0000-000000000001"]}, {}
            if obj_id and method == "GET":
                return 200, page_object(obj_id, "Roadmap fp=%s" % dumps(query.get("filter_properties"))), {}
            if obj_id and method == "PATCH":
                body = body if isinstance(body, dict) else {}
                keys = ",".join(sorted(body.keys()))
                trashed = bool(body.get("in_trash", body.get("archived", False)))
                page = page_object(obj_id, "fields:%s" % keys, in_trash=trashed)
                if "archived" in body:
                    page.pop("in_trash", None)
                    page["archived"] = trashed
                return 200, page, {}

        if resource == "blocks" and obj_id and tail == "children":
            if method == "GET":
                page_size = query.get("page_size", ["?"])[0]
                return 200, {
                    "object": "list", "has_more": True, "next_cursor": "blocks-cursor-2", "type": "block",
                    "results": [
                        {"object": "block", "id": "b1b1b1b1-0000-0000-0000-000000000001", "type": "paragraph", "has_children": False,
                         "paragraph": {"rich_text": rich("page_size=%s cursor=%s" % (page_size, query.get("start_cursor", [None])[0]))}},
                        {"object": "block", "id": "b1b1b1b1-0000-0000-0000-000000000002", "type": "heading_2", "has_children": False,
                         "heading_2": {"rich_text": rich("Plan ☃")}},
                        {"object": "block", "id": "b1b1b1b1-0000-0000-0000-000000000003", "type": "to_do", "has_children": False,
                         "to_do": {"rich_text": rich("ship"), "checked": True}},
                        {"object": "block", "id": "b1b1b1b1-0000-0000-0000-000000000004", "type": "code", "has_children": False,
                         "code": {"rich_text": rich("print(1)"), "language": "python"}},
                        {"object": "block", "id": "b1b1b1b1-0000-0000-0000-000000000005", "type": "child_page", "has_children": True,
                         "child_page": {"title": "Sub page"}},
                    ],
                }, {}
            if method == "PATCH":
                body = body if isinstance(body, dict) else {}
                children = body.get("children") or []
                if len(children) > 100:
                    return notion_error(400, "validation_error", "body failed validation: body.children.length should be ≤ `100`, instead was `%d`." % len(children))
                results = []
                for i, child in enumerate(children):
                    kind = child.get("type", "?")
                    runs = len((child.get(kind) or {}).get("rich_text") or [])
                    results.append({"object": "block", "id": "new-block-%d" % i, "type": kind, "has_children": False, "_runs": runs})
                position = body.get("position") or {"after": body.get("after")}
                results.append({"object": "block", "id": "echo-position", "type": "position:" + dumps(position)})
                results.append({"object": "block", "id": "echo-runs",
                                "type": "runs:" + ",".join(str(r["_runs"]) for r in results if "_runs" in r)})
                return 200, {"object": "list", "results": results, "has_more": False, "next_cursor": None, "type": "block"}, {}

        if resource == "databases" and obj_id and method == "GET":
            return 200, {
                "object": "database", "id": obj_id, "title": rich("Tasks DB"), "is_inline": False, "in_trash": False,
                "url": "https://www.notion.so/" + obj_id.replace("-", ""),
                "parent": {"type": "page_id", "page_id": PAGE_ID},
                "data_sources": [{"id": DS_ID, "name": "Tasks"}, {"id": "a5a5a5a5-a5a5-a5a5-a5a5-a5a5a5a5a5a5", "name": "Archive"}],
            }, {}

        if resource == "data_sources" and obj_id:
            if tail is None and method == "GET":
                source = data_source_object()
                source["id"] = obj_id
                return 200, source, {}
            if tail == "query" and method == "POST":
                body = body if isinstance(body, dict) else {}
                rows = [
                    page_object(PAGE_ID, "page_size=%s" % body.get("page_size")),
                    page_object(PAGE_B_ID, "filter=%s sorts=%s cursor=%s archived=%s fp=%s" % (
                        dumps(body.get("filter")), dumps(body.get("sorts")),
                        body.get("start_cursor"), body.get("is_archived"), dumps(query.get("filter_properties")))),
                ]
                return 200, {"object": "list", "results": rows, "has_more": True, "next_cursor": "rows-cursor-2",
                             "type": "page_or_data_source", "request_status": {"type": "complete"}}, {}

        if resource == "comments":
            if method == "GET":
                block_id = query.get("block_id", [None])[0]
                special = self.special(block_id)
                if special:
                    return special
                return 200, {
                    "object": "list", "has_more": False, "next_cursor": None, "type": "comment",
                    "results": [
                        {"object": "comment", "id": "c0c0c0c0-0000-0000-0000-000000000001", "discussion_id": "d1d1d1d1-0000-0000-0000-000000000001",
                         "parent": {"type": "page_id", "page_id": block_id}, "created_time": "2026-08-01T00:00:00.000Z",
                         "created_by": {"object": "user", "id": USER_ID, "name": "Ada"},
                         "rich_text": rich("block_id=%s page_size=%s" % (block_id, query.get("page_size", ["?"])[0])),
                         "display_name": {"type": "user", "resolved_name": "Ada"}, "attachments": []},
                        {"object": "comment", "id": "c0c0c0c0-0000-0000-0000-000000000002", "discussion_id": "d1d1d1d1-0000-0000-0000-000000000001",
                         "parent": {"type": "page_id", "page_id": block_id}, "created_time": "2026-08-02T00:00:00.000Z",
                         "created_by": {"object": "user", "id": BOT_ID}, "rich_text": rich("Reply ☃"),
                         "display_name": {"type": "integration", "resolved_name": "Fixture Bot"}, "attachments": [{"category": "image"}]},
                    ],
                }, {}
            if method == "POST":
                body = body if isinstance(body, dict) else {}
                if "parent" in body and "discussion_id" in body:
                    return notion_error(400, "validation_error", "body failed validation: body.parent and body.discussion_id are mutually exclusive.")
                parent = body.get("parent") or {}
                for key in ("page_id", "block_id"):
                    special = self.special(parent.get(key))
                    if special:
                        return special
                runs = len(body.get("rich_text") or [])
                return 200, {"object": "comment", "id": "c0c0c0c0-0000-0000-0000-00000000new1",
                             "discussion_id": body.get("discussion_id") or "d1d1d1d1-0000-0000-0000-0000000000n1",
                             "parent": parent or {"type": "discussion", "discussion_id": body.get("discussion_id")},
                             "created_time": "2026-09-02T00:00:00.000Z", "rich_text": rich("runs=%d" % runs)}, {}

        return notion_error(404, "object_not_found", "Could not find object for path %s." % path)

    @staticmethod
    def special(obj_id):
        if not obj_id:
            return None
        if obj_id.endswith("0401"):
            return notion_error(401, "unauthorized", "API token is invalid.")
        if obj_id.endswith("0403"):
            return notion_error(403, "restricted_resource", "Insufficient permissions for this endpoint.")
        if obj_id.endswith("0404"):
            return notion_error(404, "object_not_found", "Could not find page with ID: %s. Make sure the relevant pages and databases are shared with your integration." % obj_id)
        if obj_id.endswith("0409"):
            return notion_error(409, "conflict_error", "Conflict occurred while saving. Please try again.")
        if obj_id.endswith("0429"):
            status, body, _ = notion_error(429, "rate_limited", "You have been rate limited. Please try again in a few minutes.",
                                           {"additional_data": {"rate_limit_reason": "integration_rate_limit"}})
            return status, body, {"Retry-After": "7"}
        if obj_id.endswith("0500"):
            return notion_error(500, "internal_server_error", "Unexpected error occurred.")
        return None


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
