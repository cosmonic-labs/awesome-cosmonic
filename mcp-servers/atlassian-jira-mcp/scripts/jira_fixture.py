#!/usr/bin/env python3
"""Hermetic Jira Cloud REST API v3 impersonator for scripts/e2e.sh.

Usage: jira_fixture.py <port>

Serves both route prefixes — `/rest/api/3/...` (classic tokens against the
site) and `/ex/jira/<cloudId>/rest/api/3/...` (scoped tokens through the
api.atlassian.com gateway) — and echoes what it received (query strings,
bodies, the decoded Basic-auth user) under `fixtureEcho` so the tests can
assert encoding, clamping, defaults and auth headers. Writes that Jira answers
with 204 are recorded and readable at `GET /__fixture/last-write`.

ThreadingHTTPServer: the harness fires 8 concurrent requests.
"""
import base64
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit

VALID_USERS = {
    "e2e@example.com:tok-e2e": "e2e@example.com",
    "cloud@example.com:tok-cloud": "cloud@example.com",
}
ROUTE_RE = re.compile(r"^(?:/ex/jira/(?P<cloud>[^/]+))?/rest/api/3(?P<path>/.*)$")

LOCK = threading.Lock()
LAST_WRITE = {}
LAST_REQUEST = {}


def adf_paragraph(*inline):
    return {"type": "paragraph", "content": list(inline)}


def text(value, marks=None):
    node = {"type": "text", "text": value}
    if marks:
        node["marks"] = marks
    return node


RICH_DESCRIPTION = {
    "type": "doc",
    "version": 1,
    "content": [
        {"type": "heading", "attrs": {"level": 2}, "content": [text("Überblick")]},
        adf_paragraph(
            text("Hello "),
            {"type": "mention", "attrs": {"id": "5b10a2844c20165700ede21g", "text": "@Mia Krystof"}},
            text(" — see "),
            {"type": "inlineCard", "attrs": {"url": "https://example.com/spec"}},
            text(" and "),
            text("docs", marks=[{"type": "link", "attrs": {"href": "https://example.com/docs"}}]),
            text(" "),
            {"type": "emoji", "attrs": {"shortName": ":tada:", "text": "🎉"}},
        ),
        {
            "type": "bulletList",
            "content": [
                {"type": "listItem", "content": [adf_paragraph(text("first item"))]},
                {
                    "type": "listItem",
                    "content": [
                        adf_paragraph(text("second item")),
                        {
                            "type": "orderedList",
                            "attrs": {"order": 3},
                            "content": [
                                {"type": "listItem", "content": [adf_paragraph(text("nested three"))]},
                            ],
                        },
                    ],
                },
            ],
        },
        {"type": "codeBlock", "attrs": {"language": "rust"}, "content": [text("fn main() {}")]},
        {
            "type": "table",
            "content": [
                {
                    "type": "tableRow",
                    "content": [
                        {"type": "tableHeader", "content": [adf_paragraph(text("Col A"))]},
                        {"type": "tableHeader", "content": [adf_paragraph(text("Col B"))]},
                    ],
                },
                {
                    "type": "tableRow",
                    "content": [
                        {"type": "tableCell", "content": [adf_paragraph(text("a1"))]},
                        {"type": "tableCell", "content": [adf_paragraph(text("b1"))]},
                    ],
                },
            ],
        },
        {"type": "blockquote", "content": [adf_paragraph(text("quoted line"))]},
        {"type": "panel", "attrs": {"panelType": "info"}, "content": [adf_paragraph(text("panel body"))]},
        {"type": "mediaSingle", "content": [{"type": "media", "attrs": {"id": "img-1", "type": "file", "alt": "diagram.png"}}]},
        adf_paragraph({"type": "date", "attrs": {"timestamp": "1700000000000"}}, text(" "), {"type": "status", "attrs": {"text": "BLOCKED", "color": "red"}}),
        {"type": "futureNodeType", "content": [adf_paragraph(text("unknown node text survives"))]},
        adf_paragraph(text("line one"), {"type": "hardBreak"}, text("line two 日本語")),
    ],
}


