#!/usr/bin/env python3
"""Hermetic stand-in for the Obsidian "Local REST API with MCP" plugin (5.1.0).

Started by scripts/e2e.sh on 127.0.0.1:<port>. It keeps an in-memory vault,
mirrors the plugin's routes, content negotiation, error envelope
({errorCode, message}), version-gated PATCH formats, and the periodic-notes
companion's 307 redirect. A ThreadingHTTPServer, because the harness fires 8
requests at once.

Debug endpoints (no auth):
  GET  /_fixture/requests           last 50 requests {method, raw_path, headers, body}
  GET  /_fixture/count              {"count": N} — every non-fixture request ever seen
  POST /_fixture/reset              re-seed the vault, clear the log
  POST /_fixture/version {"version": "4.1.7"}   switch the impersonated plugin version
  POST /_fixture/periodic {"enabled": false}    pretend the companion plugin is missing
                                                (the core's 404 errorCode 40400 envelope;
                                                add "envelope": false for a bare text 404)
      ... {"enabled": true, "delay_ms": 2200}    slow /periodic/ answers (call-budget tests)
      ... {"enabled": true, "unknown_periods": ["quarterly"]}   404 errorCode 40460
      ... {"enabled": true, "disabled_periods": ["weekly"]}     400 errorCode 40060
                                                (default: every period but daily)
      ... {"enabled": true, "redirect_missing": true}  redirect even when the note is
                                                missing (404 on the second hop)
      ... {"enabled": true, "fail": true}        500 errorCode 50060
      ... {"enabled": true, "error_code": 40499} 404 with an unrecognised errorCode
    Every POST resets the options it does not name.
      ... {"enabled": true, "today_shift_days": -1}  the plugin's "today" is a day off UTC
  POST /_fixture/root {"fail": true}            GET / answers 500 (patch refresh fall-through)
  GET  /vault/Slow/<n>.md                       answers after slow_dir_delay_ms (2.2 s)
  POST /_fixture/key {"key": "other"}           change the accepted API key
"""
import copy
import fnmatch
import json
import os
import re
import sys
import threading
import time
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, quote, unquote, urlsplit

PORT = int(sys.argv[1])
LOCK = threading.Lock()
STATE = {
    "version": os.environ.get("FIXTURE_PLUGIN_VERSION", "5.1.0"),
    "key": "test-key",
    "periodic": os.environ.get("FIXTURE_NO_PERIODIC") != "1",
    "periodic_delay_ms": 0,      # sleep before answering /periodic/ (budget tests)
    "periodic_today_shift": 0,   # days the plugin's "today" differs from UTC
    "periodic_envelope": True,   # missing companion: the core's {errorCode: 40400} body
    "periodic_unknown": [],      # periods answered 404 errorCode 40460
    "periodic_disabled": ["weekly", "monthly", "quarterly", "yearly"],  # 400 errorCode 40060
    "periodic_redirect_missing": False,  # redirect even when the note is missing
    "periodic_fail": False,      # 500 errorCode 50060
    "periodic_code": None,       # answer 404 with this errorCode instead (unknown-code tests)
    "root_fail": False,          # GET / answers 500 (refresh-failure tests)
    "slow_dir_delay_ms": 2200,   # GET /vault/Slow/* sleeps this long, then answers
}
REQUESTS = []
VAULT = {}
DAY_MS = 86_400_000
NOW_MS = int(time.time() * 1000)
TODAY = datetime.now(timezone.utc).date()
YESTERDAY = TODAY - timedelta(days=1)

MT_MD = "text/markdown"
MT_NOTE = "application/vnd.olrapi.note+json"
MT_MAP = "application/vnd.olrapi.document-map+json"
MT_JL = "application/vnd.olrapi.jsonlogic+json"
MT_PATCH = "application/vnd.olrapi.patch-instruction+json"
# markdown-patch 1.x `ContentType`: the plugin's `isContentType` guard on
# targeted writes (POST .../heading/.., every PATCH on < 5, the header path on
# 5.x) is an exact string match — "text/markdown; charset=utf-8" is rejected
# with 400 errorCode 40012 (typeGuards.ts: Object.values(ContentType).includes).
WRITE_CONTENT_TYPES = ("text/markdown", "application/json")
INVALID_CT_MSG = "Unknown or invalid Content-Type specified in Content-Type header."
TEXT_REQUIRED_MSG = ("Incoming content must be text data and have an appropriate text/* "
                     "Content-type header set (e.g. text/markdown).")

PLAN = """---
title: Plan
tags:
  - project
  - work/tasks
status: active
---
# Overview

Plan overview paragraph.

## Details

Some details here. ^abc123

## Log

- 2026-09-01 started
- 2026-09-02 continued

| a | b |
|---|---|
| 1 | 2 |
^tbl1
"""

