# github-mcp

An MCP server that **searches and reads GitHub** for an agent, built as a
WebAssembly component for Cosmonic Desktop. Every tool call is an **outbound
HTTPS request** to the GitHub REST API, and the workload's `allowedHosts` policy
grants exactly **one** host — `api.github.com`. The tool can reach nothing else;
that allowlist is the egress boundary.

Authentication is **optional**. With no token the server works against public
data at GitHub's unauthenticated rate limit (60 requests/hour). Provide a token
(see [below](#optional-authenticate-with-a-token)) to raise the limit and reach
private repositories.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `search_repositories` | Search repositories by query; up to 10 results. |
| `get_repository` | Key metadata for one repository. |
| `list_issues` | A repository's issues (pull requests excluded); up to 20. |
| `get_file_contents` | Read a file (base64 decoded) or list a directory. |

Parameters:

| Tool | Param | Type | Purpose |
|---|---|---|---|
| `search_repositories` | `query` | string | Search query (supports GitHub qualifiers, e.g. `language:rust stars:>100`). |
| | `per_page` | number (optional) | Max results, 1–10 (default 10). |
| `get_repository` | `owner` | string | Repository owner (user or org). |
| | `repo` | string | Repository name. |
| `list_issues` | `owner`, `repo` | string | Repository coordinates. |
| | `state` | string (optional) | `"open"` (default), `"closed"`, or `"all"`. |
| `get_file_contents` | `owner`, `repo` | string | Repository coordinates. |
| | `path` | string | File or directory path (e.g. `README.md`, `src`). |
| | `ref` | string (optional) | Branch, tag, or commit SHA (default: the default branch). |

`get_file_contents` returns decoded file text capped at **~100 KB** (a
`truncated` flag marks when it was cut); when `path` is a directory it returns
the list of entries instead. Every tool returns structured JSON, for example
`get_repository`:

```json
{
  "full_name": "cosmonic-labs/awesome-cosmonic",
  "description": "Community-maintained components, host plugins, ...",
  "stars": 12,
  "forks": 3,
  "language": "Rust",
  "open_issues": 4,
  "license": "Apache-2.0",
  "default_branch": "main",
  "html_url": "https://github.com/cosmonic-labs/awesome-cosmonic"
}
```

## `allowedHosts` — the egress boundary

The tools reach a host only if it is in the workload's outbound `allowedHosts`
list. GitHub's entire REST API lives under a single host, so the list is exactly
one entry:

```yaml
allowedHosts:
  - api.github.com
```

Empty is deny-all. Because there is only ever one upstream, you should not need
to widen it.

## Optional: authenticate with a token

Unauthenticated is the default and needs no setup. To raise the rate limit
(5,000 requests/hour) or read private repositories, give the component a GitHub
[personal access token](https://github.com/settings/tokens) through the
`GITHUB_TOKEN` environment variable. Never inline the token in a manifest —
register it as a **Cosmonic secret** and flatten it into that env var, the same
mechanism fred-mcp uses for its API key.

1. Register the token as a secret with the `cosmonic_set_secret` MCP tool (the
   value goes into your OS keychain, never into a manifest):

   | Field | Value |
   |---|---|
   | `name` | `github-token` — matches `secretFrom` below |
   | `uri` | `keychain://cosmonic/github-token` |
   | `env` | `GITHUB_TOKEN` — the variable injected into the component |
   | `value` | `<your personal access token>` |

2. Reference the secret from the workload so it is flattened into the
   component's environment as `GITHUB_TOKEN`. In
   [`deploy/workload.yaml`](deploy/workload.yaml), under
   `components[].localResources.environment`, uncomment:

   ```yaml
   secretFrom:
     - name: github-token
   ```

The component reads `std::env::var("GITHUB_TOKEN")` on each request and, when
present, sends it as `Authorization: Bearer <token>`. The `env: GITHUB_TOKEN`
field on the secret registration is what makes the value land in that variable.

## Build

```console
$ wash build
```

Tested with `wash` 2.5 and the Cosmonic Desktop daemon 0.5.x. The build emits a
WASI p3 component at `target/wasm32-wasip2/release/github_mcp.wasm`.

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop): apply
[`deploy/workload.yaml`](deploy/workload.yaml) for the published image, or
promote a local build and apply the project's [`.wash/config.yaml`](.wash/config.yaml)
workload settings. Then call it through the ingress. In the stateless 2026-07-28
transport, each request stands alone, so `tools/list` and `tools/call` carry the
`Mcp-Method`/`Mcp-Name` headers and a `_meta` block:

```console
$ curl -X POST http://127.0.0.1:8200/ \
    -H 'Host: github-mcp.localhost.cosmonic.sh' \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: search_repositories' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_repositories","arguments":{"query":"cosmonic"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

See [`scripts/smoke.sh`](scripts/smoke.sh) for a full `tools/list` + all-four-tools
walk-through.

## Configuration

| Env var | Purpose |
|---|---|
| `RUST_LOG` | Log level (default `info`). |
| `GITHUB_TOKEN` | Optional GitHub token; when set, sent as a bearer token to raise the rate limit and reach private repos. Inject it from a Cosmonic secret (above), not inline. |
| `MCP_ALLOWED_HOSTS` | DNS-rebinding guard for the ingress Host header; must list the workload's `host` (`github-mcp.localhost`). |
| `MCP_OUTBOUND_MAX_BYTES` | Upper bound on the buffered outbound response body (default 4 MiB). |
| `MCP_OUTBOUND_TIMEOUT_MS` | Per-request outbound deadline (default 30000). |
