# Tool reference — playwright-mcp

Two tools are implemented by the proxy; everything else is the upstream
Playwright MCP server's own list (`@playwright/mcp` 0.0.80: 24 core tools,
more with `--caps`), forwarded with the changes in the last column. Argument
schemas come from `tools/list`; this table is the operating summary.

## Local tools

| Tool | Arguments | Returns | Notes |
|---|---|---|---|
| `playwright_status` | none | `structuredContent`: `status` (`ok` / `unreachable` / `host_rejected` / `missing` / `invalid` / `error`), `reachable`, `base_url`, `via_loopback`, `upstream {name, version, protocolVersion}`, `session {cached, initializes_on_this_instance}`, `tools {cached, cache_age_secs, ttl_secs}`, `allow_unsafe`, `auto_snapshot`, `token_configured`, `credential {ref, env, required: false}`, `remediation`, `hints[]`, `launch_command` | Read-only. `isError` when not `ok`. Sends `ping` in the cached session (initializing one if needed). |
| `playwright_reset_session` | none | `structuredContent`: `status` (`reset` / `nothing_to_reset` / `reset_local_only`), `had_session`, `upstream_status` | DELETEs the upstream session and clears the tool cache. Destructive: tabs, cookies, page state gone. |

## Upstream tools (forwarded)

Refs (`target`) come from a snapshot. `element` is an optional human
description everywhere it appears. Annotations (`readOnlyHint`,
`destructiveHint`) pass through from the upstream.

| Tool | Key arguments | Result shape | Proxy changes |
|---|---|---|---|
| `browser_navigate` | `url` (http/https; `file:` blocked upstream) | `### Ran Playwright code`, `### Page` (URL, title, console counts), `### Snapshot`, `### Events` | snapshot inlined |
| `browser_navigate_back` | — | as above | snapshot inlined |
| `browser_snapshot` | `target?`, `depth?` (1..64), `boxes?` | `### Page` + ```yaml tree with `[ref=eN]` | `depth` clamped; `filename` stripped |
| `browser_find` | `text?` or `regex?` | `### Result` with the matching subtree | — |
| `browser_click` | `target`, `doubleClick?`, `button?` (left/right/middle), `modifiers?` | action + snapshot | snapshot inlined |
| `browser_type` | `target`, `text`, `submit?`, `slowly?` | `### Ran Playwright code` (no snapshot link → nothing inlined; snapshot if you need refs) | — |
| `browser_fill_form` | `fields[{target, name, type (textbox/checkbox/radio/combobox/slider), value}]` | action + snapshot | snapshot inlined |
| `browser_press_key` | `key` (e.g. `Enter`, `ArrowDown`) | action + snapshot | snapshot inlined |
| `browser_hover` | `target` | action + snapshot | snapshot inlined |
| `browser_drag` | `startTarget`, `endTarget` | action + snapshot | snapshot inlined |
| `browser_select_option` | `target`, `values[]` | action + snapshot | snapshot inlined |
| `browser_wait_for` | exactly one of `time` (s), `text`, `textGone` | `### Result Waited for …` + snapshot | `time` clamped 0..60; snapshot inlined |
| `browser_take_screenshot` | `type?` (png/jpeg/webp), `target?`, `fullPage?`, `scale?` (css/device) | text link + `image` content block (base64) | `type` defaults to `jpeg`; `filename` stripped; fullPage warning |
| `browser_tabs` | `action` (list/new/close/select), `index?`, `url?` | `### Result` tab list | negative `index` clamped to 0 |
| `browser_resize` | `width`, `height` | action | clamped 1..8192 |
| `browser_console_messages` | `level` (error/warning/info/debug), `all?` | `### Result` message list | `filename` stripped |
| `browser_network_requests` | `static` (bool), `filter?` | `### Result` request list with indexes | `filename` stripped |
| `browser_network_request` | `index`, `part?` (request-headers/request-body/response-headers/response-body) | the requested part | `filename` stripped |
| `browser_handle_dialog` | `accept`, `promptText?` | action + snapshot | snapshot inlined |
| `browser_close` | — | `No open tabs. Navigate to a URL to create one.` | — |

## Gated (hidden unless `PLAYWRIGHT_ALLOW_UNSAFE=true`)

| Tool | Key arguments | Why gated |
|---|---|---|
| `browser_evaluate` | `function`, `target?` | arbitrary JavaScript in the page |
| `browser_run_code_unsafe` | `code` | arbitrary JavaScript in the Playwright process (RCE-equivalent) |
| `browser_file_upload` | `paths[]` | reads paths on the developer's machine |
| `browser_drop` | `target`, `paths?`, `data?` | reads paths on the developer's machine |

Calling one while hidden returns `isError` with
`<tool> is disabled: set PLAYWRIGHT_ALLOW_UNSAFE=true in the workload config to expose it …`.

## Limits

| What | Value | Where |
|---|---|---|
| Upstream tool cache | `PLAYWRIGHT_TOOLS_TTL_SECS` (300 s, max 86400) per warm instance | refreshed on a cache miss for an unknown name, cleared by `playwright_reset_session` |
| Outbound deadline | `MCP_OUTBOUND_TIMEOUT_MS` (manifest: 90000; template default 30000) | one upstream exchange |
| Outbound reply size | `MCP_OUTBOUND_MAX_BYTES` (4 MiB default) | screenshots are inline base64 |
| Text block | 1 000 000 bytes, cut on a char boundary with `…[truncated N bytes …]` | every upstream text content item |
| Content blocks | 64 per result | — |
| `browser_wait_for.time` | 0..60 s | clamp |
| `browser_snapshot.depth` | 1..64 | clamp |
| `browser_resize.width/height` | 1..8192 | clamp |
| Session re-initialize | once per call on `404 Session not found` / `400 Server not initialized` | then the error surfaces |