def deep_adf(depth):
    node = adf_paragraph(text("deep"))
    for _ in range(depth):
        node = {"type": "blockquote", "content": [node]}
    return {"type": "doc", "version": 1, "content": [node]}


def user(account_id, name, email="hidden@example.com"):
    return {
        "accountId": account_id,
        "displayName": name,
        "emailAddress": email,
        "active": True,
        "accountType": "atlassian",
        "timeZone": "Europe/Berlin",
        "avatarUrls": {"48x48": "https://example.invalid/avatar.png"},
        "self": "https://fixture.atlassian.net/rest/api/3/user?accountId=" + account_id,
    }


ME = user("5b10ac8d82e05b22cc7d4ef5", "E2E Runner", "e2e@example.com")
MIA = user("5b10a2844c20165700ede21g", "Mia Krystof")
BOB = user("712020:8d5a4bc1-0f9e-4e3f-9a0e-1234567890ab", "Bob Builder")

STATUS_TODO = {"id": "10000", "name": "To Do", "statusCategory": {"id": 2, "key": "new", "name": "To Do"}}
STATUS_PROG = {"id": "3", "name": "In Progress", "statusCategory": {"id": 4, "key": "indeterminate", "name": "In Progress"}}
STATUS_DONE = {"id": "10001", "name": "Done", "statusCategory": {"id": 3, "key": "done", "name": "Done"}}


def issue(key, summary, description=None, extra=None):
    fields = {
        "summary": summary,
        "status": STATUS_TODO,
        "assignee": MIA,
        "reporter": ME,
        "priority": {"id": "3", "name": "Medium", "iconUrl": "https://example.invalid/p.png"},
        "issuetype": {"id": "10001", "name": "Task", "subtask": False, "hierarchyLevel": 0},
        "created": "2026-08-01T10:00:00.000+0000",
        "updated": "2026-08-02T11:30:00.000+0000",
        "labels": ["e2e", "fixture"],
        "components": [{"id": "10100", "name": "API"}],
        "resolution": None,
        "description": description,
    }
    if extra:
        fields.update(extra)
    return {
        "id": "1" + re.sub(r"\D", "", key).rjust(4, "0"),
        "key": key,
        "self": "https://fixture.atlassian.net/rest/api/3/issue/" + key,
        "fields": fields,
    }


COMMENTS = [
    {
        "id": "10500",
        "author": MIA,
        "created": "2026-08-03T09:00:00.000+0000",
        "updated": "2026-08-03T09:00:00.000+0000",
        "body": {"type": "doc", "version": 1, "content": [adf_paragraph(text("first comment "), {"type": "mention", "attrs": {"id": "x", "text": "@E2E Runner"}})]},
        "visibility": {"type": "role", "value": "Administrators"},
    },
    {
        "id": "10501",
        "author": BOB,
        "created": "2026-08-04T09:00:00.000+0000",
        "updated": "2026-08-04T09:30:00.000+0000",
        "body": "legacy plain-string body",
    },
]

TRANSITIONS = [
    {"id": "11", "name": "Start Progress", "to": STATUS_PROG, "hasScreen": False, "isGlobal": False, "isInitial": False, "isAvailable": True},
    {"id": "21", "name": "Resolve", "to": STATUS_DONE, "hasScreen": True, "isGlobal": True, "isInitial": False, "isAvailable": True},
    {"id": "31", "name": "Reopen", "to": STATUS_TODO, "hasScreen": False, "isGlobal": True, "isInitial": False, "isAvailable": True},
]
TRANSITION_FIELDS = {
    "21": {
        "resolution": {
            "required": True,
            "name": "Resolution",
            "schema": {"type": "resolution", "system": "resolution"},
            "allowedValues": [{"id": "10000", "name": "Done"}, {"id": "10001", "name": "Won't Do"}],
        },
        "comment": {"required": False, "name": "Comment", "schema": {"type": "comments-page", "system": "comment"}},
    }
}