RECORDED_HEADERS = [
    "Authorization", "Accept", "Content-Type", "Operation", "Target-Type", "Target",
    "Markdown-Patch-Version", "If-Match", "Reject-If-Content-Preexists", "Target-Scope",
    "Create-Target-If-Missing", "User-Agent", "Transfer-Encoding", "Content-Length",
]


def seed():
    v = {}

    def add(path, content, age_days):
        v[path] = {
            "content": content,
            "mtime": NOW_MS - int(age_days * DAY_MS),
            "ctime": NOW_MS - int((age_days + 10) * DAY_MS),
        }

    add("Welcome.md", "# Welcome\n\nThis is the welcome note. #welcome\n", 200)
    add("Projects/Plan.md", PLAN, 0.5)
    add("Projects/Archive/Old.md", "# Old\n\nArchived plan. #project\n", 400)
    add("Notes/Héllo Wörld/Tôdo.md", "# TODO/DONE\n\n- [ ] ünïcode task\n\n# Other\n\nnothing\n", 3)
    add(f"Daily/{TODAY.isoformat()}.md", f"# {TODAY.isoformat()}\n\n- daily entry today\n", 0.1)
    add(f"Daily/{YESTERDAY.isoformat()}.md", f"# {YESTERDAY.isoformat()}\n\n- daily entry yesterday\n", 1.2)
    add("big.md", "# Big\n\n" + ("lorem ipsum dolor sit amet ☃ " * 10000), 5)
    add("BadYaml.md", "---\ntitle: bad: yaml: here\n---\n# Bad\n\ntext\n", 2.5)
    return v


VAULT = seed()


# --- markdown helpers ---------------------------------------------------------

HEADING_RE = re.compile(r"^(#{1,6})\s+(.*?)\s*$")
BLOCK_RE = re.compile(r"\s\^([A-Za-z0-9-]+)\s*$")
BLOCK_LINE_RE = re.compile(r"^\^([A-Za-z0-9-]+)\s*$")
TAG_RE = re.compile(r"(?<![\w/])#([A-Za-z0-9_/-]+)")


def split_frontmatter(text):
    if text.startswith("---\n"):
        end = text.find("\n---", 4)
        if end != -1:
            return text[4:end], text[end + 4:].lstrip("\n")
    return None, text


def parse_frontmatter(fm):
    """Tiny YAML subset: `key: value` and `key:` followed by `- item` lines."""
    if fm is None:
        return {}
    data = {}
    key = None
    for line in fm.splitlines():
        m = re.match(r"^([A-Za-z0-9_-]+):\s*(.*)$", line)
        if m:
            key, value = m.group(1), m.group(2)
            if value == "":
                data[key] = []
            else:
                data[key] = coerce(value)
            continue
        m = re.match(r"^\s*-\s+(.*)$", line)
        if m and key is not None and isinstance(data.get(key), list):
            data[key].append(coerce(m.group(1)))
    return data


def coerce(value):
    if value in ("true", "false"):
        return value == "true"
    try:
        return int(value)
    except ValueError:
        return value.strip('"')


def render_frontmatter(data):
    lines = []
    for key, value in data.items():
        if isinstance(value, list):
            lines.append(f"{key}:")
            for item in value:
                lines.append(f"  - {item}")
        elif isinstance(value, bool):
            lines.append(f"{key}: {'true' if value else 'false'}")
        else:
            lines.append(f"{key}: {value}")
    return "---\n" + "\n".join(lines) + "\n---\n"


def note_tags(text):
    fm, body = split_frontmatter(text)
    tags = []
    for t in parse_frontmatter(fm).get("tags", []) or []:
        tags.append(str(t))
    tags += TAG_RE.findall(body)
    return tags


def headings(text):
    out = []
    for i, line in enumerate(text.splitlines()):
        m = HEADING_RE.match(line)
        if m:
            out.append({"level": len(m.group(1)), "text": m.group(2), "line": i})
    return out


def heading_tree(hs):
    root = []
    stack = []
    for h in hs:
        node = {"level": h["level"], "heading": h["text"], "children": []}
        while stack and stack[-1]["level"] >= h["level"]:
            stack.pop()
        (stack[-1]["children"] if stack else root).append(node)
        stack.append(node)
    return root


def find_section(text, path):
    """Returns (start_line, end_line_exclusive, level) of the heading path."""
    lines = text.splitlines()
    hs = headings(text)
    idx = 0
    min_level = 0
    limit = len(lines)
    found = None
    for element in path:
        match = None
        for h in hs:
            if h["line"] < idx or h["line"] >= limit:
                continue
            if h["level"] <= min_level and found is not None:
                break
            if h["text"] == element:
                match = h
                break
        if match is None:
            return None
        found = match
        idx = match["line"] + 1
        min_level = match["level"]
        limit = len(lines)
        for h in hs:
            if h["line"] > match["line"] and h["level"] <= match["level"]:
                limit = h["line"]
                break
    return found["line"], limit, found["level"]


