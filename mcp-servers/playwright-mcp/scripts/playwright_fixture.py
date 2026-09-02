#!/usr/bin/env python3
"""Hermetic stand-in for `@playwright/mcp` 0.0.80 in streamable-HTTP mode.

Impersonates exactly the wire behaviour probed against the real server on
2026-09-02 (the MCP TypeScript SDK's StreamableHTTPServerTransport, MIT):

- POST /mcp needs `Content-Type: application/json` (else 415) and an Accept
  that lists both application/json and text/event-stream (else 406);
- `initialize` (no session header) answers 200 text/event-stream with an
  `mcp-session-id` header; any other method without the header is
  400 `Bad Request: Server not initialized`; an unknown session is
  404 `Session not found`; `notifications/*` answer 202;
- `ping`, `tools/list`, `tools/call` answer SSE frames
  (`event: message` / `data: {...}`);
- DELETE /mcp with a valid session forgets it (200), else 404;
- a Host allow-list mode answers 403 `Access is only allowed at …`;
- a bearer-token mode answers 401 with WWW-Authenticate.

tools/call results mirror the upstream's section format (`### Ran Playwright
code`, `### Page`, `### Snapshot` with a file link, `### Error`, `### Modal
state`). Every call is recorded (arguments as received, session, headers) so
the e2e can assert encoding, clamping and headers through /__fixture/stats.

Threaded: the harness fires 8 concurrent calls.

Usage: playwright_fixture.py <port>
"""
import json
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
STATE = {}

# A 1x1 transparent PNG (67 bytes) — the image payload for screenshots.
TINY_PNG = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)


def reset_state():
    STATE.clear()
    STATE.update(
        {
            "initialize_count": 0,
            "delete_count": 0,
            "sessions": {},
            "calls": [],
            "requests": [],
            "snapshot_counter": 0,
            "mode": {
                "host_check": None,  # e.g. "localhost:8931" -> 403 unless Host matches
                "require_token": None,  # e.g. "test-token" -> 401 unless Bearer matches
                "fail_status": None,  # next tools/call answers this HTTP status
                "fail_body": None,
                "fail_once": True,
                "retry_after": None,
                "snapshot_error": False,  # browser_snapshot answers isError
            },
        }
    )


reset_state()


def schema(props, required=None):
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": props,
        **({"required": required} if required else {}),
        "additionalProperties": False,
    }


def ann(title, read_only):
    return {
        "title": title,
        "readOnlyHint": read_only,
        "destructiveHint": not read_only,
        "openWorldHint": True,
    }


TARGET = {"type": "string", "description": "Exact target element reference from the page snapshot, or a unique element selector"}
ELEMENT = {"type": "string", "description": "Human-readable element description used to obtain permission to interact with the element"}
FILENAME = {"type": "string", "description": "File name to save the output to."}

