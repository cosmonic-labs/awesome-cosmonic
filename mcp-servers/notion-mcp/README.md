# notion-mcp

A Notion MCP server as a sandboxed WebAssembly component for
[Cosmonic Desktop](https://cosmonic.com/docs/desktop). It talks to the Notion
REST API (`https://api.notion.com/v1`, `Notion-Version: 2026-03-11`) with an
**internal integration token** and exposes 17 curated tools: an auth check,
search, page metadata and body (as Notion-flavored Markdown), page creation
and updates, Markdown find-and-replace, block appends from a small Markdown
subset, database / data source schema and queries, comments, and users.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://notion-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://notion-mcp.localhost:8200/>.

## Tools

Every id argument accepts a 32-hex id, a hyphenated UUID, or a `notion.so`
URL. Page sizes clamp to 1..100. Gated tools refuse (without calling Notion)
when `NOTION_READ_ONLY=true`.

| Tool | Parameters | Output | Gated |
|---|---|---|---|
| `check_auth` | — | `status` ok/missing/invalid/insufficient, bot identity + workspace, `remediation` | no |
| `get_self` | — | bot user `{id, name, workspace_name, workspace_id, owner_type}` | no |
| `search` | `query?`, `object?` (page/data_source), `sort_direction?`, `page_size?`, `start_cursor?` | rows `{object,id,title,url,parent,last_edited_time,database_id?}`, cursor, `request_status` | no |
| `get_page` | `page_id`, `filter_properties?`, `raw?` | metadata + flattened property values | no |
| `get_page_content` | `page_id`, `include_transcript?`, `max_chars?` | Notion-flavored Markdown + `truncated`, `unknown_block_ids` | no |
| `get_block_children` | `block_id`, `page_size?`, `start_cursor?`, `raw?` | block summaries `{id,type,has_children,text,…}` | no |
| `create_page` | `parent_page_id` xor `parent_data_source_id`, `title?`, `properties?`, `body_markdown?`, `icon_emoji?` | `{id, url, title, parent}` | yes |
| `create_data_source_item` | `data_source_id`, `title?`, `properties?` (plain values), `body_markdown?` | `{id, url, title, properties}` | yes |
| `update_page` | `page_id`, `properties?`, `in_trash?`, `icon_emoji?` | `{id, url, in_trash, last_edited_time, properties}` | yes |
| `update_page_markdown` | `page_id`, `mode` update/replace, `updates?`, `new_markdown?`, `allow_deleting_content?`, `max_chars?` | resulting Markdown | yes |
| `append_blocks` | `block_id`, `markdown`, `position?` end/start, `after_block_id?` | `{sent, appended, blocks[{id,type}]}` | yes |
| `get_database` | `database_id` | `{id, title, data_sources[{id,name}], …}` | no |
| `get_data_source` | `data_source_id`, `raw?` | schema: properties with types/options/relations/formulas | no |
| `query_data_source` | `data_source_id`, `filter?`, `sorts?`, `page_size?`, `start_cursor?`, `include_trashed?`, `filter_properties?`, `raw?` | rows with flattened properties, cursor, `request_status` | no |
| `list_comments` | `block_id`, `page_size?`, `start_cursor?` | comment summaries | no |
| `create_comment` | `text`, `page_id` xor `block_id` xor `discussion_id` | `{id, discussion_id, created_time}` | yes |
| `list_users` | `page_size?`, `start_cursor?` | `{id, type, name, email?, workspace_name?}` | no |

Full argument tables, limits and the error catalogue are in the served skill:
`skill://notion-mcp/SKILL.md` with `references/TOOLS.md`, `ENDPOINTS.md`,
`MARKDOWN.md`, `PROPERTIES.md` (also readable under
[skills/server](skills/server)).

## Configuration

| Env var | Kind | Default | Required |
|---|---|---|---|
| `NOTION_TOKEN` | **secret** (ref `notion-mcp-token`) | — | yes |
| `NOTION_VERSION` | named config | `2026-03-11` | no (`2025-09-03` also accepted) |
| `NOTION_READ_ONLY` | named config | `false` | no |
| `NOTION_BASE_URL` | named config (test override) | `https://api.notion.com` | no |
| `MCP_ALLOWED_HOSTS` | named config | `notion-mcp.localhost` | yes (ingress host guard) |
| `RUST_LOG` | named config | `info` | no |
| `MCP_OUTBOUND_TIMEOUT_MS` | named config | `30000` | no |

### The secret

1. As a workspace owner open <https://www.notion.so/profile/integrations> →
   **New integration** → Internal → pick the workspace → Capabilities: Read,
   Update and Insert content, Read and Insert comments, User information →
   Save → Configuration tab → copy the **Internal Integration Secret**
   (`ntn_…`).
2. Share content with it: open each top-level page/database in Notion → `…`
   menu → **Connections** → Connect to *your integration* (children inherit).
3. Register the reference (the value never needs to transit an agent — paste
   it in Cosmonic Desktop → Secrets, or use the API):

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock      # Linux
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"notion-mcp-token","uri":"keychain://cosmonic/notion-mcp-token","env":"NOTION_TOKEN","value":"ntn_..."}'
```

or `cosmonic_set_secret name=notion-mcp-token uri=keychain://cosmonic/notion-mcp-token env=NOTION_TOKEN value=<token>`.
Then call `check_auth`. A missing token makes every tool return
"`NOTION_TOKEN` is not set …" with these steps; an invalid one surfaces
Notion's `401 unauthorized` message plus the re-register hint. `GET /` shows
whether the secret is configured (presence only).

