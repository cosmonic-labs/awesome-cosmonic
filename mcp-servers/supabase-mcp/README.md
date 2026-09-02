# supabase-mcp

A Rust/WebAssembly port of the official
[Supabase MCP server](https://github.com/supabase-community/supabase-mcp)'s
platform tools for [Cosmonic Desktop](https://cosmonic.com/docs/desktop). It
talks to the **Supabase Management API** (`https://api.supabase.com/v1`) with
a Personal Access Token and exposes project discovery, schema introspection,
SQL execution (read-only by default), migrations, per-service logs,
security/performance advisors, Edge Function source, the project URL,
publishable keys and TypeScript type generation.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://supabase-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://supabase-mcp.localhost:8200/>.

## Tools

Every project-scoped tool takes `project_id` — the 20-letter lowercase ref
that `list_projects` returns as `id`. It is ignored when the server is pinned
with `SUPABASE_PROJECT_REF`.

| Tool | Parameters | Output | Gated? |
|---|---|---|---|
| `check_auth` | — | `status` ok/missing/invalid/insufficient, identity, mode, remediation | — |
| `list_organizations` | — | organizations {id, slug, name} | hidden + refused when pinned |
| `list_projects` | — | projects {id, name, organization_id/slug, region, status, created_at, database_version} | hidden + refused when pinned |
| `get_project` | `project_id` | project details, `healthy`, hint when paused/transitioning | — |
| `list_tables` | `project_id`, `schemas?` (≤ 20, default `["public"]`, `[]` = all), `verbose?`, `max_rows?` | tables with RLS flag, row estimate, size, primary keys (+ columns/FKs when verbose), `truncated` | — |
| `list_extensions` | `project_id` | extensions {name, schema, default_version, installed_version, comment} | — |
| `list_migrations` | `project_id` | migrations {version, name} | — |
| `execute_sql` | `project_id`, `query` (≤ 100 k chars), `max_rows?` (≤ 1000) | rows (JSON), `row_count`, `truncated`, `read_only`; text wrapped in an untrusted-data boundary | writes fail under `SUPABASE_READ_ONLY=true` |
| `apply_migration` | `project_id`, `name` (snake_case), `query` (≤ 200 k chars) | `success` | refused under `SUPABASE_READ_ONLY=true` |
| `get_logs` | `project_id`, `service`, `iso_timestamp_start?`, `iso_timestamp_end?`, `limit?` (≤ 100) | log rows newest-first, normalised window, `window_clamped` | 30 req/min upstream |
| `get_advisors` | `project_id`, `type` security/performance, `level_min?` | lints with remediation URLs, `by_level` | — |
| `list_edge_functions` | `project_id` | functions {id, slug, name, status, version, verify_jwt, paths} | — |
| `get_edge_function` | `project_id`, `function_slug` | metadata + `files[]` {name, content, truncated} | 512 KiB/file, 2 MiB total |
| `get_project_url` | `project_id` | `url` (`https://<ref>.supabase.co`, computed) | — |
| `get_publishable_keys` | `project_id` | publishable + legacy anon keys with `disabled`; secret keys never returned | — |
| `generate_typescript_types` | `project_id`, `included_schemas?` (≤ 20) | `types` (≤ 1 MiB), `truncated` | — |

Skill: `skill://supabase-mcp/SKILL.md` (catalog at `skill://index.json`,
tool reference at `skill://supabase-mcp/references/TOOLS.md`). The skill
carries the sequencing (`check_auth` → `list_projects` → `get_project` →
schema tools), the read-only/pinned semantics, the logs quirks, the limits and
the full error catalogue.

## Configuration

| Env | Kind | Default | Required |
|---|---|---|---|
| `SUPABASE_ACCESS_TOKEN` | secret ref `supabase-mcp-access-token` | — | yes |
| `SUPABASE_READ_ONLY` | named config | `true` | no — `false` allows `execute_sql` writes and `apply_migration` |
| `SUPABASE_PROJECT_REF` | named config | unset | no — 20-letter ref; pins every tool to that project and hides `list_organizations`/`list_projects` |
| `SUPABASE_MAX_ROWS` | named config | `200` | no — default row cap, 1..1000 |
| `SUPABASE_BASE_URL` | test override | `https://api.supabase.com` | no — Management API origin (the server appends `/v1`); the e2e points it at a fixture. Also drives `get_project_url`'s domain (`api.supabase.com` → `supabase.co`, `.green`/`.red` staging hosts map to themselves) |
| `MCP_ALLOWED_HOSTS` | named config | `supabase-mcp.localhost` | DNS-rebinding guard; must match the ingress host |
| `RUST_LOG` | named config | `info` | log level |

A missing token makes every tool (and `check_auth`) return one actionable
error: `SUPABASE_ACCESS_TOKEN is not set. Create a Personal Access Token at
https://supabase.com/dashboard/account/tokens and register it as the
supabase-mcp-access-token secret …`. An invalid token surfaces the upstream
401 message plus the same hint. `GET /` reports the credential's presence
(never its value) in a `credentials` block.

### The secret

Sign in to Supabase → Account → **Access Tokens**
(<https://supabase.com/dashboard/account/tokens>) → *Generate new token*.
Prefer a **fine-grained**, expiring token with only: `organizations_read`,
`projects_read`, `database_read`, `analytics_logs_read`,
`edge_functions_read`, `api_gateway_keys_read` — plus `database_write` and
`database_migrations_write` only if you deploy with
`SUPABASE_READ_ONLY=false`. A classic token has the full power of your
account. Copy the `sbp_…` value once (it is not shown again) and register it:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock     # Linux; macOS: "$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"supabase-mcp-access-token","uri":"keychain://cosmonic/supabase-mcp-access-token","env":"SUPABASE_ACCESS_TOKEN","value":"sbp_..."}'
```

or paste it in Desktop → Secrets, or use the `cosmonic_set_secret` MCP tool
with the same name/uri/env. Then (re)deploy and call `check_auth`.

## Outbound policy

`allowedHosts: ["api.supabase.com"]` in both `deploy/workload.yaml` and
`.wash/config.yaml` — the only host the tools dial (HTTPS with public CA
roots). No loopback ports, volumes or extra host interfaces are needed.

## Build and test

```console
$ cargo build --release
$ wasm-tools component wit target/wasm32-wasip2/release/supabase_mcp.wasm | grep 'export wasi:http/handler@0.3.0'
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ scripts/e2e.sh            # hermetic: a Python fixture stands in for api.supabase.com
```

The e2e starts five wasmtime instances (read-only, tokenless guard,
`SUPABASE_READ_ONLY=false`, pinned, wrong token) against
`scripts/fixture.py`, which echoes what it received so the suite can assert
headers, query encoding, bound parameters, the `read_only` flag, clamping and
every error mapping (401/402/403/404/406/429/500, SQL errors, read-only
writes). Options: `E2E_PG=1` runs the borrowed pg-meta SQL against the local
`mcp-pg` Postgres container (`podman exec`); `E2E_LIVE=1` with
`SUPABASE_ACCESS_TOKEN` in the environment adds one `check_auth` against the
real API. `cargo test` is not used (wasm target).

## Deploy on Cosmonic Desktop

Register the secret (above), then:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/supabase-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/supabase-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"supabase-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/supabase-mcp:0.1.0@sha256:…", …}
$ IMAGE=oci.localhost:8200/apps/supabase-mcp:0.1.0@sha256:…
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads \
        -H 'Content-Type: application/json' --data-binary @-
```

Or apply `deploy/workload.yaml` as-is (published image) with
`cosmonic_apply_workload`. Set `SUPABASE_READ_ONLY`, `SUPABASE_PROJECT_REF`
and `SUPABASE_MAX_ROWS` in the manifest's `environment.config` before
applying. The server is then at <http://supabase-mcp.localhost:8200/>.

### Talk to it

```console
$ curl -s http://supabase-mcp.localhost:8200/ | jq '.status, .capabilities.tools, .credentials'

$ curl -s -X POST http://supabase-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://supabase-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: execute_sql' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"execute_sql","arguments":{"project_id":"abcdefghijklmnopqrst","query":"select now()"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

### Connect a client

```console
$ claude mcp add --transport http supabase-mcp http://supabase-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"supabase-mcp":{"type":"http","url":"http://supabase-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [supabase-community/supabase-mcp](https://github.com/supabase-community/supabase-mcp)
  (`packages/mcp-server-supabase`, Apache-2.0): the tool surface and
  descriptions, the Management API endpoint list, the pg-meta SQL
  (`src/sql/tables.sql`, `columns.sql`, `extensions.sql` verbatim, plus the
  `listTablesSql` composition and system-schema list), the per-service
  ClickHouse log templates, the compact table/column reshaping, the
  publishable-key filtering, the project-URL domain mapping, the 401/403
  wording, the untrusted-data boundary, and the fixture's 406-without-Accept
  behaviour (modelled on `test/mocks.ts`).
- [supabase/postgres-meta](https://github.com/supabase/postgres-meta)
  (Apache-2.0): the SQL error body shape proxied by the query endpoint
  (`message`, `code`, `formattedError`, `position`).

This port is Apache-2.0 (see `LICENSE`).

## Known limitations

- Not ported: project create/pause/restore, cost confirmation, branching,
  storage configuration, `deploy_edge_function` (multipart upload), docs
  search, and the hosted OAuth flow (a static PAT is used instead).
- `get_project_url` is computed, not fetched: custom domains / vanity
  subdomains are not reflected (same as the official server).
- `list_projects` is unpaginated upstream; accounts with hundreds of projects
  return one large body. Outbound bodies are capped at 4 MiB and one exchange
  at 30 s (`MCP_OUTBOUND_MAX_BYTES` / `MCP_OUTBOUND_TIMEOUT_MS` override
  them); an over-cap or timed-out response is a tool error that says which
  parameter to narrow, never an `allowedHosts` hint.
- The read-only guarantee for `execute_sql` relies on the Management API's
  `read_only` flag (restricted Postgres role); `apply_migration` is refused
  locally regardless.
- Function slugs are accepted case-insensitively (`^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`)
  because the platform allows mixed-case names; the research spec suggested
  lowercase only.
- The advisors endpoints and the Edge Function body format are experimental
  upstream; an unexpected advisors shape is passed through raw, a non-object
  Edge Function metadata body is an `unexpected Edge Function metadata shape`
  tool error (never a trap), and a non-multipart function body is returned as
  one `(raw body)` file.