TOOLS = [
    {
        "name": "browser_navigate",
        "description": "Navigate to a URL",
        "inputSchema": schema({"url": {"type": "string", "description": "The URL to navigate to"}}, ["url"]),
        "annotations": ann("Navigate to a URL", False),
    },
    {
        "name": "browser_snapshot",
        "description": "Capture accessibility snapshot of the current page, this is better than screenshot",
        "inputSchema": schema(
            {
                "target": TARGET,
                "filename": {"type": "string", "description": "Save snapshot to markdown file instead of returning it in the response."},
                "depth": {"type": "number", "description": "Limit the depth of the snapshot tree"},
                "boxes": {"type": "boolean", "description": "Include bounding boxes"},
            }
        ),
        "annotations": ann("Page snapshot", True),
    },
    {
        "name": "browser_click",
        "description": "Perform click on a web page",
        "inputSchema": schema(
            {
                "element": ELEMENT,
                "target": TARGET,
                "doubleClick": {"type": "boolean"},
                "button": {"type": "string", "enum": ["left", "right", "middle"]},
                "modifiers": {"type": "array", "items": {"type": "string", "enum": ["Alt", "Control", "ControlOrMeta", "Meta", "Shift"]}},
            },
            ["target"],
        ),
        "annotations": ann("Click", False),
    },
    {
        "name": "browser_type",
        "description": "Type text into editable element",
        "inputSchema": schema(
            {"element": ELEMENT, "target": TARGET, "text": {"type": "string"}, "submit": {"type": "boolean"}, "slowly": {"type": "boolean"}},
            ["target", "text"],
        ),
        "annotations": ann("Type text", False),
    },
    {
        "name": "browser_take_screenshot",
        "description": "Take a screenshot of the current page. You can't perform actions based on the screenshot, use browser_snapshot for actions.",
        "inputSchema": schema(
            {
                "element": ELEMENT,
                "target": TARGET,
                "type": {"type": "string", "enum": ["png", "jpeg", "webp"], "description": "Image format for the screenshot. If unset, inferred from the filename extension, otherwise png."},
                "filename": FILENAME,
                "fullPage": {"type": "boolean"},
                "scale": {"type": "string", "enum": ["css", "device"], "default": "css"},
            },
            ["scale"],
        ),
        "annotations": ann("Take a screenshot", True),
    },
    {
        "name": "browser_wait_for",
        "description": "Wait for text to appear or disappear or a specified time to pass",
        "inputSchema": schema({"time": {"type": "number", "description": "The time to wait in seconds"}, "text": {"type": "string"}, "textGone": {"type": "string"}}),
        "annotations": ann("Wait for", True),
    },
    {
        "name": "browser_console_messages",
        "description": "Returns all console messages",
        "inputSchema": schema(
            {"level": {"type": "string", "enum": ["error", "warning", "info", "debug"], "default": "info"}, "all": {"type": "boolean"}, "filename": FILENAME},
            ["level"],
        ),
        "annotations": ann("Get console messages", True),
    },
    {
        "name": "browser_resize",
        "description": "Resize the browser window",
        "inputSchema": schema({"width": {"type": "number"}, "height": {"type": "number"}}, ["width", "height"]),
        "annotations": ann("Resize browser window", False),
    },
    {
        "name": "browser_tabs",
        "description": "List, create, close, or select a browser tab.",
        "inputSchema": schema({"action": {"type": "string", "enum": ["list", "new", "close", "select"]}, "index": {"type": "number"}, "url": {"type": "string"}}, ["action"]),
        "annotations": ann("Manage tabs", False),
    },
    {
        "name": "browser_handle_dialog",
        "description": "Handle a dialog",
        "inputSchema": schema({"accept": {"type": "boolean"}, "promptText": {"type": "string"}}, ["accept"]),
        "annotations": ann("Handle a dialog", False),
    },
    {
        "name": "browser_close",
        "description": "Close the page",
        "inputSchema": schema({}),
        "annotations": ann("Close browser", False),
    },
    {
        "name": "browser_run_code_unsafe",
        "description": "Run a Playwright code snippet. Unsafe: executes arbitrary JavaScript in the Playwright server process and is RCE-equivalent.",
        "inputSchema": schema({"code": {"type": "string"}, "filename": {"type": "string", "description": "Load code from the specified file."}}),
        "annotations": ann("Run Playwright code (unsafe)", False),
    },
    {
        "name": "browser_evaluate",
        "description": "Evaluate JavaScript expression on page or element",
        "inputSchema": schema({"function": {"type": "string"}, "element": ELEMENT, "target": TARGET, "filename": FILENAME}, ["function"]),
        "annotations": ann("Evaluate JavaScript", False),
    },
]

ANSI_ERROR = (
    "### Error\nError: browserBackend.callTool: net::ERR_UNSAFE_PORT at {url}\nCall log:\n"
    "[2m  - navigating to \"{url}\", waiting until \"domcontentloaded\"[22m\n"
)


