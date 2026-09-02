---
name: playwright-mcp
description: Drive a real web browser (navigate, read the accessibility snapshot, click, type, fill forms, wait for text, screenshot, inspect console and network) through the official Playwright MCP server running on the developer's machine. Use when a task needs to open, test, scrape or interact with a web page or a locally running web app, and to interpret this server's setup and browser errors.
---

# Using the playwright-mcp MCP server

This server is a sandboxed WebAssembly **proxy** on Cosmonic Desktop. The
browser itself runs in the official Playwright MCP server (`@playwright/mcp`)
on the developer's machine; this proxy forwards its tools, keeps one browser
session per warm instance, hides the unsafe tools, and inlines the page
snapshot after every action so you can act without host filesystem access.
Tool tables: [references/TOOLS.md](references/TOOLS.md). Upstream flags and
Desktop grants: [references/SETUP.md](references/SETUP.md).

## Start here

1. **Call `playwright_status` first.** `status: ok` means the upstream
   answered; anything else carries the exact remediation in `remediation`
   (start command with `--host 127.0.0.1 --allowed-hosts …`, the three loopback
   grants, or the bearer token). `unreachable`, `host_rejected`, `missing` and
   `invalid` are permanent until a human changes the setup — **never retry
   them in a loop**; report the remediation text.
2. `tools/list` shows the upstream's tool set. If it lists only
   `playwright_status` and `playwright_reset_session`, the upstream was not
   reachable when the client connected: fix the setup, then reconnect (or
   re-list tools).
3. `browser_navigate` a URL, then work from the inlined snapshot.

## Snapshot before you act, re-snapshot after

- Element refs (`e4`, `e17`) come **only** from `browser_snapshot` /
  `browser_find` (or the snapshot inlined into an action result). They are
  renumbered on every new snapshot and navigation. A stale one fails with
  `Ref e77 not found in the current page snapshot. Try capturing new snapshot.`
  — take a new snapshot, do not guess.
- The upstream's action results only carry a file link
  (`- [Snapshot](out/page-….yml)`) that this sandbox cannot read. With
  `PLAYWRIGHT_AUTO_SNAPSHOT=true` (default) the proxy replaces that line with
  a fresh inline ```yaml snapshot; the `### Page`, `### Modal state` and
  `### Events` sections stay as the upstream wrote them. If the operator set
  it to `false`, call `browser_snapshot` yourself after every action.
- For big pages use `browser_find` (text or regex) or `browser_snapshot` with
  `target`/`depth` instead of dumping the whole tree; text blocks are cut at
  1 MB with a `…[truncated …]` marker.
- Screenshots are for humans (`browser_take_screenshot`, jpeg viewport by
  default). You cannot find refs in a picture.

## Argument names that trip agents

- The element parameter is **`target`** (a snapshot ref like `"e4"` or a
  unique selector), not `ref` or `selector`; `element` is an optional
  human-readable description. Sending `ref` yields
  `Invalid arguments for tool "browser_click": ✖ Invalid input: expected string, received undefined → at target`.
  `browser_fill_form` items likewise need `target`, `name`, `type`, `value`.
- `browser_wait_for` needs exactly one of `time` (seconds, clamped to 60
  here), `text` or `textGone`; an empty call fails with
  `Either time, text or textGone must be provided`.
- `browser_navigate` only waits for `domcontentloaded` plus a 500 ms settle;
  for SPA content follow with `browser_wait_for { text }`.
