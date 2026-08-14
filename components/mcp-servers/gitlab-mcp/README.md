# gitlab-mcp

An MCP server that **searches and reads GitLab** for an agent, built as a
WebAssembly component for Cosmonic Desktop. Every tool call is an **outbound
HTTPS request** to the GitLab REST API v4, and the workload's `allowedHosts`
policy grants exactly **one** host — `gitlab.com`. The tool can reach nothing
else; that allowlist is the egress boundary.

Authentication is **optional**. With no token the server works against public
projects at GitLab's unauthenticated rate limit. Provide a token (see
[below](#optional-authenticate-with-a-token)) to raise the limit and reach
private projects.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `search_projects` | Search projects by query; up to 10 results, ordered by stars. |
| `get_project` | Key metadata for one project. |
| `list_issues` | A project's issues; up to 20. |
| `get_file_contents` | Read a file (base64 decoded). |

Parameters:

| Tool | Param | Type | Purpose |
|---|---|---|---|
| `search_projects` | `query` | string | Search query, matched against project name and path. |
| | `per_page` | number (optional) | Max results, 1–10 (default 10). |
| `get_project` | `id` | string | Project ID — numeric (e.g. `278964`) or `namespace/project` path (e.g. `gitlab-org/gitlab`). |
| `list_issues` | `id` | string | Project ID (numeric or `namespace/project`). |
| | `state` | string (optional) | `"opened"` (default), `"closed"`, or `"all"` (`"open"` accepted as an alias for `"opened"`). |
| `get_file_contents` | `id` | string | Project ID (numeric or `namespace/project`). |
| | `path` | string | File path within the repository (e.g. `README.md`, `src/main.rs`). |
| | `ref` | string (optional) | Branch, tag, or commit SHA (default: the project's default branch). |

A project is addressed by either its **numeric ID** or its full
**`namespace/project` path**; when a path is given, this server URL-encodes it
(so `gitlab-org/gitlab-foss` becomes `gitlab-org%2Fgitlab-foss`) before calling
the API. `get_file_contents` returns decoded file text capped at **~100 KB** (a
`truncated` flag marks when it was cut). GitLab's files API **requires a `ref`**;
when you omit it, the server first reads the project's `default_branch` and uses
that. Every tool returns structured JSON, for example `get_project`:

```json
{
  "name": "GitLab FOSS",
  "path_with_namespace": "gitlab-org/gitlab-foss",
  "description": "GitLab FOSS is a read-only mirror of GitLab, ...",
  "star_count": 2312,
  "forks_count": 4421,
  "default_branch": "master",
  "visibility": "public",
  "web_url": "https://gitlab.com/gitlab-org/gitlab-foss"
}
```

## `allowedHosts` — the egress boundary

The tools reach a host only if it is in the workload's outbound `allowedHosts`
list. GitLab's entire REST API lives under a single host, so the list is exactly
one entry:

```yaml
allowedHosts:
  - gitlab.com
```

Empty is deny-all. Because there is only ever one upstream, you should not need
to widen it.

## Optional: authenticate with a token

Unauthenticated is the default and needs no setup. To raise the rate limit or
read private projects, give the component a GitLab
[personal access token](https://gitlab.com/-/user_settings/personal_access_tokens)
through the `GITLAB_TOKEN` environment variable. Never inline the token in a
manifest — register it as a **Cosmonic secret** and flatten it into that env
var.

1. Register the token as a secret with the `cosmonic_set_secret` MCP tool (the
   value goes into your OS keychain, never into a manifest):

   | Field | Value |
   |---|---|
   | `name` | `gitlab-token` — matches `secretFrom` below |
   | `uri` | `keychain://cosmonic/gitlab-token` |
   | `env` | `GITLAB_TOKEN` — the variable injected into the component |
   | `value` | `<your personal access token>` |

2. Reference the secret from the workload so it is flattened into the
   component's environment as `GITLAB_TOKEN`. In
   [`deploy/workload.yaml`](deploy/workload.yaml), under
   `components[].localResources.environment`, uncomment:

   ```yaml
   secretFrom:
     - name: gitlab-token
   ```

The component reads `std::env::var("GITLAB_TOKEN")` on each request and, when
present, sends it in the `PRIVATE-TOKEN` header (GitLab's authentication
scheme). The `env: GITLAB_TOKEN` field on the secret registration is what makes
the value land in that variable.

## Build

```console
$ wash build
```

Tested with `wash` 2.5 and the Cosmonic Desktop daemon 0.5.x. The build emits a
WASI p3 component at `target/wasm32-wasip2/release/gitlab_mcp.wasm`.

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop): apply
[`deploy/workload.yaml`](deploy/workload.yaml) for the published image, or
promote a local build and apply the project's [`.wash/config.yaml`](.wash/config.yaml)
workload settings. Then call it through the ingress. In the stateless 2026-07-28
transport, each request stands alone, so `tools/list` and `tools/call` carry the
`Mcp-Method`/`Mcp-Name` headers and a `_meta` block:

```console
$ curl -X POST http://127.0.0.1:8200/ \
    -H 'Host: gitlab-mcp.localhost.cosmonic.sh' \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: search_projects' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_projects","arguments":{"query":"gitlab"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

See [`scripts/smoke.sh`](scripts/smoke.sh) for a full `tools/list` + all-four-tools
walk-through.

## Configuration

| Env var | Purpose |
|---|---|
| `RUST_LOG` | Log level (default `info`). |
| `GITLAB_TOKEN` | Optional GitLab token; when set, sent in the `PRIVATE-TOKEN` header to raise the rate limit and reach private projects. Inject it from a Cosmonic secret (above), not inline. |
| `MCP_ALLOWED_HOSTS` | DNS-rebinding guard for the ingress Host header; must list the workload's `host` (`gitlab-mcp.localhost`). |
| `MCP_OUTBOUND_MAX_BYTES` | Upper bound on the buffered outbound response body (default 4 MiB). |
| `MCP_OUTBOUND_TIMEOUT_MS` | Per-request outbound deadline (default 30000). |