def blocks(text):
    out = {}
    lines = text.splitlines()
    for i, line in enumerate(lines):
        m = BLOCK_RE.search(line)
        if m:
            out[m.group(1)] = (i, i)
            continue
        m = BLOCK_LINE_RE.match(line)
        if m:
            start = i - 1
            while start > 0 and lines[start - 1].strip():
                start -= 1
            out[m.group(1)] = (max(start, 0), i)
    return out


def note_json(path, note):
    fm, body = split_frontmatter(note["content"])
    return {
        "path": path,
        "content": note["content"],
        "frontmatter": parse_frontmatter(fm),
        "tags": note_tags(note["content"]),
        "links": [],
        "backlinks": [],
        "unresolvedLinks": [],
        "stat": {
            "ctime": note["ctime"],
            "mtime": note["mtime"],
            "size": len(note["content"].encode()),
        },
    }


def document_map(path, note):
    fm, _ = split_frontmatter(note["content"])
    return {
        "path": path,
        "headings": heading_tree(headings(note["content"])),
        "blocks": sorted(blocks(note["content"]).keys()),
        "frontmatterFields": list(parse_frontmatter(fm).keys()),
        "version": "v1",
    }


# --- JsonLogic (the subset the plugin's users rely on) --------------------------

def truthy(v):
    if isinstance(v, list):
        return len(v) > 0
    return bool(v)


def jl(rule, data):
    if isinstance(rule, list):
        return [jl(r, data) for r in rule]
    if not isinstance(rule, dict) or len(rule) != 1:
        return rule
    op, args = next(iter(rule.items()))
    if not isinstance(args, list):
        args = [args]
    if op == "var":
        key = jl(args[0], data) if args else ""
        default = jl(args[1], data) if len(args) > 1 else None
        cur = data
        if key in ("", None):
            return cur
        for part in str(key).split("."):
            if isinstance(cur, dict) and part in cur:
                cur = cur[part]
            elif isinstance(cur, list) and part.isdigit() and int(part) < len(cur):
                cur = cur[int(part)]
            else:
                return default
        return cur
    if op == "if":
        i = 0
        while i + 1 < len(args):
            if truthy(jl(args[i], data)):
                return jl(args[i + 1], data)
            i += 2
        return jl(args[i], data) if i < len(args) else None
    if op == "and":
        v = True
        for a in args:
            v = jl(a, data)
            if not truthy(v):
                return v
        return v
    if op == "or":
        v = False
        for a in args:
            v = jl(a, data)
            if truthy(v):
                return v
        return v
    vals = [jl(a, data) for a in args]
    if op == "!":
        return not truthy(vals[0])
    if op == "!!":
        return truthy(vals[0])
    if op in ("==", "==="):
        return vals[0] == vals[1]
    if op in ("!=", "!=="):
        return vals[0] != vals[1]
    if op in (">", ">=", "<", "<="):
        try:
            a, b = float(vals[0]), float(vals[1])
        except (TypeError, ValueError):
            return False
        return {">": a > b, ">=": a >= b, "<": a < b, "<=": a <= b}[op]
    if op == "in":
        needle, hay = vals[0], vals[1]
        if isinstance(hay, str):
            return str(needle) in hay
        if isinstance(hay, list):
            return needle in hay
        return False
    if op == "glob":
        return fnmatch.fnmatchcase(str(vals[1]), str(vals[0]))
    if op == "regexp":
        return re.search(str(vals[0]), str(vals[1])) is not None
    raise ValueError(f"unknown operator {op}")


# --- patch application -------------------------------------------------------

class PatchError(Exception):
    def __init__(self, status, code, message):
        super().__init__(message)
        self.status, self.code, self.message = status, code, message


