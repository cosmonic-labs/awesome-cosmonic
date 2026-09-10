# iss-mcp

An MCP server that answers **who is currently in space** and **where the
International Space Station is right now**, built as a WebAssembly component for
Cosmonic Desktop. It makes **outbound HTTP calls** to the
[Open Notify](http://open-notify.org) project, governed by the workload's
`allowedHosts` policy.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

Both tools take **no parameters**.

| Tool | Purpose |
|---|---|
| `who_is_in_space` | The people currently in space, with the spacecraft each is aboard, and a count. |
| `iss_position` | The ISS's current latitude and longitude (as numbers), plus the reading's Unix and ISO-8601 timestamps. |

## Endpoints & `allowedHosts`

Open Notify is **HTTP-only** — it has no HTTPS endpoint — so the outbound calls
go over plain `http://`.

- `http://api.open-notify.org/astros.json` — people currently in space
- `http://api.open-notify.org/iss-now.json` — ISS latitude/longitude

The workload's outbound allow-list must contain exactly this one host:

```yaml
allowedHosts: [api.open-notify.org]
```

Open Notify is a small community service and is occasionally briefly
unavailable; when a call cannot complete, the tool returns a friendly
try-again message rather than an error.

## Build

```console
$ wash build
```

Tested with `wash` 2.5 and the Cosmonic Desktop daemon 0.5.x. The build emits a
WASI p3 component at `target/wasm32-wasip2/release/iss_mcp.wasm`.

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop): apply
[`deploy/workload.yaml`](deploy/workload.yaml) for the published image, or
promote a local build and apply [`workload.yaml`](workload.yaml). Then call it
through the ingress. In the stateless 2026-07-28 transport, each request stands
alone, so `tools/list` and `tools/call` carry the `Mcp-Method`/`Mcp-Name`
headers and a `_meta` block:

```console
$ curl -X POST http://iss-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: who_is_in_space' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"who_is_in_space","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Outbound requests are governed by the workload's `allowedHosts` policy
(`api.open-notify.org`).

## Configuration

| Env var | Purpose |
|---|---|
| `RUST_LOG` | Log level (default `info`). |
| `MCP_ALLOWED_HOSTS` | DNS-rebinding guard for the ingress Host header; must list the workload's `host` (`iss-mcp.localhost`). |
| `OPEN_NOTIFY_BASE_URL` | Override the Open Notify base URL (testing only). |