### Outbound policy

`allowedHosts: ["api.notion.com"]` in `deploy/workload.yaml` and
`.wash/config.yaml` — the only host the server dials. No loopback ports,
volumes, or host capabilities beyond the `wasi:http` ingress are needed.

## Build and test

```console
$ cargo build --release                       # target/wasm32-wasip2/release/notion_mcp.wasm
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ wasm-tools component wit target/wasm32-wasip2/release/notion_mcp.wasm | grep 'export wasi:http/handler@0.3.0'
$ scripts/e2e.sh [--no-build]                 # 196 hermetic cases
```

The e2e suite (sourcing `../../scripts/mcp_e2e_lib.sh`) starts a threaded
Python fixture (`scripts/fixture.py`) that impersonates `api.notion.com` and
four wasmtime instances: with the token, without it (missing-secret path),
`NOTION_READ_ONLY=true`, and `NOTION_VERSION=2025-09-03`. Set `E2E_LIVE=1`
with `NOTION_TOKEN` in the environment to add two read-only calls against the
real API. `cargo test` is not used (wasm target).

## Deploy on Cosmonic Desktop

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/notion-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/notion-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"notion-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/notion-mcp:0.1.0@sha256:…", …}
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads -H 'Content-Type: application/json' --data-binary @-
```

Register the `notion-mcp-token` secret first (above). `deploy/workload.yaml`
is the manifest of record: labels, the `wasi:http` handler on host
`notion-mcp.localhost`, `secretFrom: [{name: notion-mcp-token}]`,
`allowedHosts: [api.notion.com]`, and every named config with a comment.
Applying a name that already exists replaces it (`GET /v1/workloads` first).

### Talk to it

```console
$ curl -s http://notion-mcp.localhost:8200/ | jq '.status, .capabilities.tools, .credentials[0].status'
"ok"
["append_blocks","check_auth","create_comment", …]
"configured"

$ curl -s -X POST http://notion-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://notion-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: search' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"query":"Roadmap","object":"page","page_size":5},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"{…}"}],"structuredContent":{"count":1,"page_size":5,"results":[{"object":"page","id":"…","title":"Roadmap","url":"https://www.notion.so/…"}],"has_more":false,"next_cursor":null},"isError":false}}
```

Connect a client (the server is stateless, so any streamable-HTTP client works):

```console
$ claude mcp add --transport http notion-mcp http://notion-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"notion-mcp":{"type":"http","url":"http://notion-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [makenotion/notion-mcp-server](https://github.com/makenotion/notion-mcp-server)
  (MIT) — the endpoint list from its OpenAPI description, the per-operation
  `Notion-Version` idea (a newer version forced for the Markdown endpoints),
  the `NOTION_TOKEN` convention, and the `update_content` vs `replace_content`
  guidance. No code was copied (it is a TypeScript OpenAPI shim).
- [suekou/mcp-notion-server](https://github.com/suekou/mcp-notion-server)
  (MIT) — tool-shape ideas: simple Markdown-ish block appends and the
  raw-vs-flattened result split. No code.
- Notion API reference (developers.notion.com; facts only): endpoints,
  versioning and upgrade guides (2025-09-03, 2026-03-11), request limits,
  status codes, the Markdown dialect.

This port is Apache-2.0 (see [LICENSE](LICENSE)).

## Known limitations

- Internal integration tokens only; Notion's hosted MCP OAuth flow is out of
  scope (a callback server cannot run in the sandbox). Personal access tokens
  cannot list users.
- Notion search matches titles only and is eventually consistent; content
  created seconds ago may not appear.
- `append_blocks` converts a flat subset (no nesting, inline formatting kept
  as text); use `update_page_markdown` for callouts, toggles, tables, columns.
- `create_data_source_item` coerces the common property types; `files`,
  `rollup`, `formula`, `verification`, `button`, `place` are rejected by name
  (use `create_page` with raw `properties` for files).
- No in-component retry or sleep: 429 responses report `retry_after_seconds`
  and the caller waits. The schema cache is per warm instance (60 s).
- Large pages: `get_page_content` is bounded by `max_chars` (≤ 500,000) and the
  4 MiB outbound body cap; Notion itself truncates at ~20k blocks
  (`truncated: true`). Pre-signed file URLs in Markdown expire within about an
  hour.
- `NOTION_TIMEOUT_MS` from the design spec is not a separate knob: the
  template's `MCP_OUTBOUND_TIMEOUT_MS` is the per-call deadline.