def apply_patch(path, note, target_type, target, operation, scope, content, value, create):
    text = note["content"]
    lines = text.splitlines()
    if target_type == "heading":
        if not isinstance(target, list):
            raise PatchError(400, 40081, "heading targets must be an array of heading texts")
        sec = find_section(text, target)
        if sec is None:
            if not create:
                raise PatchError(404, 40400, f"heading {' > '.join(target)} not found")
            lines += ["", "#" * min(len(target), 6) + " " + target[-1], ""]
            text = "\n".join(lines) + "\n"
            sec = find_section(text, target) or find_section(text, [target[-1]])
            lines = text.splitlines()
        start, end, level = sec
        payload = content if content is not None else json.dumps(value)
        if scope == "marker":
            if operation == "replace":
                lines[start] = "#" * level + " " + payload
            elif operation == "delete":
                del lines[start]
            else:
                raise PatchError(400, 40080, f"{operation} is not valid for scope marker")
        elif scope == "markerAndContent":
            if operation == "replace":
                lines[start:end] = payload.splitlines()
            elif operation == "delete":
                del lines[start:end]
            else:
                raise PatchError(400, 40080, f"{operation} is not valid for scope markerAndContent")
        else:
            if operation == "append":
                lines[end:end] = payload.splitlines()
            elif operation == "prepend":
                lines[start + 1:start + 1] = payload.splitlines()
            elif operation == "replace":
                lines[start + 1:end] = payload.splitlines()
            elif operation == "delete":
                del lines[start + 1:end]
        return "\n".join(lines) + "\n"
    if target_type == "block":
        if not isinstance(target, str):
            raise PatchError(400, 40081, "block targets take a string id")
        found = blocks(text).get(target.lstrip("^"))
        if found is None:
            raise PatchError(404, 40400, f"block ^{target} not found")
        start, end = found
        payload = content if content is not None else json.dumps(value)
        if operation == "append":
            lines[end + 1:end + 1] = payload.splitlines()
        elif operation == "prepend":
            lines[start:start] = payload.splitlines()
        elif operation == "replace":
            marker = lines[end]
            lines[start:end + 1] = payload.splitlines() + ([marker] if marker.startswith("^") else [])
        elif operation == "delete":
            del lines[start:end + 1]
        return "\n".join(lines) + "\n"
    if target_type == "frontmatter":
        if not isinstance(target, str):
            raise PatchError(400, 40081, "frontmatter targets take a string key")
        if path == "BadYaml.md":
            raise PatchError(400, 40005, "Invalid frontmatter")
        fm, body = split_frontmatter(text)
        data = parse_frontmatter(fm)
        if target not in data and not create and operation == "delete":
            raise PatchError(404, 40400, f"frontmatter field {target} not found")
        payload = value if value is not None else content
        if operation == "append":
            if isinstance(data.get(target), list):
                data[target] += payload if isinstance(payload, list) else [payload]
            else:
                data[target] = payload
        elif operation == "replace" or operation == "prepend":
            data[target] = payload
        elif operation == "delete":
            data.pop(target, None)
        return render_frontmatter(data) + body
    raise PatchError(400, 40081, f"unknown targetType {target_type}")


