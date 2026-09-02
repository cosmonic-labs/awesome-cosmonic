# atlassian-jira-mcp

Jira Cloud MCP server: JQL search, issue create/edit/comment/transition/assign,
projects, issue types, create-screen metadata and user lookup over the Jira
REST API v3 — as a sandboxed WebAssembly component for
[Cosmonic Desktop](https://cosmonic.com/docs/desktop).

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://atlassian-jira-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://atlassian-jira-mcp.localhost:8200/>.

## What it does

The server talks **directly** to your Jira Cloud site with an Atlassian API
token (HTTP Basic auth) — no admin enablement, no OAuth, and outbound network
access limited to the site host by the workload's deny-all-by-default
`allowedHosts`. Rich text (Atlassian Document Format) is rendered to readable
text on the way out and built from plain text on the way in, so agents never
touch ADF. Users are addressed by `accountId`, searches use the current
`POST /search/jql` endpoint with cursor pagination (the legacy `/search` was
removed in 2025), and every upstream failure is mapped to an actionable tool
error (401/403/404/400/409/413/422/429 with `Retry-After`).

Atlassian's official
[Rovo remote MCP server](https://github.com/atlassian/atlassian-mcp-server)
(`mcp.atlassian.com`) is the hosted alternative; it requires OAuth 2.1 or an
admin-enabled API token and cannot run inside the sandbox. This server exists
for the case where you want a local, auditable component with a static token
and a narrow egress policy.

## Tools

| Tool | Parameters | Output | Gated |
|---|---|---|---|
| `check_auth` | — | `status` ok/missing/invalid/insufficient/error, account identity, route (site/gateway), read-only flag, projects filter, `remediation` | no |
| `get_myself` | — | accountId, displayName, emailAddress, timeZone, active, locale | no |
| `search_issues` | `jql`, `fields?`, `max_results?` (1..100, 25), `next_page_token?`, `expand?` | compact issues (ADF rendered), `next_page_token`, `is_last`, effective `jql`/`fields` | no |
| `count_issues` | `jql` | approximate `count` | no |
| `get_issue` | `issue_key`, `fields?`, `expand?`, `include_comments?` | the issue (rendered description/comments, compact users/statuses), `url` | no |
| `create_issue` | `project_key`, `issue_type`, `summary`, `description?`, `priority?`, `labels?`, `assignee_account_id?`, `parent_key?`, `extra_fields?` | `id`, `key`, `url` | `JIRA_READ_ONLY` |
| `update_issue` | `issue_key`, `summary?`, `description?`, `priority?`, `labels?` or `add_labels?`/`remove_labels?`, `assignee_account_id?`, `extra_fields?`, `notify_users?`, `return_issue?` | `ok` (+ the issue with `return_issue`) | `JIRA_READ_ONLY` |
| `add_comment` | `issue_key`, `body`, `visibility_type?`, `visibility_value?` | comment `id`, author, created, `url` | `JIRA_READ_ONLY` |
| `get_comments` | `issue_key`, `start_at?`, `max_results?` (1..100, 50), `order_by?` | rendered comments, `total`, `has_more` | no |
| `get_transitions` | `issue_key`, `include_fields?` | available transitions (id, name, target status, required screen fields) | no |
| `transition_issue` | `issue_key`, `transition` (id or name), `comment?`, `fields?` | `ok`, the transition applied | `JIRA_READ_ONLY` |
| `assign_issue` | `issue_key`, `account_id?` (omit = unassign, `-1` = default) | `ok`, result | `JIRA_READ_ONLY` |
| `list_projects` | `query?`, `start_at?`, `max_results?` (1..100, 50), `type_key?` | projects (key, name, type, style, lead), pagination | no |
| `get_project` | `project_key` | project with issue types, components, versions, lead | no |
| `list_issue_types` | `project_key`, `max_results?` (1..200, 50) | issue types (id, name, subtask, hierarchyLevel) | no |
| `get_create_fields` | `project_key`, `issue_type` (id or name) | create-screen fields: required, type, allowed values | no |
| `search_users` | `query`, `max_results?` (1..50, 20), `assignable_to_project?` or `assignable_to_issue?` | users with accountIds | no |

Full shapes, limits and clamps: `skill://atlassian-jira-mcp/references/TOOLS.md`
([skills/server/references/TOOLS.md](skills/server/references/TOOLS.md)).

## Skill

The operating manual — which tool first, sequencing, JQL rules, the ADF
story, the error catalogue, rate-limit etiquette — is served over MCP:

| URI | Content |
|---|---|
| `skill://index.json` | catalog |
| `skill://atlassian-jira-mcp/SKILL.md` | the playbook ([skills/server/SKILL.md](skills/server/SKILL.md)) |
| `skill://atlassian-jira-mcp/references/TOOLS.md` | tool reference table |

## Configuration

| Env var | Kind | Default | Required | Purpose |
|---|---|---|---|---|
| `ATLASSIAN_SITE` | named config | — | yes | Jira Cloud site host, e.g. `acme.atlassian.net` (scheme/path tolerated and stripped). Builds `https://<site>` and browse URLs. |
| `ATLASSIAN_EMAIL` | named config | — | yes | Atlassian account email owning the token — the Basic-auth username. |
| `ATLASSIAN_API_TOKEN` | **secret ref `atlassian-api-token`** | — | yes | Atlassian API token — the Basic-auth password. |
| `ATLASSIAN_CLOUD_ID` | named config | empty | no | Set only for tokens created "with scopes": routes calls through `https://api.atlassian.com/ex/jira/<cloudId>`. Read the `cloudId` at `https://<site>/_edge/tenant_info`. |
| `JIRA_READ_ONLY` | named config | `false` | no | `true` refuses `create_issue`, `update_issue`, `add_comment`, `transition_issue`, `assign_issue` before dialing. |
| `JIRA_PROJECTS_FILTER` | named config | empty | no | Comma-separated project keys; searches become `(<jql>) AND project in (KEYS)` and `list_projects` is filtered. |
| `ATLASSIAN_BASE_URL` | test override | `https://<ATLASSIAN_SITE>` (or `https://api.atlassian.com`) | no | Replaces the upstream **origin**; the `/ex/jira/<cloudId>` prefix still applies when a cloud id is set. The e2e points it at a local fixture. |
| `MCP_ALLOWED_HOSTS` | named config | `atlassian-jira-mcp.localhost` | yes | DNS-rebinding guard; must list the ingress host. |
| `RUST_LOG` | named config | `info` | no | Log filter. |
| `MCP_OUTBOUND_TIMEOUT_MS` / `MCP_OUTBOUND_MAX_BYTES` | named config | `30000` / 4 MiB | no | Outbound deadline and body cap (template defaults). |

`GET /` carries a `credentials` block (configured/missing, never values) and
`check_auth` validates the token with one call — see
[Credentials](#credentials).

### Credentials

1. Sign in at <https://id.atlassian.com/manage-profile/security/api-tokens>
   and **Create API token** (classic — works against
   `https://<site>.atlassian.net`) *or* **Create API token with scopes**
   (choose Jira scopes `read:jira-work`, `write:jira-work`, `read:jira-user`;
   these tokens only work through `api.atlassian.com`, so also set
   `ATLASSIAN_CLOUD_ID`). Every token created since Dec 2024 expires (max one
   year) — set a reminder to rotate it.
2. Register it as the `atlassian-api-token` secret reference. Prefer pasting
   the value in Cosmonic Desktop → Secrets; the equivalent daemon call is:

   ```console
   $ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock      # Linux
   $ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
       -H 'Content-Type: application/json' \
       -d '{"name":"atlassian-api-token","uri":"keychain://cosmonic/atlassian-api-token","env":"ATLASSIAN_API_TOKEN","value":"<token>"}'
   ```

   or, from an agent, `cosmonic_set_secret name=atlassian-api-token
   uri=keychain://cosmonic/atlassian-api-token env=ATLASSIAN_API_TOKEN
   value=<token>`. To rotate, register the ref again with the new value.

   The ref name deliberately departs from the repo's `<name>-<purpose>`
   pattern: one Atlassian account → one ref, shared by this server and
   `atlassian-confluence-mcp`.
3. Set `ATLASSIAN_SITE` and `ATLASSIAN_EMAIL` in
   [`deploy/workload.yaml`](deploy/workload.yaml) (`localResources.environment.config`).
4. Deploy, then call `check_auth`. `status: missing` means the ref or named
   config is absent; `invalid` means Jira rejected the pair (wrong email,
   expired/revoked token, or a scoped token used without `ATLASSIAN_CLOUD_ID`).

## Outbound policy (`allowedHosts`)

The manifest ships:

```yaml
allowedHosts:
  - "https://*.atlassian.net"     # narrow to your site, e.g. https://acme.atlassian.net
  - "https://api.atlassian.com"   # only needed with ATLASSIAN_CLOUD_ID (scoped tokens)
```

Narrow the first entry to your own site and drop the second unless you use a
scoped token. The same list lives in `.wash/config.yaml`. No loopback ports,
volumes or extra host interfaces are needed.

## Build and test

```console
$ cargo build --release                 # target/wasm32-wasip2/release/atlassian_jira_mcp.wasm
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ scripts/e2e.sh                        # hermetic: fixture + 3 wasmtime instances, 330 cases
```

The e2e (`scripts/e2e.sh`, sourcing `../../scripts/mcp_e2e_lib.sh`) runs a
Python `ThreadingHTTPServer` that impersonates Jira Cloud on both route
prefixes (`scripts/jira_fixture.py`), then exercises every tool: happy paths,
malformed and adversarial input (traversal-shaped keys, 10 kB unicode, JQL with
quotes/newlines, 300-deep JSON), clamping, ADF rendering both ways, the
projects filter, the scoped-token route, upstream error mapping
(400/401/403/404/409/410/429/500/502/HTML), the read-only gate and the
missing-secret path. `E2E_LIVE=1` with `ATLASSIAN_SITE`, `ATLASSIAN_EMAIL`
and `ATLASSIAN_API_TOKEN` exported adds a read-only smoke against a real site.
`cargo test` does not apply (wasm target).

## Deploy on Cosmonic Desktop

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/atlassian-jira-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/atlassian-jira-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"atlassian-jira-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/atlassian-jira-mcp:0.1.0@sha256:…", …}
```

Register the secret (above), fill in the named config, then apply
[`deploy/workload.yaml`](deploy/workload.yaml) with `image` replaced by the
digest-pinned reference (`cosmonic_apply_workload`, the Desktop UI, or
`POST /v1/workloads`):

```console
$ IMAGE='oci.localhost:8200/apps/atlassian-jira-mcp:0.1.0@sha256:…'
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads \
        -H 'Content-Type: application/json' --data-binary @-
$ curl -s http://atlassian-jira-mcp.localhost:8200/ | jq '.status, .credentials'
```

### Talk to it

```console
$ curl -s http://atlassian-jira-mcp.localhost:8200/ | jq '.capabilities.tools'

$ curl -s -X POST http://atlassian-jira-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://atlassian-jira-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: search_issues' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_issues","arguments":{"jql":"assignee = currentUser() ORDER BY updated DESC","max_results":5},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Connect a client (the server is stateless, so any streamable-HTTP client
works):

```console
$ claude mcp add --transport http atlassian-jira-mcp http://atlassian-jira-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"atlassian-jira-mcp":{"type":"http","url":"http://atlassian-jira-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Layout

```
├── .wash/config.yaml        # build + workload draft (env, allowedHosts)
├── deploy/workload.yaml     # THE manifest
├── scripts/e2e.sh           # test entry point (sources ../../scripts/mcp_e2e_lib.sh)
├── scripts/jira_fixture.py  # hermetic Jira Cloud impersonator
├── skills/server/           # SKILL.md + references/TOOLS.md, served over MCP
└── src/
    ├── jira.rs              # config, Basic auth, client, error mapping, ADF, validators
    ├── server.rs            # the 17 tools
    └── lib.rs bridge.rs discovery.rs skills.rs telemetry.rs   # template (discovery adds `credentials`)
```

## Borrowed from

- [sooperset/mcp-atlassian](https://github.com/sooperset/mcp-atlassian) (MIT)
  — tool semantics and parameter shapes for search/get/create/update/
  transition/comment/assign, the read-only write gate and the projects-filter
  idea, and the ADF↔text conversion approach. Reimplemented in Rust; no code
  copied.
- [atlassian/atlassian-mcp-server](https://github.com/atlassian/atlassian-mcp-server)
  (Apache-2.0) — tool grouping precedent; cited as the official hosted
  alternative.
- Atlassian's Jira Cloud platform REST API v3 OpenAPI description — endpoint
  list, parameter maxima, deprecation flags (`/search` and the global
  `/issue/createmeta` are deprecated/removed) and the `ErrorCollection` shape
  (reference only).
- [pycontribs/jira](https://github.com/pycontribs/jira) (BSD-2-Clause) —
  migration notes for `/search/jql` (`nextPageToken`, explicit fields, no total).

This port is licensed Apache-2.0 (see [LICENSE](LICENSE)).

## Known limitations

- Jira Cloud only. Jira Data Center / Server (Bearer PATs, API v2, wiki
  markup bodies, the old `/search`) is not supported.
- Basic auth with an API token only. Atlassian OAuth 2.0 (3LO), service-account
  Bearer keys and the Rovo MCP's OAuth 2.1 flow are out of scope (the sandbox
  has no callback server).
- No attachments, worklogs, sprints/boards (Agile API), issue links, watchers
  or bulk operations in this version.
- No automatic retry on 429; the tool error carries `retry_after_seconds` and
  the skill teaches backoff.
- Rendered ADF is capped at 20 000 characters per field; documents nested
  deeper than 32 levels are cut, and JSON bodies nested past 128 levels are
  refused as unreadable rather than parsed.
- Wildcard `allowedHosts` (`https://*.atlassian.net`) is what the manifest
  ships; narrow it to your site for a tighter egress policy.
