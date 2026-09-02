# playwright-mcp

Browser automation through the **official Playwright MCP server** running on
the developer's machine — navigate, read the accessibility snapshot, click,
type, fill forms, wait for text, screenshot, inspect console and network — as
a sandboxed WebAssembly MCP **proxy** on Cosmonic Desktop.

A browser cannot run inside a Wasm component, so this server forwards
`tools/list` and `tools/call` to [`@playwright/mcp`](https://github.com/microsoft/playwright-mcp)
(Apache-2.0) started in its streamable-HTTP mode on the same machine, and adds
what a sandboxed proxy needs on top:

- **One browser session per warm instance.** The upstream `Mcp-Session-Id` is
  cached in the component (`poolSize: 1`), so a sequence of `browser_*` calls
  drives one browser; a lost session (upstream restart) is re-initialized
  transparently once.
- **Inline snapshots.** The upstream's action results only carry a file link
  (`- [Snapshot](out/page-….yml)`) the sandbox cannot read; the proxy replaces
  it with a fresh inline accessibility snapshot (`PLAYWRIGHT_AUTO_SNAPSHOT`),
  keeping `### Page`, `### Modal state` and `### Events` intact.
- **A safety gate.** `browser_evaluate`, `browser_run_code_unsafe`,
  `browser_file_upload` and `browser_drop` (arbitrary JS / host paths) are
  hidden and refused unless `PLAYWRIGHT_ALLOW_UNSAFE=true`; the `filename`
  parameter is stripped everywhere (files would land on the developer's disk).
- **Clamps and hygiene.** `browser_wait_for.time` ≤ 60 s, `browser_snapshot.depth`
  ≤ 64, `browser_resize` ≤ 8192 px, jpeg screenshots by default, ANSI codes
  stripped from Playwright call logs, text blocks bounded at 1 MB.
- **A diagnostic tool and a skill.** `playwright_status` names the exact
  missing setup step (loopback grants, `--allowed-hosts`, `--host 127.0.0.1`,
  bearer token); `skill://playwright-mcp/SKILL.md` teaches snapshot-before-act,
  the `target` argument, the session model and the error catalogue.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`, serves a
discovery document on `GET /` and `GET /health`, and publishes its playbook
as a skill at `skill://playwright-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://playwright-mcp.localhost:8200/>.

## Tools

Two local tools plus the upstream's own list (24 core tools in `@playwright/mcp`
0.0.80; more with `--caps pdf,vision,devtools`), forwarded with the changes in
the last column. Full reference: [skills/server/references/TOOLS.md](skills/server/references/TOOLS.md).

| Tool | Params | Output | Gated? |
|---|---|---|---|
| `playwright_status` | none | `structuredContent`: `status` (`ok`/`unreachable`/`host_rejected`/`missing`/`invalid`/`error`), upstream name/version/protocol, session + tool-cache state, gates, `remediation`, `launch_command` | no (read-only) |
| `playwright_reset_session` | none | `structuredContent`: `status` (`reset`/`nothing_to_reset`), `upstream_status` — DELETEs the upstream browser session | destructive |
| `browser_navigate` | `url` | action sections + **inline snapshot** | forwarded |
| `browser_snapshot` | `target?`, `depth?` (1..64), `boxes?` | ```yaml accessibility tree with `[ref=eN]` | forwarded, `filename` stripped |
| `browser_find` | `text?` \| `regex?` | matching subtree | forwarded |
| `browser_click` / `browser_hover` / `browser_drag` / `browser_select_option` / `browser_press_key` / `browser_fill_form` / `browser_navigate_back` / `browser_handle_dialog` / `browser_tabs` / `browser_resize` | as upstream (`target` = snapshot ref or selector) | action sections + inline snapshot | forwarded, clamps on `index`/`width`/`height` |
| `browser_type` | `target`, `text`, `submit?`, `slowly?` | action section | forwarded |
| `browser_wait_for` | one of `time` (≤ 60 s), `text`, `textGone` | `Waited for …` + inline snapshot | forwarded, `time` clamped |
| `browser_take_screenshot` | `type?` (jpeg default), `target?`, `fullPage?`, `scale?` | text + `image` content block (base64) | forwarded, `filename` stripped |
| `browser_console_messages` / `browser_network_requests` / `browser_network_request` | as upstream | text lists | forwarded, `filename` stripped |
| `browser_close` | none | `No open tabs …` | forwarded |
| `browser_evaluate` / `browser_run_code_unsafe` / `browser_file_upload` / `browser_drop` | as upstream | as upstream | **hidden + refused unless `PLAYWRIGHT_ALLOW_UNSAFE=true`** |

Browser tools return the upstream's content blocks (text, images) plus a
`### Proxy notes` block whenever the proxy stripped or clamped something.
Upstream tool errors (`Ref e99 not found …`, zod validation, `net::ERR_…`)
pass through verbatim as `isError` results; an unknown tool name is JSON-RPC
`-32602`.

## Skill

`skill://playwright-mcp/SKILL.md` (catalog: `skill://index.json`), with
`references/TOOLS.md` (tool reference, limits) and `references/SETUP.md`
(upstream flags, the three loopback grants, the same-port coupling).

## Configuration

| Env var | Kind | Default | Required |
|---|---|---|---|
| `PLAYWRIGHT_BASE_URL` | named config | `http://host.wasmcloud.internal:8931/mcp` | no — full URL of the upstream streamable-HTTP endpoint (ends in `/mcp`); the e2e points it at a fixture |
| `PLAYWRIGHT_ALLOW_UNSAFE` | named config | `false` | no — `true` lists and forwards the four unsafe tools |
| `PLAYWRIGHT_AUTO_SNAPSHOT` | named config | `true` | no — `false` skips the snapshot round-trip after actions (results keep the file link) |
| `PLAYWRIGHT_TOOLS_TTL_SECS` | named config | `300` (max 86400) | no — upstream `tools/list` cache per warm instance |
| `MCP_OUTBOUND_TIMEOUT_MS` | named config (template) | `30000`; manifest sets `90000` | no — upstream navigation may take 60 s and `browser_wait_for` up to 60 s |
| `MCP_OUTBOUND_MAX_BYTES` | named config (template) | `4194304` | no — raise for fullPage PNG screenshots |
| `MCP_ALLOWED_HOSTS` | named config (template) | `playwright-mcp.localhost` (manifest) | yes — the ingress Host guard |
| `RUST_LOG` | named config | `info` | no |
| `PLAYWRIGHT_BEARER_TOKEN` | **secret ref** `playwright-mcp-token` | unset | **no** — only for a hosted upstream behind an authenticating proxy; sent as `Authorization: Bearer` |

`GET /` carries a `credentials` block (presence only) for the optional token;
`playwright_status` is its validator.

### The optional secret

The local `npx @playwright/mcp` server has **no authentication**; nothing needs
registering. Only if `PLAYWRIGHT_BASE_URL` points at a hosted endpoint behind an
authenticating reverse proxy:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock          # Linux
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"playwright-mcp-token","uri":"keychain://cosmonic/playwright-mcp-token","env":"PLAYWRIGHT_BEARER_TOKEN","value":"<token>"}'
```

(or `cosmonic_set_secret name=playwright-mcp-token uri=keychain://cosmonic/playwright-mcp-token env=PLAYWRIGHT_BEARER_TOKEN value=<token>`),
then uncomment `secretFrom: [{name: playwright-mcp-token}]` in
`deploy/workload.yaml`. A missing token against an authenticating endpoint
yields the tool error `PLAYWRIGHT_BEARER_TOKEN is not set and the Playwright
MCP endpoint at … demands authentication (HTTP 401 …)` naming the ref and the
command; a rejected one yields `… rejected the bearer token (HTTP 401: …)`.