PROJECTS = [
    {"id": "10000", "key": "E2E", "name": "End to End", "projectTypeKey": "software", "style": "classic", "simplified": False, "isPrivate": False, "lead": ME, "description": "Fixture project"},
    {"id": "10001", "key": "OPS", "name": "Operations", "projectTypeKey": "business", "style": "next-gen", "simplified": True, "isPrivate": False, "lead": MIA, "description": ""},
    {"id": "10002", "key": "SD", "name": "Service Desk", "projectTypeKey": "service_desk", "style": "classic", "simplified": False, "isPrivate": True, "lead": BOB},
]

ISSUE_TYPES = [
    {"id": "10001", "name": "Task", "description": "A task", "subtask": False, "hierarchyLevel": 0},
    {"id": "10002", "name": "Bug", "description": "A bug", "subtask": False, "hierarchyLevel": 0},
    {"id": "10003", "name": "Sub-task", "description": "", "subtask": True, "hierarchyLevel": -1},
]

CREATE_FIELDS = {
    "10001": [
        {"fieldId": "summary", "key": "summary", "name": "Summary", "required": True, "schema": {"type": "string", "system": "summary"}, "hasDefaultValue": False, "operations": ["set"]},
        {"fieldId": "issuetype", "key": "issuetype", "name": "Issue Type", "required": True, "schema": {"type": "issuetype", "system": "issuetype"}, "hasDefaultValue": False, "operations": [], "allowedValues": [{"id": "10001", "name": "Task"}]},
        {"fieldId": "priority", "key": "priority", "name": "Priority", "required": False, "schema": {"type": "priority", "system": "priority"}, "hasDefaultValue": True, "operations": ["set"], "allowedValues": [{"id": "1", "name": "Highest"}, {"id": "3", "name": "Medium"}, {"id": "5", "name": "Lowest"}]},
        {"fieldId": "components", "key": "components", "name": "Components", "required": False, "schema": {"type": "array", "items": "component", "system": "components"}, "hasDefaultValue": False, "operations": ["add", "set", "remove"], "allowedValues": [{"id": "10100", "name": "API"}, {"id": "10101", "name": "UI"}]},
        {"fieldId": "customfield_10016", "key": "customfield_10016", "name": "Story point estimate", "required": False, "schema": {"type": "number", "custom": "com.pyxis.greenhopper.jira:jsw-story-points", "customId": 10016}, "hasDefaultValue": False, "operations": ["set"]},
    ],
    "10002": [
        {"fieldId": "summary", "key": "summary", "name": "Summary", "required": True, "schema": {"type": "string", "system": "summary"}, "hasDefaultValue": False, "operations": ["set"]},
        {"fieldId": "environment", "key": "environment", "name": "Environment", "required": True, "schema": {"type": "string", "system": "environment"}, "hasDefaultValue": False, "operations": ["set"]},
    ],
}


