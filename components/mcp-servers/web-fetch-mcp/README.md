# web-fetch-mcp

An MCP server that **fetches the contents of a URL** for an agent, built as a
WebAssembly component for Cosmonic Desktop. Every fetch is an **outbound HTTP
call** governed by the workload's `allowedHosts` policy. The tool can reach
**only** the hosts that allowlist grants, and nothing else. That allowlist is
the egress boundary.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `fetch_url` | GET a URL over HTTP or HTTPS and return its contents. |

Parameters:

| Param | Type | Purpose |
|---|---|---|
| `url` | string | The URL to fetch. Must be `http://` or `https://`. |
| `format` | string (optional) | `"text"` (default) strips HTML tags and collapses whitespace to readable plain text; `"raw"` returns the body unchanged. |

The response body is capped at **~100 KB**; anything beyond that is dropped and
the result is flagged `truncated`. Binary content types (images, archives, and
so on) are summarized rather than returned as bytes.

The tool returns structured JSON:

```json
{
  "url": "https://httpbin.org/get",
  "status": 200,
  "content_type": "application/json",
  "truncated": false,
  "content": "..."
}
```

## `allowedHosts`: the egress boundary

`fetch_url` reaches a host only if that host is in the workload's outbound
`allowedHosts` list. Empty is deny-all. The starter set ships three
HTTPS-capable hosts:

```yaml
allowedHosts:
  - en.wikipedia.org
  - raw.githubusercontent.com
  - httpbin.org
```

Ask for a URL on any other host and the tool returns a friendly error rather
than data:

> Couldn't reach `example.org` — it may not be in this workload's egress
> allowlist (allowedHosts). Add it to the manifest to grant access.

Widen the list in [`workload.yaml`](workload.yaml) /
[`deploy/workload.yaml`](deploy/workload.yaml) to grant more hosts.

## Build

```console
$ wash build
```

Tested with `wash` 2.5 and the Cosmonic Desktop daemon 0.5.x. The build emits a
WASI p3 component at `target/wasm32-wasip2/release/web_fetch_mcp.wasm`.

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop): apply
[`deploy/workload.yaml`](deploy/workload.yaml) for the published image, or
promote a local build and apply [`workload.yaml`](workload.yaml). Then call it
through the ingress. In the stateless 2026-07-28 transport, each request stands
alone, so `tools/list` and `tools/call` carry the `Mcp-Method`/`Mcp-Name`
headers and a `_meta` block:

```console
$ curl -X POST http://web-fetch-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: fetch_url' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fetch_url","arguments":{"url":"https://httpbin.org/get"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Outbound requests are governed by the workload's `allowedHosts` policy. See
[`scripts/smoke.sh`](scripts/smoke.sh) for a full `tools/list` +
allowed-vs-blocked `tools/call` walk-through.

## Configuration

| Env var | Purpose |
|---|---|
| `RUST_LOG` | Log level (default `info`). |
| `MCP_ALLOWED_HOSTS` | DNS-rebinding guard for the ingress Host header; must list the workload's `host` (`web-fetch-mcp.localhost.cosmonic.sh`, `web-fetch-mcp.localhost`). |
| `MCP_OUTBOUND_MAX_BYTES` | Upper bound on the buffered outbound response body before the ~100 KB cap is applied (default 4 MiB). |
| `MCP_OUTBOUND_TIMEOUT_MS` | Per-fetch outbound deadline (default 30000). |