def sse(payload):
    return ("event: message\ndata: " + json.dumps(payload, ensure_ascii=False) + "\n\n").encode("utf-8")


def text_result(text, is_error=False, extra_blocks=None):
    blocks = [{"type": "text", "text": text}] + (extra_blocks or [])
    result = {"content": blocks}
    if is_error:
        result["isError"] = True
    return result


def page_section(url="http://127.0.0.1:9351/"):
    return "### Page\n- Page URL: %s\n- Page Title: Fixture Page\n- Console: 1 errors, 0 warnings\n" % url


def action_result(code, url="http://127.0.0.1:9351/", extra_sections=""):
    n = STATE["calls"][-1]["index"] if STATE["calls"] else 0
    return text_result(
        "### Ran Playwright code\n```js\n%s\n```\n%s%s### Snapshot\n- [Snapshot](out/page-%d.yml)\n### Events\n- New console entries: out/console-%d.log#L1"
        % (code, page_section(url), extra_sections, n, n)
    )


def snapshot_text():
    STATE["snapshot_counter"] += 1
    n = STATE["snapshot_counter"]
    return (
        page_section()
        + "### Snapshot\n```yaml\n- generic [active] [ref=e1]:\n  - heading \"Hello Playwright\" [level=1] [ref=e2]\n"
        + "  - textbox \"Your name\" [ref=e3]\n  - button \"Go\" [ref=e%d]\n  - paragraph\n```" % (n + 3)
    )