# --- HTTP -----------------------------------------------------------------------

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    # helpers
    def send(self, status, body=b"", ctype="application/json", extra=None):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        if body or status not in (204, 307):
            self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def send_json(self, status, obj, extra=None):
        self.send(status, json.dumps(obj), "application/json", extra)

    def err(self, status, code, message, extra=None):
        self.send_json(status, {"errorCode": code, "message": message}, extra)

    def read_body(self):
        # wasi:http streams request bodies as chunked transfer encoding (no
        # Content-Length); BaseHTTPRequestHandler does not decode that, and an
        # unconsumed body would corrupt the next keep-alive request.
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            chunks = []
            while True:
                line = self.rfile.readline().strip()
                size = int(line.split(b";")[0], 16) if line else 0
                if size == 0:
                    while True:
                        trailer = self.rfile.readline()
                        if trailer in (b"\r\n", b"\n", b""):
                            break
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()
            return b"".join(chunks)
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")

    def do_PUT(self):
        self.route("PUT")

    def do_PATCH(self):
        self.route("PATCH")

    def do_DELETE(self):
        self.route("DELETE")

    def route(self, method):
        parts = urlsplit(self.path)
        raw_path = parts.path
        qs = {k: v[0] for k, v in parse_qs(parts.query, keep_blank_values=True).items()}
        body = self.read_body()
        if not raw_path.startswith("/_fixture/"):
            with LOCK:
                REQUESTS.append({
                    "method": method,
                    "raw_path": self.path,
                    "headers": {h: self.headers.get(h) for h in RECORDED_HEADERS if self.headers.get(h) is not None},
                    "body": body[:4000].decode("utf-8", "replace"),
                })
                del REQUESTS[:-50]
                STATE["total_requests"] = STATE.get("total_requests", 0) + 1
        try:
            self.dispatch(method, raw_path, qs, body)
        except PatchError as e:
            self.err(e.status, e.code, e.message)
        except Exception as e:  # never let the fixture thread die silently
            self.err(500, 50000, f"fixture crashed: {e!r}")

    def dispatch(self, method, raw_path, qs, body):
        segs = [unquote(s) for s in raw_path.split("/")]
        if raw_path.startswith("/_fixture/"):
            return self.fixture(method, raw_path, body)
        if raw_path == "/" and method == "GET":
            if STATE["root_fail"]:
                return self.err(500, 50000, "fixture: status document unavailable")
            authed = self.headers.get("Authorization") == f"Bearer {STATE['key']}"
            return self.send_json(200, {
                "status": "OK",
                "service": "Obsidian Local REST API",
                "versions": {"self": STATE["version"], "obsidian": "1.13.4"},
                "authenticated": authed,
                "manifest": {"id": "obsidian-local-rest-api", "name": "Local REST API with MCP", "version": STATE["version"]},
            })
        if self.headers.get("Authorization") != f"Bearer {STATE['key']}":
            return self.err(401, 40101, "Authorization required. Find your API Key in the 'Local REST API with MCP' section of your Obsidian settings.")
        if raw_path == "/vault/" or raw_path.startswith("/vault/"):
            return self.vault(method, raw_path, segs[2:], qs, body)
        if raw_path == "/search/simple/":
            return self.search_simple(method, qs)
        if raw_path == "/search/":
            return self.search(method, body)
        if raw_path == "/tags/":
            return self.tags()
        if raw_path == "/commands/":
            return self.send_json(200, {"commands": [
                {"id": "editor:toggle-bold", "name": "Toggle bold"},
                {"id": "workspace:close", "name": "Close current tab"},
                {"id": "app:reload", "name": "Reload app without saving"},
            ]})
        if raw_path.startswith("/commands/") and method == "POST":
            cid = segs[2] if len(segs) > 2 else ""
            if cid in ("editor:toggle-bold", "workspace:close"):
                return self.send(204)
            return self.err(404, 40400, f"Command {cid} not found")
        if raw_path.startswith("/open/") and method == "POST":
            path = "/".join(segs[2:])
            with LOCK:
                if path not in VAULT:
                    VAULT[path] = {"content": "", "mtime": NOW_MS, "ctime": NOW_MS}
            return self.send(200, "", "text/plain")
        if raw_path == "/active/":
            # The plugin sets `encodeURI(file.path)`: a bare vault path, no /vault/ prefix.
            return self.serve_note("Projects/Plan.md", VAULT["Projects/Plan.md"], None, None,
                                   {"Content-Location": quote("Projects/Plan.md", safe="/")})
        if raw_path.startswith("/periodic/"):
            return self.periodic(segs[2:])
        return self.err(404, 40400, "Not Found")

    # /_fixture/*
    def fixture(self, method, raw_path, body):
        global VAULT
        if raw_path == "/_fixture/requests":
            with LOCK:
                return self.send_json(200, list(REQUESTS))
        data = json.loads(body or b"{}")
        if raw_path == "/_fixture/reset":
            with LOCK:
                VAULT = seed()
                REQUESTS.clear()
            return self.send_json(200, {"ok": True})
        if raw_path == "/_fixture/version":
            STATE["version"] = data["version"]
            return self.send_json(200, {"version": STATE["version"]})
        if raw_path == "/_fixture/periodic":
            STATE["periodic"] = bool(data["enabled"])
            STATE["periodic_delay_ms"] = int(data.get("delay_ms", 0))
            STATE["periodic_today_shift"] = int(data.get("today_shift_days", 0))
            STATE["periodic_envelope"] = bool(data.get("envelope", True))
            STATE["periodic_unknown"] = list(data.get("unknown_periods", []))
            STATE["periodic_disabled"] = list(data.get("disabled_periods", ["weekly", "monthly", "quarterly", "yearly"]))
            STATE["periodic_redirect_missing"] = bool(data.get("redirect_missing", False))
            STATE["periodic_fail"] = bool(data.get("fail", False))
            STATE["periodic_code"] = data.get("error_code")
            return self.send_json(200, {"periodic": STATE["periodic"]})
        if raw_path == "/_fixture/root":
            STATE["root_fail"] = bool(data.get("fail", False))
            return self.send_json(200, {"root_fail": STATE["root_fail"]})
        if raw_path == "/_fixture/count":
            with LOCK:
                return self.send_json(200, {"count": STATE.get("total_requests", 0)})
        if raw_path == "/_fixture/key":
            STATE["key"] = data["key"]
            return self.send_json(200, {"ok": True})
        return self.err(404, 40400, "no such fixture endpoint")

    # /vault/...
    def vault(self, method, raw_path, segs, qs, body):
        if raw_path.endswith("/"):
            if method != "GET":
                return self.err(405, 40510, "Method not allowed for directories")
            prefix = "/".join(s for s in segs if s != "")
            prefix = prefix + "/" if prefix else ""
            with LOCK:
                names = set()
                for key in VAULT:
                    if key.startswith(prefix):
                        rest = key[len(prefix):]
                        names.add(rest.split("/")[0] + "/" if "/" in rest else rest)
            if not names:
                return self.err(404, 40400, "File does not exist.")
            return self.send_json(200, {"files": sorted(names)})
        # file path with optional target suffix
        target = None
        for i, s in enumerate(segs):
            if i > 0 and s in ("heading", "block", "frontmatter") and segs[i - 1].endswith(".md"):
                target = (s, segs[i + 1:])
                segs = segs[:i]
                break
        path = "/".join(segs)
        if path == "slow.md" and method == "GET":
            time.sleep(6)
            return self.send(200, "slow note", MT_MD)
        if path.startswith("Slow/") and method == "GET":
            # Slow but within the per-exchange deadline: exercises the
            # per-call wall-clock budget of batch_get_file_contents.
            time.sleep(STATE["slow_dir_delay_ms"] / 1000)
            return self.send(200, f"slow note {path}", MT_MD)
        if path == "Binary.png" and method == "GET":
            return self.send(200, b"\x89PNG\r\n\x1a\n" + b"\x00" * 64, "image/png")
        if path == "Forbidden.md":
            return self.err(403, 40300, "Access to this file is forbidden by plugin settings")
        if path == "RateLimited.md":
            return self.err(429, 42900, "Too many requests", {"Retry-After": "7"})
        if path == "Crash.md":
            return self.err(500, 50020, "File operation failed: vault adapter threw")
        with LOCK:
            is_folder = any(k.startswith(path + "/") for k in VAULT)
            note = copy.deepcopy(VAULT.get(path))
        if is_folder and path not in VAULT:
            return self.err(405, 40510, "Path is a directory; use a trailing slash to list it")
        if method == "GET":
            if note is None:
                return self.err(404, 40400, "File does not exist.")
            return self.serve_note(path, note, target, self.headers.get("Target-Scope"))
        if method == "PUT":
            with LOCK:
                VAULT[path] = {"content": body.decode("utf-8", "replace"), "mtime": NOW_MS, "ctime": (note or {}).get("ctime", NOW_MS)}
            return self.send(204)
        if method == "POST":
            new = body.decode("utf-8", "replace")
            if target is not None:
                # _vaultPatchTargeted: the exact-match Content-Type guard runs
                # before the note is even looked up.
                if self.headers.get("Content-Type") not in WRITE_CONTENT_TYPES:
                    return self.err(400, 40012, INVALID_CT_MSG)
                if note is None:
                    return self.err(404, 40400, "File does not exist.")
                if target[0] != "heading":
                    return self.err(400, 40080, "append only supports heading targets in this fixture")
                sec = find_section(note["content"], target[1])
                if sec is None:
                    return self.err(404, 40400, "heading not found")
                if self.headers.get("Reject-If-Content-Preexists") == "true" and new in note["content"]:
                    return self.err(409, 40910, "Content already exists in the target")
                updated = apply_patch(path, note, "heading", target[1], "append", "content", new, None, False)
                with LOCK:
                    VAULT[path]["content"] = updated
                    VAULT[path]["mtime"] = NOW_MS
                return self.send(200, updated, MT_MD)
            # _vaultPost: any text/* body is appended unconditionally — the
            # plugin never reads Reject-If-Content-Preexists on this path.
            if not (self.headers.get("Content-Type") or "").startswith("text/"):
                return self.err(400, 40010, TEXT_REQUIRED_MSG)
            old = note["content"] if note else ""
            with LOCK:
                VAULT[path] = {"content": (old + ("\n" if old and not old.endswith("\n") else "") + new), "mtime": NOW_MS, "ctime": (note or {}).get("ctime", NOW_MS)}
            return self.send(204)
        if method == "PATCH":
            if note is None:
                return self.err(404, 40400, "File does not exist.")
            return self.patch(path, note, body)
        if method == "DELETE":
            if note is None:
                return self.err(404, 40400, "File does not exist.")
            with LOCK:
                VAULT.pop(path, None)
            return self.send(204)
        return self.err(405, 40510, "Method not allowed")

    def serve_note(self, path, note, target, scope, extra=None):
        accept = self.headers.get("Accept", "")
        text = note["content"]
        if target is not None:
            kind, spec = target
            if kind == "heading":
                sec = find_section(text, spec)
                if sec is None:
                    return self.err(404, 40400, f"heading {' > '.join(spec)} not found")
                lines = text.splitlines()
                start, end, _ = sec
                if scope == "marker":
                    text = lines[start] + "\n"
                elif scope == "markerAndContent":
                    text = "\n".join(lines[start:end]) + "\n"
                else:
                    text = "\n".join(lines[start + 1:end]).strip("\n") + "\n"
            elif kind == "block":
                found = blocks(text).get((spec[0] if spec else "").lstrip("^"))
                if found is None:
                    return self.err(404, 40400, "block not found")
                start, end = found
                lines = text.splitlines()
                text = "\n".join(BLOCK_RE.sub("", l) for l in lines[start:end + 1] if not BLOCK_LINE_RE.match(l)) + "\n"
            elif kind == "frontmatter":
                fm, _ = split_frontmatter(text)
                data = parse_frontmatter(fm)
                key = spec[0] if spec else ""
                if key not in data:
                    return self.err(404, 40400, f"frontmatter field {key} not found")
                return self.send_json(200, data[key], extra)
            return self.send(200, text, MT_MD, extra)
        if MT_NOTE in accept:
            return self.send_json(200, note_json(path, note), extra)
        if MT_MAP in accept:
            return self.send_json(200, document_map(path, note), extra)
        return self.send(200, text, MT_MD, extra)

    def patch(self, path, note, body):
        ctype = (self.headers.get("Content-Type") or "").split(";")[0].strip()
        major = int(STATE["version"].split(".")[0])
        if major >= 5:
            if self.headers.get("Target-Type") and not self.headers.get("Markdown-Patch-Version"):
                return self.err(400, 40084, "Header-based targeting requires an explicit Markdown-Patch-Version header on this plugin version")
            if ctype not in (MT_PATCH, "application/json"):
                return self.err(400, 40081, f"Invalid patch instruction: unsupported Content-Type {ctype}")
            try:
                instr = json.loads(body)
            except ValueError:
                return self.err(400, 40081, "Invalid patch instruction: body is not JSON")
            if instr.get("content") == "INVALID-INSTRUCTION":
                return self.err(400, 40081, "Invalid patch instruction: unsupported operation/scope/targetType combination")
            if "content" in instr and "value" in instr:
                return self.err(400, 40081, "Invalid patch instruction: content and value are mutually exclusive")
            if instr.get("ifMatch") is not None and instr["ifMatch"] != "v1":
                return self.err(412, 41200, "Precondition failed: document changed")
            target = instr.get("target")
            warnings = None
            if instr.get("targetType") == "heading" and isinstance(target, list) and len(target) >= 6:
                warnings = quote(json.dumps([{"code": "heading-depth-overflow", "message": "heading level clamped to 6"}]), safe="")
            updated = apply_patch(path, note, instr.get("targetType"), target, instr.get("operation"),
                                  instr.get("scope", "content"), instr.get("content"), instr.get("value"),
                                  bool(instr.get("createTargetIfMissing")))
            with LOCK:
                VAULT[path]["content"] = updated
                VAULT[path]["mtime"] = NOW_MS
            extra = {"Markdown-Patch-Warnings": warnings} if warnings else None
            return self.send(200, updated, MT_MD, extra)
        # legacy (< 5) `_vaultPatch`: header targeting only, in the plugin's
        # check order — a JSON instruction carries no Target-Type header, so it
        # is answered 40053; then Operation/Target; then the exact-match
        # Content-Type guard (40012).
        op = self.headers.get("Operation")
        tt = self.headers.get("Target-Type")
        tg = self.headers.get("Target")
        if not tt:
            return self.err(400, 40053, "No 'Target-Type' header was provided.")
        if not op:
            return self.err(400, 40056, "No 'Operation' header was provided.")
        if not tg:
            return self.err(400, 40055, "No 'Target' header was provided.")
        if self.headers.get("Content-Type") not in WRITE_CONTENT_TYPES:
            return self.err(400, 40012, INVALID_CT_MSG)
        tg = unquote(tg)
        target = tg.split("::") if tt == "heading" else tg
        if ctype == "application/json":
            value, content = json.loads(body or b"null"), None
        else:
            value, content = None, body.decode("utf-8", "replace")
        updated = apply_patch(path, note, tt, target, op, "content", content, value,
                              self.headers.get("Create-Target-If-Missing") == "true")
        with LOCK:
            VAULT[path]["content"] = updated
            VAULT[path]["mtime"] = NOW_MS
        return self.send(200, updated, MT_MD, {"Deprecation": 'true; sunset-version="6.0"'})

    # /search/simple/
    def search_simple(self, method, qs):
        if method != "POST":
            return self.err(405, 40510, "POST only")
        q = qs.get("query")
        if not q:
            return self.err(400, 40090, "Invalid search: query is required")
        if q == "boom":
            return self.err(500, 50010, "Error preparing simple search")
        ctx = qs.get("contextLength", "100")
        echo = f"[query={q}][contextLength={ctx}]"
        results = []
        if q == "many":
            for i in range(500):
                results.append({"filename": f"Many/note-{i:03d}.md", "score": 500 - i,
                                "matches": [{"match": {"start": 0, "end": 4, "source": "content"}, "context": f"many {i}"} for _ in range(20)]})
        else:
            with LOCK:
                items = list(VAULT.items())
            for path, note in items:
                lower = note["content"].lower()
                base = path.rsplit("/", 1)[-1].lower()
                matches = []
                if q.lower() in base:
                    matches.append({"match": {"start": 0, "end": len(q), "source": "filename"}, "context": path})
                start = 0
                while True:
                    idx = lower.find(q.lower(), start)
                    if idx == -1 or len(matches) > 30:
                        break
                    matches.append({"match": {"start": idx, "end": idx + len(q), "source": "content"},
                                    "context": note["content"][max(0, idx - 20): idx + len(q) + 20]})
                    start = idx + 1
                if matches:
                    results.append({"filename": path, "score": float(len(matches)), "matches": matches})
            results.sort(key=lambda r: -r["score"])
        if results:
            results[0]["matches"][0]["context"] = echo + " " + results[0]["matches"][0]["context"]
        results.append({"filename": "_fixture/echo.md", "score": 0,
                        "matches": [{"match": {"start": 0, "end": 0, "source": "content"}, "context": echo}]})
        return self.send_json(200, results)

    # /search/
    def search(self, method, body):
        if method != "POST":
            return self.err(405, 40510, "POST only")
        ctype = (self.headers.get("Content-Type") or "").split(";")[0].strip()
        if ctype != MT_JL:
            return self.err(400, 40012, f"Invalid content type {ctype}; use {MT_JL}")
        try:
            rule = json.loads(body)
        except ValueError:
            return self.err(400, 40070, "Invalid filter query: body is not JSON")
        raw = body.decode("utf-8", "replace")
        out = []
        with LOCK:
            items = list(VAULT.items())
        for path, note in items:
            data = note_json(path, note)
            if '"content"' not in raw:
                data.pop("content")
            try:
                result = jl(rule, data)
            except Exception as e:
                return self.err(400, 40070, f"Invalid filter query: {e}")
            if truthy(result):
                out.append({"filename": path, "result": result})
        return self.send_json(200, out)

    def tags(self):
        counts = {}
        with LOCK:
            items = list(VAULT.items())
        for _, note in items:
            for tag in note_tags(note["content"]):
                parts = tag.split("/")
                for i in range(1, len(parts) + 1):
                    name = "/".join(parts[:i])
                    counts[name] = counts.get(name, 0) + 1
        return self.send_json(200, {"tags": [{"name": n, "count": c} for n, c in sorted(counts.items())]})

    def periodic(self, segs):
        # The companion plugin "Local REST API - Periodic Notes" (src/routes.ts,
        # src/types.ts, src/constants.ts): a 307 to the resolved /vault/ path,
        # or its {errorCode, message} envelope — 40060 period not enabled (400),
        # 40460 period unknown (404), 40461 no note for that date (404), 50060
        # operation failed (500). Without the companion the core plugin's
        # notFoundHandler answers 404 errorCode 40400 "Not Found".
        if not STATE["periodic"]:
            if STATE["periodic_envelope"]:
                return self.err(404, 40400, "Not Found")
            return self.send(404, "Not Found", "text/plain")
        if STATE["periodic_delay_ms"]:
            time.sleep(STATE["periodic_delay_ms"] / 1000)
        if STATE["periodic_fail"]:
            return self.err(500, 50060, "Periodic note operation failed unexpectedly.")
        if STATE["periodic_code"]:
            return self.err(404, int(STATE["periodic_code"]), "Something else entirely.")
        period = segs[0] if segs else ""
        if period not in ("daily", "weekly", "monthly", "quarterly", "yearly") or period in STATE["periodic_unknown"]:
            return self.err(404, 40460, "Specified period does not exist.")
        if period in STATE["periodic_disabled"]:
            return self.err(400, 40060, "Specified period is not enabled.")
        rest = [s for s in segs[1:] if s != ""]
        if len(rest) == 3:
            try:
                y, m, d = (int(x) for x in rest)
                date = datetime(y, m, d).date()
            except ValueError:
                return self.err(400, 40000, "Invalid date")
        else:
            date = TODAY + timedelta(days=STATE["periodic_today_shift"])
        path = f"Daily/{date.isoformat()}.md"
        with LOCK:
            exists = path in VAULT
        if not exists and not STATE["periodic_redirect_missing"]:
            # periodicGetNote: period.get() found no file -> no redirect.
            return self.err(404, 40461, "Periodic note does not exist for the specified period.")
        loc = "/vault/" + "/".join(quote(s, safe="") for s in path.split("/"))
        return self.send(307, "", "text/plain", {"Location": loc, "Content-Location": loc})


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