- `filename` does not exist through this proxy (stripped everywhere: files
  would land on the developer's disk). Results always come back inline; a
  `### Proxy notes` block says when something was stripped or clamped.
- `browser_take_screenshot`: `type` defaults to `jpeg` here; `fullPage: true`
  PNGs can exceed the reply-size cap.

## Session model

- One upstream browser session per warm proxy instance (`poolSize: 1`).
  `browser_close` closes the page only (next navigate opens a new one; a
  snapshot after close shows `about:blank` with an empty tree).
- `playwright_reset_session` DELETEs the upstream session: cookies, tabs and
  page state are gone; the next call initializes a fresh context. Use it to
  recover from a wedged page or a dialog you cannot clear.
- If Desktop recycles the instance or the upstream restarts, the next call
  re-initializes transparently (the proxy retries once on `Session not
  found`) but page state is lost: re-navigate. The operator can launch the
  upstream with `--shared-browser-context` to keep tabs across that.
- Long calls hold the instance: navigation can take up to 60 s upstream and
  `browser_wait_for` up to 60 s; the outbound deadline is 90 s
  (`MCP_OUTBOUND_TIMEOUT_MS`).

## The browser is on the developer's machine, not in the sandbox

- `browser_navigate` to `http://localhost:3000` reaches **their** localhost
  (unlike this component, which needs a loopback grant for anything local).
- `file:` URLs are blocked upstream (`Access to "file:" protocol is blocked`)
  unless the operator passed `--allow-unrestricted-file-access`; serve the
  file over HTTP instead.
- `browser_evaluate`, `browser_run_code_unsafe`, `browser_file_upload` and
  `browser_drop` are hidden from `tools/list` and refused with
  `… is disabled: set PLAYWRIGHT_ALLOW_UNSAFE=true …` unless the operator
  enabled them. Do not work around the gate; use click/type/fill_form.
- Extra tools (`browser_pdf_save`, `browser_mouse_*_xy`, tracing) exist only
  when the upstream runs with `--caps pdf,vision,devtools`;
  `Tool "browser_pdf_save" not found` means that cap is off, not a proxy bug.

## Dialogs

A result with a `### Modal state` section (alert/confirm/prompt, file
chooser) means the page is blocked: nothing else works until
`browser_handle_dialog { accept, promptText? }`. Then re-snapshot.

## Error catalogue

| You see | It means | Do |
|---|---|---|
| `playwright_status` `unreachable` / "Could not reach the Playwright MCP server at …" | upstream not running on that port, bound to `::1` only (missing `--host 127.0.0.1`), or a loopback grant missing (allowedHosts, allowedHostLoopbackPorts, Settings → Security) | Give the user the `launch_command` and the three grants; permanent until fixed |
| "… denied …" / "policy" in the same message | Desktop refused the outbound request: `allowedHosts` / `allowedHostLoopbackPorts` mismatch | Fix the manifest; retrying is pointless |
| "DnsError … address not available" / "sentinel name did not resolve" in the same message | `host.wasmcloud.internal` does not resolve while Settings → Security → allow host loopback is off (or the port grant is missing) | The user flips the door on (and checks the port grant); retrying is pointless |
| `host_rejected` / HTTP 403 "Access is only allowed at localhost:8931" | upstream Host allow-list lacks `host.wasmcloud.internal:8931` | Restart upstream with `--allowed-hosts host.wasmcloud.internal:8931,localhost:8931` (comma form) |
| "PLAYWRIGHT_BEARER_TOKEN is not set and the … endpoint demands authentication" (HTTP 401/403) | a hosted upstream needs a token; secret ref `playwright-mcp-token` not registered | Ask the user to register it; the local npx server never needs one |
| "… rejected the bearer token (HTTP 401 …)" | token wrong/expired | Replace the `playwright-mcp-token` secret; do not retry |
| HTTP 429 "rate limiting" | an authenticating proxy throttled the call | Honour `Retry-After`; no tight loops |
| HTTP 406 / 415 in an error | proxy bug (Accept / Content-Type) | Report it; not user-fixable |
| HTTP 5xx | upstream failure | Retry once after a few seconds |
| "timed out after 90000 ms" / "timed out" | navigation or wait exceeded the outbound deadline | Split the wait (`browser_wait_for` ≤ 60 s), retry once; raise `MCP_OUTBOUND_TIMEOUT_MS` if it recurs |
| "response body exceeded the outbound size limit" | fullPage PNG or a huge snapshot | jpeg viewport shot, `browser_snapshot` with `depth`/`target`, or raise `MCP_OUTBOUND_MAX_BYTES` |
| "The upstream browser session was lost twice in a row" | upstream restarting / evicting sessions | `playwright_reset_session`, then `playwright_status` |
| `isError` "Ref eNN not found in the current page snapshot" | stale ref | `browser_snapshot` / `browser_find`, use the new ref |
| `isError` "Invalid arguments for tool … → at target" | wrong parameter name (`ref`, `selector`) or missing required field | Fix the argument names per `tools/list` |
| `isError` "Either time, text or textGone must be provided" | empty `browser_wait_for` | Provide one condition |
| `isError` "Access to \"file:\" protocol is blocked" | `file:` navigation | Serve over http, or the operator adds `--allow-unrestricted-file-access` |
| `isError` "net::ERR_…" / "Timeout 60000ms exceeded" with a call log | the browser on the developer's machine could not load the URL | Check the URL from their side; retry; then `browser_wait_for { text }` |
| `isError` "Tool \"browser_x\" not found" | not offered at the upstream's current `--caps`, or a stale list | Operator adds `--caps …`; `playwright_reset_session` refreshes the list |
| "… is disabled: set PLAYWRIGHT_ALLOW_UNSAFE=true …" | gated tool called | Use click/type/fill_form; the operator decides on the gate |
| JSON-RPC `-32602` "unknown tool" | a name neither local nor offered by the upstream | Re-read `tools/list` |
| `### Modal state` in a result | page blocked on a dialog | `browser_handle_dialog`, then re-snapshot |
| "No open tabs. Navigate to a URL to create one." / snapshot of `about:blank` | after `browser_close` or a fresh session | `browser_navigate` |

## Reading results

- `"isError": true` inside a `result` — the tool ran and failed; the text is
  written for you: surface it (upstream messages pass through verbatim, ANSI
  colour codes removed).
- JSON-RPC `error` `-32602` — the request itself was malformed (missing /
  ill-typed params, unknown tool). Fix the call; it is not an outage.
- HTTP `403 Forbidden` before any JSON-RPC body — this proxy's own Host guard
  (`MCP_ALLOWED_HOSTS`), not the upstream's. HTTP `413` — request body over
  the transport cap.
- `playwright_status` and `playwright_reset_session` return
  `structuredContent`; browser tools return the upstream's content blocks
  (text, images) plus a `### Proxy notes` text block when the proxy changed
  something.