def is_adf(value):
    return isinstance(value, dict) and value.get("type") == "doc" and isinstance(value.get("content"), list)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    # -- plumbing -----------------------------------------------------------
    def send_json(self, status, payload, headers=None):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json;charset=UTF-8")
        self.send_header("Content-Length", str(len(body)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def send_empty(self, status=204):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def send_html(self, status, html):
        body = html.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html;charset=UTF-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def error_collection(self, status, messages=None, errors=None, headers=None):
        self.send_json(status, {"errorMessages": messages or [], "errors": errors or {}, "status": status}, headers)

    def read_body(self):
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            # Drain a chunked body (the wasi:http client streams bodies that
            # carry no Content-Length this way).
            raw = b""
            while True:
                size_line = self.rfile.readline().strip()
                size = int(size_line.split(b";")[0] or b"0", 16)
                if size == 0:
                    while self.rfile.readline().strip():
                        pass  # trailers
                    break
                raw += self.rfile.read(size)
                self.rfile.readline()  # CRLF after the chunk
        else:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
        if not raw:
            return None
        try:
            return json.loads(raw.decode("utf-8"))
        except ValueError:
            return {"__unparseable__": raw.decode("utf-8", "replace")[:200]}

    def auth_user(self):
        header = self.headers.get("Authorization") or ""
        if not header.startswith("Basic "):
            return None
        try:
            pair = base64.b64decode(header[6:].encode()).decode("utf-8")
        except Exception:
            return None
        return VALID_USERS.get(pair)

    def record_write(self, path, body, query):
        with LOCK:
            LAST_WRITE.clear()
            LAST_WRITE.update({"method": self.command, "path": path, "query": query, "body": body})

    def echo(self, path, query, body=None):
        return {
            "path": path,
            "query": query,
            "body": body,
            "authUser": self.auth_user(),
            "accept": self.headers.get("Accept"),
            "contentType": self.headers.get("Content-Type"),
            "userAgent": self.headers.get("User-Agent"),
        }

    # -- dispatch -----------------------------------------------------------
    def dispatch(self):
        parts = urlsplit(self.path)
        raw_path = parts.path
        query = {k: v if len(v) > 1 else v[0] for k, v in parse_qs(parts.query, keep_blank_values=True).items()}
        raw_query = parts.query

        if raw_path == "/__fixture/last-write":
            with LOCK:
                return self.send_json(200, LAST_WRITE)
        if raw_path == "/__fixture/last-request":
            with LOCK:
                return self.send_json(200, LAST_REQUEST)
        if raw_path == "/__fixture/health":
            return self.send_json(200, {"ok": True})

        match = ROUTE_RE.match(raw_path)
        if not match:
            return self.error_collection(404, ["Fixture: unknown route " + raw_path])
        cloud = match.group("cloud")
        path = unquote(match.group("path"))

        if self.auth_user() is None:
            return self.error_collection(401, ["Client must be authenticated to access this resource."])
        if "application/json" not in (self.headers.get("Accept") or ""):
            return self.error_collection(406, ["Fixture: Accept must include application/json"])

        body = self.read_body() if self.command in ("POST", "PUT") else None
        if self.command in ("POST", "PUT") and "application/json" not in (self.headers.get("Content-Type") or ""):
            return self.error_collection(415, ["Fixture: Content-Type must be application/json"])
        echo = self.echo(path, query, body)
        echo["rawQuery"] = raw_query
        echo["cloudId"] = cloud
        echo["method"] = self.command
        with LOCK:
            LAST_REQUEST.clear()
            LAST_REQUEST.update(echo)
        method = self.command

        # -- identity --------------------------------------------------------
        if method == "GET" and path == "/myself":
            me = dict(ME)
            me["fixtureEcho"] = echo
            return self.send_json(200, me)

        # -- search ----------------------------------------------------------
        if path == "/search" or path.startswith("/search?"):
            return self.error_collection(410, ["The requested API has been removed. Use /rest/api/3/search/jql."])
        if method == "POST" and path == "/search/jql":
            jql = (body or {}).get("jql") or ""
            if "BADFIELD" in jql:
                return self.error_collection(400, ["Field 'BADFIELD' does not exist or you do not have permission to view it."])
            if jql.strip().lower().startswith("order by"):
                return self.error_collection(400, ["The provided JQL query is unbounded. Please add search restrictions in order to narrow down your search."])
            if "RATE" in jql:
                return self.error_collection(429, [], headers={"Retry-After": "7", "X-RateLimit-Remaining": "0", "RateLimit-Reason": "burst-limit"})
            token = (body or {}).get("nextPageToken")
            if token == "p2":
                # 55 levels: past the renderer's depth cap (32), under
                # serde_json's 128-level recursion limit (two JSON levels per
                # ADF node plus the envelope).
                page = {"issues": [issue("E2E-3", "third issue", deep_adf(55))], "isLast": True}
            elif token == "absurd-depth":
                # 300 levels: beyond serde_json's recursion limit — must be a
                # clean decode error, never a trap.
                page = {"issues": [issue("E2E-4", "absurd issue", deep_adf(300))], "isLast": True}
            elif token:
                return self.error_collection(400, ["The provided nextPageToken is invalid."])
            else:
                page = {
                    "issues": [
                        issue("E2E-1", "first issue ☃", RICH_DESCRIPTION),
                        issue("E2E-2", "second issue", "plain string description"),
                    ],
                    "nextPageToken": "p2",
                    "isLast": False,
                }
            if "names" in ((body or {}).get("expand") or ""):
                page["names"] = {"summary": "Summary", "status": "Status"}
            page["fixtureEcho"] = echo
            return self.send_json(200, page)
        if method == "POST" and path == "/search/approximate-count":
            jql = (body or {}).get("jql") or ""
            if "BADFIELD" in jql:
                return self.error_collection(400, ["Field 'BADFIELD' does not exist or you do not have permission to view it."])
            return self.send_json(200, {"count": 42, "fixtureEcho": echo})

        # -- issues ----------------------------------------------------------
        m = re.match(r"^/issue/([^/]+)$", path)
        if m and method == "GET":
            key = m.group(1)
            special = {
                "NOPE-1": lambda: self.error_collection(404, ["Issue does not exist or you do not have permission to see it."]),
                "FORB-1": lambda: self.error_collection(403, ["You do not have the permission to see the specified issue."]),
                "RATE-1": lambda: self.error_collection(429, [], headers={"Retry-After": "7", "X-RateLimit-Remaining": "0", "RateLimit-Reason": "points-budget"}),
                "AUTH-1": lambda: self.error_collection(401, ["Client must be authenticated to access this resource."]),
                "HTML-1": lambda: self.send_html(200, "<html><body>Log in to continue</body></html>"),
                "BOOM-1": lambda: self.error_collection(500, ["Internal server error"]),
                "GONE-1": lambda: self.error_collection(410, ["Gone"]),
                "CONF-1": lambda: self.error_collection(409, ["Issue was updated concurrently"]),
                "PROXY-1": lambda: self.send_json(502, {"message": "Bad Gateway from proxy"}),
            }
            if key in special:
                return special[key]()
            result = issue(key, "fetched issue " + key, RICH_DESCRIPTION, {"comment": {"comments": COMMENTS, "total": 2, "startAt": 0, "maxResults": 100}})
            if "changelog" in (query.get("expand") or ""):
                result["changelog"] = {"startAt": 0, "maxResults": 100, "total": 1, "histories": [{"id": "1", "created": "2026-08-02T11:30:00.000+0000", "items": [{"field": "status", "fromString": "To Do", "toString": "In Progress"}]}]}
            if "renderedFields" in (query.get("expand") or ""):
                result["renderedFields"] = {"description": "<p>rendered html</p>"}
            result["fixtureEcho"] = echo
            return self.send_json(200, result)
        if m and method == "PUT":
            key = m.group(1)
            if key == "NOPE-1":
                return self.error_collection(404, ["Issue does not exist or you do not have permission to see it."])
            fields = (body or {}).get("fields") or {}
            if "description" in fields and not is_adf(fields["description"]):
                return self.error_collection(400, [], {"description": "Operation value must be an Atlassian Document (see the Atlassian Document Format)"})
            if "customfield_99999" in fields:
                return self.error_collection(400, [], {"customfield_99999": "Field 'customfield_99999' cannot be set. It is not on the appropriate screen, or unknown."})
            self.record_write(path, body, query)
            if query.get("returnIssue") == "true":
                return self.send_json(200, issue(key, fields.get("summary", "updated issue"), fields.get("description")))
            return self.send_empty(204)
        if method == "POST" and path == "/issue":
            fields = (body or {}).get("fields") or {}
            errors = {}
            if not fields.get("summary"):
                errors["summary"] = "You must specify a summary of the issue."
            if not fields.get("project"):
                errors["project"] = "Specify a valid project ID or key"
            if "description" in fields and not is_adf(fields["description"]):
                errors["description"] = "Operation value must be an Atlassian Document (see the Atlassian Document Format)"
            if "customfield_99999" in fields:
                errors["customfield_99999"] = "Field 'customfield_99999' cannot be set. It is not on the appropriate screen, or unknown."
            if (fields.get("project") or {}).get("key") == "NOPE":
                return self.error_collection(400, [], {"project": "Specify a valid project ID or key"})
            if (fields.get("priority") or {}).get("name") == "Bogus":
                errors["priority"] = "Priority name 'Bogus' is not valid"
            if errors:
                return self.error_collection(400, [], errors)
            self.record_write(path, body, query)
            return self.send_json(201, {"id": "10101", "key": "E2E-101", "self": "https://fixture.atlassian.net/rest/api/3/issue/10101"})

        m = re.match(r"^/issue/([^/]+)/transitions$", path)
        if m:
            key = m.group(1)
            if key == "NOPE-1":
                return self.error_collection(404, ["Issue does not exist or you do not have permission to see it."])
            if method == "GET":
                transitions = []
                for t in TRANSITIONS:
                    t = dict(t)
                    if "transitions.fields" in (query.get("expand") or "") and t["id"] in TRANSITION_FIELDS:
                        t["fields"] = TRANSITION_FIELDS[t["id"]]
                    transitions.append(t)
                return self.send_json(200, {"expand": "transitions", "transitions": transitions, "fixtureEcho": echo})
            if method == "POST":
                tid = ((body or {}).get("transition") or {}).get("id")
                if tid not in {t["id"] for t in TRANSITIONS}:
                    return self.error_collection(400, ["Transition id '%s' is not valid for this issue." % tid])
                if tid == "21" and not ((body or {}).get("fields") or {}).get("resolution"):
                    return self.error_collection(400, [], {"resolution": "Resolution is required."})
                update = (body or {}).get("update") or {}
                for entry in update.get("comment") or []:
                    if not is_adf((entry.get("add") or {}).get("body")):
                        return self.error_collection(400, [], {"comment": "Operation value must be an Atlassian Document (see the Atlassian Document Format)"})
                self.record_write(path, body, query)
                return self.send_empty(204)

        m = re.match(r"^/issue/([^/]+)/assignee$", path)
        if m and method == "PUT":
            key = m.group(1)
            if key == "NOPE-1":
                return self.error_collection(404, ["Issue does not exist or you do not have permission to see it."])
            account = (body or {}).get("accountId", "MISSING")
            if account == "MISSING":
                return self.error_collection(400, ["accountId is required (null to unassign)"])
            if account == "bad-user":
                return self.error_collection(400, ["User 'bad-user' cannot be assigned issues."])
            self.record_write(path, body, query)
            return self.send_empty(204)

        m = re.match(r"^/issue/([^/]+)/comment$", path)
        if m:
            key = m.group(1)
            if key == "NOPE-1":
                return self.error_collection(404, ["Issue does not exist or you do not have permission to see it."])
            if method == "GET":
                start = int(query.get("startAt") or 0)
                page = COMMENTS[start:start + int(query.get("maxResults") or 50)]
                if (query.get("orderBy") or "-created") == "-created":
                    page = list(reversed(page))
                return self.send_json(200, {"comments": page, "total": len(COMMENTS), "startAt": start, "maxResults": int(query.get("maxResults") or 50), "fixtureEcho": echo})
            if method == "POST":
                if not is_adf((body or {}).get("body")):
                    return self.error_collection(400, [], {"body": "Operation value must be an Atlassian Document (see the Atlassian Document Format)"})
                visibility = (body or {}).get("visibility")
                if visibility and visibility.get("value") == "no-such-role":
                    return self.error_collection(400, [], {"visibility": "Role 'no-such-role' does not exist."})
                self.record_write(path, body, query)
                created = {"id": "10777", "author": ME, "created": "2026-09-02T12:00:00.000+0000", "updated": "2026-09-02T12:00:00.000+0000", "body": body.get("body"), "fixtureEcho": echo}
                if visibility:
                    created["visibility"] = visibility
                return self.send_json(201, created)

        # -- create metadata -------------------------------------------------
        m = re.match(r"^/issue/createmeta/([^/]+)/issuetypes$", path)
        if m and method == "GET":
            key = m.group(1)
            if key == "NOPE":
                return self.error_collection(404, ["The project NOPE was not found, or you do not have permission to view it."])
            return self.send_json(200, {"issueTypes": ISSUE_TYPES, "startAt": 0, "maxResults": int(query.get("maxResults") or 50), "total": len(ISSUE_TYPES), "fixtureEcho": echo})
        m = re.match(r"^/issue/createmeta/([^/]+)/issuetypes/([^/]+)$", path)
        if m and method == "GET":
            key, type_id = m.group(1), m.group(2)
            if key == "NOPE" or type_id not in CREATE_FIELDS:
                return self.error_collection(404, ["The issue type %s was not found, or you do not have permission to view it." % type_id])
            return self.send_json(200, {"fields": CREATE_FIELDS[type_id], "startAt": 0, "maxResults": int(query.get("maxResults") or 50), "total": len(CREATE_FIELDS[type_id]), "fixtureEcho": echo})

        # -- projects --------------------------------------------------------
        if method == "GET" and path == "/project/search":
            values = PROJECTS
            keys = query.get("keys")
            if keys:
                wanted = set(keys if isinstance(keys, list) else [keys])
                values = [p for p in values if p["key"] in wanted]
            text_filter = (query.get("query") or "").lower()
            if text_filter:
                values = [p for p in values if text_filter in p["key"].lower() or text_filter in p["name"].lower()]
            type_key = query.get("typeKey")
            if type_key:
                values = [p for p in values if p["projectTypeKey"] == type_key]
            start = int(query.get("startAt") or 0)
            size = int(query.get("maxResults") or 50)
            page = values[start:start + size]
            return self.send_json(200, {"self": "x", "maxResults": size, "startAt": start, "total": len(values), "isLast": start + size >= len(values), "values": page, "fixtureEcho": echo})
        m = re.match(r"^/project/([^/]+)$", path)
        if m and method == "GET":
            key = m.group(1)
            found = next((p for p in PROJECTS if p["key"] == key or p["id"] == key), None)
            if not found:
                return self.error_collection(404, ["No project could be found with key '%s'." % key])
            project = dict(found)
            project["issueTypes"] = ISSUE_TYPES
            project["components"] = [{"id": "10100", "name": "API"}, {"id": "10101", "name": "UI"}]
            project["versions"] = [{"id": "10200", "name": "1.0", "released": True}]
            project["fixtureEcho"] = echo
            return self.send_json(200, project)

        # -- users -----------------------------------------------------------
        if method == "GET" and path in ("/user/search", "/user/assignable/search"):
            q = query.get("query") or ""
            if q == "nobody":
                return self.send_json(200, [])
            users = [dict(ME), dict(MIA), dict(BOB)]
            users[0]["fixtureEcho"] = echo
            return self.send_json(200, users[: int(query.get("maxResults") or 50)])

        return self.error_collection(404, ["Fixture: no handler for %s %s" % (method, path)])

    def do_GET(self):
        self.dispatch()

    def do_POST(self):
        self.dispatch()

    def do_PUT(self):
        self.dispatch()

    def do_DELETE(self):
        self.error_collection(405, ["Fixture: DELETE not supported"])


if __name__ == "__main__":
    port = int(sys.argv[1])
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    server.serve_forever()