## Upstream setup and grants

### 1. Start the upstream (on the machine running Desktop)

```console
$ npx @playwright/mcp@latest --port 8931 --host 127.0.0.1 --headless \
    --allowed-hosts host.wasmcloud.internal:8931,localhost:8931
```

- `--host 127.0.0.1`: the default `localhost` bind is `::1`-only on many
  machines, and Desktop resolves `host.wasmcloud.internal` to IPv4 loopback.
- `--allowed-hosts …` (comma-separated; a space-separated list keeps only the
  last value): the upstream validates the `Host` header and the sandbox cannot
  rewrite it — without this every call is `403 Access is only allowed at
  localhost:8931`. Env equivalent: `PLAYWRIGHT_MCP_ALLOWED_HOSTS`.
- Optional: `--isolated` (throwaway profile), `--shared-browser-context`
  (tabs survive a proxy re-initialize), `--caps pdf,vision,devtools`
  (more tools), `--output-dir` (where the upstream writes its own
  snapshot/screenshot files on every action).

### 2. `allowedHosts` and the loopback grant

`deploy/workload.yaml` lists exactly the host the code dials:

```yaml
allowedHosts:
  - "host.wasmcloud.internal:8931"
allowedHostLoopbackPorts:
  - "8931"
```

A workload reaches the developer's machine only when three things line up:

1. `allowedHosts: ["host.wasmcloud.internal:8931"]` (above),
2. `allowedHostLoopbackPorts: ["8931"]` on the component (above),
3. Desktop **Settings → Security → allow host loopback** (default off).

If you change `--port`, change `PLAYWRIGHT_BASE_URL`, both manifest entries and
`--allowed-hosts` together. No volumes, no other host interfaces.

### 3. Verify

`curl http://playwright-mcp.localhost:8200/`, then call `playwright_status`: it
answers `status: ok` with the upstream's name/version, or `unreachable` /
`host_rejected` with the exact fix.

## Build and test

```console
$ cargo build --release
$ scripts/e2e.sh            # hermetic: Python fixture impersonating @playwright/mcp 0.0.80
$ E2E_LIVE=1 scripts/e2e.sh # also drives the real `npx -y @playwright/mcp@0.0.80` (headless chromium)
```

Gates: `cargo fmt --check`, `cargo clippy --all-features -- -D warnings`,
`cargo build --release`, and
`wasm-tools component wit target/wasm32-wasip2/release/playwright_mcp.wasm | grep 'export wasi:http/handler@0.3.0'`.
`cargo test` does not apply (wasm target).