def call_tool(name, args, record):
    echo = json.dumps(
        {
            "name": name,
            "arguments": args,
            "session": record["session"],
            "host": record["host"],
            "authorization": record["authorization"],
            "index": record["index"],
        },
        ensure_ascii=False,
        sort_keys=True,
    )
    known = {t["name"] for t in TOOLS}
    if name not in known:
        return text_result('### Error\nTool "%s" not found' % name, True)
    if name == "browser_navigate":
        url = args.get("url")
        if not isinstance(url, str):
            return text_result('### Error\nInvalid arguments for tool "browser_navigate":\n✖ Invalid input: expected string, received undefined\n  → at url', True)
        if url.startswith("file:"):
            return text_result('### Error\nError: Access to "file:" protocol is blocked. Attempted URL: "%s"' % url, True)
        if "unsafe-port" in url:
            return text_result(ANSI_ERROR.format(url=url), True)
        if "/slow" in url:
            time.sleep(5)
        return action_result("await page.goto('%s');" % url, url)
    if name == "browser_snapshot":
        if STATE["mode"]["snapshot_error"]:
            return text_result("### Error\nError: No open pages available. Use browser_navigate to navigate to a page first.", True)
        return text_result(snapshot_text())
    if name == "browser_click":
        target = args.get("target")
        if not isinstance(target, str):
            return text_result('### Error\nInvalid arguments for tool "browser_click":\n✖ Invalid input: expected string, received undefined\n  → at target', True)
        if target == "e99":
            return text_result("### Error\nError: Ref e99 not found in the current page snapshot. Try capturing new snapshot.", True)
        if target == "e-dialog":
            return action_result(
                "await page.getByRole('button', { name: 'Alert' }).click();",
                extra_sections='### Modal state\n- ["alert" dialog with message "hi"]: can be handled by the "browser_handle_dialog" tool\n',
            )
        return action_result("await page.getByRole('button', { name: 'Go' }).click();")
    if name == "browser_type":
        return text_result("### Ran Playwright code\n```js\nawait page.getByRole('textbox').fill(%s);\n```\n### Result\n%s" % (json.dumps(args.get("text", ""), ensure_ascii=False), echo))
    if name == "browser_take_screenshot":
        kind = args.get("type") or "png"
        return text_result(
            "### Result\n- [Screenshot of viewport](out/page-1.%s)\n%s" % (kind, echo),
            extra_blocks=[{"type": "image", "data": TINY_PNG, "mimeType": "image/" + kind}],
        )
    if name == "browser_wait_for":
        if not any(k in args for k in ("time", "text", "textGone")):
            return text_result("### Error\nError: Either time, text or textGone must be provided", True)
        what = args.get("text") or args.get("textGone") or args.get("time")
        return text_result("### Result\nWaited for %s\n%s### Snapshot\n- [Snapshot](out/page-w.yml)\n### Echo\n%s" % (what, page_section(), echo))
    if name == "browser_close":
        return text_result("### Result\nNo open tabs. Navigate to a URL to create one.\n### Ran Playwright code\n```js\nawait page.close()\n```")
    if name == "browser_handle_dialog":
        return action_result("await dialog.accept();")
    # Everything else echoes what it received so tests can assert encoding,
    # clamps and stripped parameters.
    return text_result("### Result\n" + echo)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send(self, status, body=b"", content_type="text/plain; charset=UTF-8", headers=None):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _json(self, status, payload, headers=None):
        self._send(status, json.dumps(payload).encode(), "application/json", headers)

    def _read_body(self):
        # wasi:http streams request bodies as chunked transfer encoding (no
        # Content-Length); BaseHTTPRequestHandler does not decode that.
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            chunks = []
            while True:
                line = self.rfile.readline().strip()
                size = int(line.split(b";")[0] or b"0", 16)
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
        if self.path == "/__fixture/stats":
            with LOCK:
                payload = {
                    "initialize_count": STATE["initialize_count"],
                    "delete_count": STATE["delete_count"],
                    "sessions": sorted(STATE["sessions"].keys()),
                    "calls": STATE["calls"],
                    "requests": STATE["requests"][-50:],
                    "mode": STATE["mode"],
                }
            self._send(200, json.dumps(payload, ensure_ascii=False).encode("utf-8"), "application/json")
            return
        if self.path == "/":
            self._send(200, b"playwright fixture")
            return
        if self.path == "/mcp":
            self._json(405, {"jsonrpc": "2.0", "error": {"code": -32000, "message": "Method not allowed."}, "id": None})
            return
        self._send(404, b"not found")

    def do_DELETE(self):
        if self.path != "/mcp":
            self._send(404, b"not found")
            return
        sid = self.headers.get("Mcp-Session-Id")
        with LOCK:
            STATE["requests"].append({"method": "DELETE", "session": sid, "authorization": self.headers.get("Authorization")})
            if sid and sid in STATE["sessions"]:
                del STATE["sessions"][sid]
                STATE["delete_count"] += 1
                self._send(200, b"")
                return
        self._send(404, b"Session not found")

    def do_POST(self):
        body = self._read_body()
        if self.path == "/__fixture/expire":
            with LOCK:
                STATE["sessions"].clear()
            self._send(200, b"ok")
            return
        if self.path == "/__fixture/reset":
            with LOCK:
                reset_state()
            self._send(200, b"ok")
            return
        if self.path == "/__fixture/mode":
            with LOCK:
                STATE["mode"].update(json.loads(body or b"{}"))
            self._send(200, b"ok")
            return
        if self.path != "/mcp":
            self._send(404, b"not found")
            return

        host = self.headers.get("Host", "")
        authorization = self.headers.get("Authorization")
        accept = self.headers.get("Accept", "")
        content_type = self.headers.get("Content-Type", "")
        sid = self.headers.get("Mcp-Session-Id")
        protocol_header = self.headers.get("MCP-Protocol-Version")

        with LOCK:
            mode = dict(STATE["mode"])
            STATE["requests"].append(
                {
                    "method": "POST",
                    "session": sid,
                    "accept": accept,
                    "content_type": content_type,
                    "protocol_version": protocol_header,
                    "authorization": authorization,
                    "host": host,
                    "user_agent": self.headers.get("User-Agent"),
                }
            )

        if mode["host_check"] and host != mode["host_check"]:
            self._send(403, ("Access is only allowed at %s" % mode["host_check"]).encode())
            return
        if mode["require_token"] and authorization != "Bearer " + mode["require_token"]:
            self._json(401, {"error": "unauthorized", "error_description": "a bearer token for this Playwright endpoint is required"}, {"WWW-Authenticate": 'Bearer realm="playwright"'})
            return
        if "application/json" not in accept or "text/event-stream" not in accept:
            self._json(406, {"jsonrpc": "2.0", "error": {"code": -32000, "message": "Not Acceptable: Client must accept both application/json and text/event-stream"}, "id": None})
            return
        if not content_type.startswith("application/json"):
            self._json(415, {"jsonrpc": "2.0", "error": {"code": -32000, "message": "Unsupported Media Type: Content-Type must be application/json"}, "id": None})
            return
        try:
            message = json.loads(body.decode("utf-8"))
        except Exception:
            self._json(400, {"jsonrpc": "2.0", "error": {"code": -32700, "message": "Parse error: invalid JSON"}, "id": None})
            return
        method = message.get("method")
        rid = message.get("id")
        params = message.get("params") or {}

        if method == "initialize":
            new_sid = "fx-" + uuid.uuid4().hex[:12]
            with LOCK:
                STATE["initialize_count"] += 1
                STATE["sessions"][new_sid] = {"calls": 0, "client": params.get("clientInfo"), "protocolVersion": params.get("protocolVersion")}
            payload = {
                "result": {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "Playwright", "version": "fixture"}},
                "jsonrpc": "2.0",
                "id": rid,
            }
            self._send(200, sse(payload), "text/event-stream", {"mcp-session-id": new_sid, "Cache-Control": "no-cache"})
            return

        if not sid:
            self._json(400, {"jsonrpc": "2.0", "error": {"code": -32000, "message": "Bad Request: Server not initialized"}, "id": None})
            return
        with LOCK:
            if sid not in STATE["sessions"]:
                known = False
            else:
                known = True
                STATE["sessions"][sid]["calls"] += 1
        if not known:
            self._send(404, b"Session not found")
            return

        if isinstance(method, str) and method.startswith("notifications/"):
            self._send(202, b"", "text/plain; charset=UTF-8")
            return
        if method == "ping":
            self._send(200, sse({"result": {}, "jsonrpc": "2.0", "id": rid}), "text/event-stream")
            return
        if method == "tools/list":
            self._send(200, sse({"result": {"tools": TOOLS}, "jsonrpc": "2.0", "id": rid}), "text/event-stream")
            return
        if method == "tools/call":
            name = params.get("name")
            args = params.get("arguments")
            if args is None:
                args = {}
            with LOCK:
                if mode["fail_status"]:
                    status = mode["fail_status"]
                    fail_body = mode["fail_body"] or ("HTTP %d from fixture" % status)
                    headers = {"Retry-After": mode["retry_after"]} if mode["retry_after"] else {}
                    if mode["fail_once"]:
                        STATE["mode"]["fail_status"] = None
                        STATE["mode"]["fail_body"] = None
                        STATE["mode"]["retry_after"] = None
                    self._send(status, fail_body.encode(), "text/plain; charset=UTF-8", headers)
                    return
                record = {
                    "index": len(STATE["calls"]) + 1,
                    "name": name,
                    "arguments": args,
                    "session": sid,
                    "host": host,
                    "authorization": authorization,
                    "protocol_version": protocol_header,
                    "params_keys": sorted(params.keys()),
                }
                STATE["calls"].append(record)
                result = call_tool(name, args, record)
            self._send(200, sse({"result": result, "jsonrpc": "2.0", "id": rid}), "text/event-stream")
            return
        self._send(200, sse({"jsonrpc": "2.0", "error": {"code": -32601, "message": "Method not found"}, "id": rid}), "text/event-stream")


if __name__ == "__main__":
    port = int(sys.argv[1])
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    server.serve_forever()
