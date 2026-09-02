# Upstream setup for playwright-mcp

This proxy does nothing on its own: the browser lives in the official
Playwright MCP server (`@playwright/mcp`, microsoft/playwright-mcp,
Apache-2.0) that the developer runs natively on their machine. Everything
below is what has to be true before `browser_*` tools work. Verify with
`playwright_status`; it names the failing step.

## 1. Start the upstream in streamable-HTTP mode

```console
$ npx @playwright/mcp@latest --port 8931 --host 127.0.0.1 --headless \
    --allowed-hosts host.wasmcloud.internal:8931,localhost:8931
```

| Flag | Why it is needed |
|---|---|
| `--port 8931` | Streamable-HTTP mode at `http://127.0.0.1:8931/mcp` (without `--port` the server speaks stdio only). `PLAYWRIGHT_BASE_URL` defaults to `http://host.wasmcloud.internal:8931/mcp`. |
| `--host 127.0.0.1` | The default bind is `localhost`, which on many machines is `::1` only; Cosmonic Desktop resolves `host.wasmcloud.internal` to IPv4 loopback, so an IPv6-only listener answers "connection refused". |
| `--allowed-hosts host.wasmcloud.internal:8931,localhost:8931` | The upstream validates the `Host` header (DNS-rebinding guard) and defaults to the bound host only. The sandbox dials `host.wasmcloud.internal:8931` and cannot rewrite `Host`, so without this every call is `403 Access is only allowed at localhost:8931`. The list is **comma-separated**; a space-separated list keeps only the last value. `'*'` disables the check. |
| `--headless` | No window pops up. Drop it to watch the browser. |

Useful extras:

- `--isolated` — in-memory profile (no cookies/logins carried over; discarded on exit).
- `--shared-browser-context` — one browser context for every connected client, so tabs and cookies survive a proxy re-initialize (instance recycling, `playwright_reset_session`).
- `--caps pdf,vision,devtools` — adds `browser_pdf_save`, `browser_mouse_*_xy`, tracing tools. They appear in `tools/list` after the cache TTL (`PLAYWRIGHT_TOOLS_TTL_SECS`, 300 s) or a `playwright_reset_session`.
- `--output-dir <dir>` / `--output-max-size <bytes>` — where the upstream writes snapshot/console/screenshot files on every action (the proxy never reads them; the `filename` parameter is stripped for that reason).
- `--allow-unrestricted-file-access` — permits `file:` navigation (blocked by default).
- `--timeout-navigation 60000` / `--timeout-action 5000` — the upstream's own deadlines; the proxy's outbound deadline (`MCP_OUTBOUND_TIMEOUT_MS`, 90 s in the manifest) must stay above them.

Every flag has a `PLAYWRIGHT_MCP_*` environment equivalent
(`PLAYWRIGHT_MCP_PORT`, `PLAYWRIGHT_MCP_HOST`, `PLAYWRIGHT_MCP_ALLOWED_HOSTS`,
`PLAYWRIGHT_MCP_HEADLESS`, `PLAYWRIGHT_MCP_ISOLATED`, `PLAYWRIGHT_MCP_CAPS`, …).

## 2. The three Cosmonic Desktop loopback grants

A workload reaches the developer's machine only through the sentinel name
`host.wasmcloud.internal`, and only when all three line up:

1. `deploy/workload.yaml` → `allowedHosts: ["host.wasmcloud.internal:8931"]`
2. `deploy/workload.yaml` → `allowedHostLoopbackPorts: ["8931"]` on the component
3. Desktop **Settings → Security → allow host loopback** (default **off**)

Missing any of them: `playwright_status` reports `unreachable` with a
"denied"/"policy" detail (1 or 2 missing) or a connection error (3 off, or the
upstream not running). These are permanent until a human changes the setup —
never retry them in a loop.

## 3. Same-port coupling

If you change `--port`, four things must change together: `--port`,
`--allowed-hosts`, `PLAYWRIGHT_BASE_URL`, and both manifest entries
(`allowedHosts`, `allowedHostLoopbackPorts`).

## 4. Optional: a hosted upstream with a bearer token

`PLAYWRIGHT_BASE_URL` may point at an HTTPS Playwright MCP endpoint behind an
authenticating reverse proxy (webpki roots only — self-signed certificates
fail). Register the token as the `playwright-mcp-token` secret (env
`PLAYWRIGHT_BEARER_TOKEN`) and list `secretFrom: [{name: playwright-mcp-token}]`
in the manifest; the proxy then sends `Authorization: Bearer <token>` on every
upstream request. The local `npx` server needs none. Replace `allowedHosts`
with the hosted hostname and drop the loopback grant.

## 5. What the sandbox does and does not contain

- The **browser runs natively** on the developer's machine, with their network
  and (unless `--isolated`) their persistent profile. `browser_navigate` to
  `http://localhost:3000` reaches *their* localhost.
- The proxy's only extra protection is the gate on `browser_evaluate`,
  `browser_run_code_unsafe`, `browser_file_upload`, `browser_drop`
  (`PLAYWRIGHT_ALLOW_UNSAFE`, default off) and the removal of `filename`.
- Instances are `poolSize: 1`: one upstream browser session per warm instance.
  If Desktop recycles the instance or the upstream restarts, the next call
  re-initializes transparently and page state is gone (use
  `--shared-browser-context` to keep tabs across that).