The suite runs five wasmtime instances (token, no-token guard, unsafe gate on
+ auto-snapshot off, unreachable upstream, and a sequential-only instance for
session/cache assertions — `wasmtime serve` pools instances under
concurrency) against `scripts/playwright_fixture.py`, which reproduces the
upstream's sessions, SSE framing, 400/403/404/406/415/429 behaviour and result
sections, and records every request for assertions on headers, encoding and
clamping.

## Deploy on Cosmonic Desktop

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/playwright-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/playwright-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"playwright-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/playwright-mcp:0.1.0@sha256:…", …}
```

Apply [`deploy/workload.yaml`](deploy/workload.yaml) with `image` replaced by
that pinned ref (`cosmonic_apply_workload`, or `POST /v1/workloads`):

```console
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
  | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads -H 'Content-Type: application/json' --data-binary @-
```

Then switch on Settings → Security → allow host loopback, start the upstream,
and talk to <http://playwright-mcp.localhost:8200/>. Docs:
<https://cosmonic.com/docs/desktop>.

### Talk to it

```console
$ curl -s http://playwright-mcp.localhost:8200/ | jq '.status, .capabilities.tools, .credentials[0].status'
"ok"
["playwright_status","playwright_reset_session", …]
"missing"
```

(`capabilities.tools` lists the browser tools once the instance has talked to
the upstream; before that only the two local tools appear.)

```console
$ curl -s -X POST http://playwright-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://playwright-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/call' -H 'Mcp-Name: playwright_status' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"playwright_status","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"Playwright MCP upstream OK at http://host.wasmcloud.internal:8931/mcp: Playwright 1.63.0-alpha-… (MCP 2025-11-25). Session cached: true; tools cached: 24."}],"structuredContent":{"status":"ok", …},"isError":false}}

$ curl -s -X POST http://playwright-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/call' -H 'Mcp-Name: browser_navigate' \
    -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com/"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

### Connect a client

```console
$ claude mcp add --transport http playwright-mcp http://playwright-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"playwright-mcp":{"type":"http","url":"http://playwright-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [microsoft/playwright-mcp](https://github.com/microsoft/playwright-mcp)
  (`@playwright/mcp` 0.0.80, **Apache-2.0**) — the upstream this server
  proxies: its tool surface, CLI flags, result section format and
  streamable-HTTP session semantics were studied and reproduced in the
  fixture; no code is copied, the proxy speaks its wire protocol.
- [modelcontextprotocol/typescript-sdk](https://github.com/modelcontextprotocol/typescript-sdk)
  (**MIT**) — the `StreamableHTTPServerTransport` header/status behaviour
  the upstream embeds (`mcp-session-id`, 406 Accept rule, 400 "Server not
  initialized", 404 "Session not found", DELETE), emulated by the fixture and
  handled by the client.
- [cosmonic-labs/mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs)
  (**Apache-2.0**) — `lib.rs`, `bridge.rs`, `discovery.rs`, `skills.rs`,
  `telemetry.rs` as shipped (`discovery.rs` gained the `credentials` block).
- Landscape note: [executeautomation/mcp-playwright](https://github.com/executeautomation/mcp-playwright)
  (MIT) is a stdio-only community alternative; nothing was borrowed from it.

This port is Apache-2.0 (see `LICENSE`).

## Known limitations

- **The browser is on the developer's machine, not in the sandbox.** It runs
  with their network and (without `--isolated`) their persistent profile;
  `browser_navigate` to `http://localhost:3000` reaches *their* localhost.
  The proxy's only extra protections are the unsafe-tool gate and the
  `filename` removal.
- **Session affinity.** Browser state is bound to one upstream session cached
  in one warm instance. Instance recycling, `poolSize > 1`, or an upstream
  restart silently give a fresh context (mitigated by the transparent
  re-initialize, `playwright_reset_session`, and `--shared-browser-context`).
- **Host header coupling.** `wasi:http` cannot override `Host`, so the
  upstream must be started with `--allowed-hosts host.wasmcloud.internal:8931,…`
  and `--host 127.0.0.1`; the proxy reports both with a distinct error.
- **Reply size and time.** Screenshots (especially fullPage PNG) and deep
  snapshots can exceed `MCP_OUTBOUND_MAX_BYTES`; navigations up to 60 s plus
  `browser_wait_for` need the 90 s outbound timeout, and a long wait holds the
  single instance.
- **The upstream writes files on every action** (snapshots, console logs,
  screenshots under its `--output-dir`, evicted via `--output-max-size`);
  harmless, but surprising.
- **Version drift.** `@playwright/mcp` is 0.0.x on an alpha Playwright build
  (`ref` became `target` recently). The dynamic passthrough absorbs schema
  drift; the snapshot-link detection (`- [Snapshot](`) and the skill prose may
  need updates.
- **`tools/list` when the upstream is down** lists only the two local tools
  (so a client can still connect and read `playwright_status`); reconnect or
  re-list after fixing the setup.
- No OAuth / interactive auth (out of scope); only a static bearer token.
