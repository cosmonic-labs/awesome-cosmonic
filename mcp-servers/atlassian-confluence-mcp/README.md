# atlassian-confluence-mcp

Confluence Cloud MCP server: CQL search, pages (read/list/children/create/
update/trash), spaces, footer and inline comments, labels and attachments over
the Confluence Cloud REST API v2 (`/wiki/api/v2`) plus the three v1 endpoints
v2 still lacks (search, current user, add label). Authentication is an
Atlassian API token sent as HTTP Basic (`email:token`) — a static credential,
so it fits the sandbox with no OAuth flow.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://atlassian-confluence-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://atlassian-confluence-mcp.localhost:8200/>.

## Tools

Page ids accept a numeric id or a page URL; space ids accept a numeric id or a
space key (resolved with one extra call). Lists are cursor-paginated: pass
`next_cursor` back as `cursor`. Every list `limit` is clamped to its range.

| Tool | Parameters | Output | Gated |
|---|---|---|---|
| `check_auth` | — | `status: ok\|missing\|invalid\|insufficient`, account identity, route, gates in force, `remediation` | no |
| `get_current_user` | — | `accountId`, `email`, `displayName`, `publicName`, `accountType`, `timeZone` | no |
| `search` | `cql` or `text`, `space_key?`, `limit` 1..100, `cursor?` | hits `{id, type, status, title, spaceKey, url, lastModified, excerpt}`, `totalSize`, `next_cursor` | no |
| `list_pages` | `space_id?`, `title?` (exact), `status?`, `sort?`, `limit` 1..250, `cursor?` | page metadata list, `spaceId`/`spaceKey`, `next_cursor` | no |
| `get_page` | `page_id`, `format?` text\|storage\|atlas_doc_format\|view, `max_chars` 1000..200000, `version?` | metadata, `version`, `labels`, `url`, `body` (+ `body_chars`, `body_truncated`) | no |
| `get_page_children` | `page_id`, `sort?`, `limit` 1..250, `cursor?` | child pages with `childPosition`, `next_cursor` | no |
| `list_spaces` | `keys?` (≤ 50), `type?`, `status?`, `limit` 1..250, `cursor?` | `{id, key, name, type, status, homepageId, description, url}` list | no |
| `get_space` | `space` (id or key) | one space | no |
| `create_page` | `space_id`, `title`, `body`, `body_format?` markdown\|storage\|wiki, `parent_id?` | created page metadata (`id`, `version`, `url`) | `CONFLUENCE_READ_ONLY` |
| `update_page` | `page_id`, `title?`, `body?`, `body_format?`, `version_message?`, `minor_edit?`, `expected_version?` | updated page metadata, `previous_version` | `CONFLUENCE_READ_ONLY` |
| `delete_page` | `page_id`, `confirm` (must be `true`) | `{deleted: true, id, status: 204}` — trash only, never purge | `CONFLUENCE_READ_ONLY`, `CONFLUENCE_ALLOW_DELETE`, `confirm` |
| `get_comments` | `page_id`, `kind?` footer\|inline, `sort?`, `limit` 1..250, `cursor?` | comments with bodies rendered to text, `parentCommentId`, `version`, `next_cursor` | no |
| `add_comment` | `page_id` or `reply_to_comment_id`, `body`, `body_format?` | created comment | `CONFLUENCE_READ_ONLY` |
| `get_labels` | `page_id`, `prefix?`, `limit` 1..250, `cursor?` | `{id, name, prefix}` list | no |
| `add_label` | `page_id`, `labels` (1..20) | resulting label list | `CONFLUENCE_READ_ONLY` |
| `list_attachments` | `page_id`, `media_type?`, `filename?`, `limit` 1..250 (default 50), `cursor?` | attachments with `fileSize`, `mediaType`, `url`, absolute `download_url` | no |

Write tools take **markdown by default** and convert it to Confluence storage
format (fenced code → code macro, task lists → `ac:task-list`, tables, links,
images). `get_page` renders storage/ADF to readable text by default and can
return the raw storage XHTML for round-trip edits. `update_page` reads the live
version first, sends `version + 1`, and refuses without writing when
`expected_version` differs from the live version. Control characters that
XML 1.0 forbids (NUL, ESC, form feed, DEL, …) are stripped from titles,
bodies and version messages so a pasted terminal transcript cannot earn a
`400 Error parsing xhtml`.

Full argument details, limits and result shapes:
[`skills/server/references/TOOLS.md`](skills/server/references/TOOLS.md);
CQL syntax: [`skills/server/references/CQL.md`](skills/server/references/CQL.md).

## Skill

The server publishes its operating manual over MCP resources
(`io.modelcontextprotocol/skills`):

| URI | Content |
|---|---|
| `skill://index.json` | Catalog (name + trigger description) |
| `skill://atlassian-confluence-mcp/SKILL.md` | When to use the server, tool sequencing, body-format rules, the optimistic-locking story, the error catalogue |
| `skill://atlassian-confluence-mcp/references/TOOLS.md` | Tool reference table |
| `skill://atlassian-confluence-mcp/references/CQL.md` | CQL cheat sheet |

## Configuration

| Env var | Kind | Default | Required | Purpose |
|---|---|---|---|---|
| `ATLASSIAN_SITE` | named config | — | yes | Atlassian Cloud site: `acme` or `acme.atlassian.net` (must match `allowedHosts`) |
| `ATLASSIAN_EMAIL` | named config | — | yes | Email of the account that owns the API token (Basic user) |
| `ATLASSIAN_API_TOKEN` | secret ref `atlassian-api-token` | — | yes | Atlassian API token (Basic password); **one ref serves both this server and `atlassian-jira-mcp`** |
| `ATLASSIAN_CLOUD_ID` | named config | unset | no | Only for tokens created *with scopes*: routes through `https://api.atlassian.com/ex/confluence/<cloudId>` (the `cloudId` from `https://<site>.atlassian.net/_edge/tenant_info`) |
| `ATLASSIAN_BASE_URL` | test override | `https://<site>` | no | Replaces the origin; the e2e points it at a local fixture |
| `CONFLUENCE_READ_ONLY` | named config | `false` | no | `true` refuses `create_page`, `update_page`, `delete_page`, `add_comment`, `add_label` without dialing |
| `CONFLUENCE_ALLOW_DELETE` | named config | `false` | no | `delete_page` (trash only) is refused unless `true` |
| `CONFLUENCE_SPACES_FILTER` | named config | unset | no | Comma-separated space keys; scopes `search`, `list_spaces`, `list_pages` and `create_page` |
| `MCP_ALLOWED_HOSTS` | named config | `atlassian-confluence-mcp.localhost` | yes | DNS-rebinding guard; must equal the ingress host |
| `RUST_LOG` | named config | `info` | no | Log filter |

### Secret registration

Create an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>
(choose the classic/unscoped token unless you also set `ATLASSIAN_CLOUD_ID`;
every token expires, at most after one year). Prefer pasting it into Cosmonic
Desktop → Secrets as `atlassian-api-token` (env `ATLASSIAN_API_TOKEN`); the
daemon API equivalent is:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock   # Linux; macOS: ~/Library/Application\ Support/Cosmonic/cosmonicd.sock
$ curl --unix-socket "$SOCK" http://localhost/v1/secrets/refs          # skip if atlassian-jira-mcp already registered it
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"atlassian-api-token","uri":"keychain://cosmonic/atlassian-api-token","env":"ATLASSIAN_API_TOKEN","value":"<token>"}'
```

(or `cosmonic_set_secret name=atlassian-api-token uri=keychain://cosmonic/atlassian-api-token env=ATLASSIAN_API_TOKEN value=<token>`).
Then call `check_auth`. A missing token yields a distinct tool error naming the
variable, the ref and the token page; an invalid one surfaces Confluence's 401
text plus the same hint. `GET /` carries a `credentials` block reporting
whether the token is configured (presence only).

### `allowedHosts`

Exactly the hosts the tools dial, in `deploy/workload.yaml` and mirrored in
`.wash/config.yaml`:

- `https://your-site.atlassian.net` — your Confluence Cloud site (replace
  `your-site`; `*.atlassian.net` is the looser alternative);
- `https://api.atlassian.com` — the scoped-token gateway, only dialed when
  `ATLASSIAN_CLOUD_ID` is set.

No loopback ports, host volumes or extra host interfaces are needed.

## Build and test

```console
$ cargo build --release                                  # target/wasm32-wasip2/release/atlassian_confluence_mcp.wasm
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ wasm-tools component wit target/wasm32-wasip2/release/atlassian_confluence_mcp.wasm | grep 'export wasi:http/handler@0.3.0'
$ scripts/e2e.sh                                         # hermetic; needs wasmtime 46/47, python3, curl
```

`scripts/e2e.sh` starts a threaded Python fixture impersonating Confluence
(recording every request at `/__last` so encoding, clamping and auth headers
are asserted), then four wasmtime instances: the primary, a guard without the
secret (missing-secret path + Host guard), one with
`CONFLUENCE_ALLOW_DELETE=true` + `CONFLUENCE_SPACES_FILTER=ENG,DOCS`, and a
read-only one on the `ATLASSIAN_CLOUD_ID` gateway route. Cases cover every tool
(happy path, malformed and adversarial input, limit clamping, cursor
pass-through, markdown→storage and storage/ADF→text rendering, the
optimistic-lock flow, 400/401/403/404/409/429/500 mapping including the single
short `Retry-After` retry, and the write gates). `E2E_LIVE=1` with
`ATLASSIAN_SITE`/`ATLASSIAN_EMAIL`/`ATLASSIAN_API_TOKEN` exported adds three
read-only calls against the real site.

## Deploy on Cosmonic Desktop

Edit `deploy/workload.yaml`: replace `your-site` (in `ATLASSIAN_SITE` and
`allowedHosts`), set `ATLASSIAN_EMAIL`, and register the secret ref as above.
Then, for a local build:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/atlassian-confluence-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/atlassian-confluence-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"atlassian-confluence-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/atlassian-confluence-mcp:0.1.0@sha256:…"}
$ IMAGE=oci.localhost:8200/apps/atlassian-confluence-mcp:0.1.0@sha256:…
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads -H 'Content-Type: application/json' --data-binary @-
```

(or apply the manifest with the `cosmonic_apply_workload` MCP tool / the
Desktop UI; for the published image apply `deploy/workload.yaml` as-is).
Verify:

```console
$ curl -s http://atlassian-confluence-mcp.localhost:8200/ | jq '.status, .capabilities.tools, .credentials[0].status'
```

Deployment docs: <https://cosmonic.com/docs/desktop>.

## Talk to it

Discovery (no handshake needed):

```console
$ curl -s http://atlassian-confluence-mcp.localhost:8200/
{
  "status": "ok",
  "server": { "name": "atlassian-confluence-mcp", "version": "0.1.0", "description": "Confluence Cloud MCP server: …" },
  "protocol": { "specVersion": "2026-07-28", "transport": "streamable-http", "stateless": true },
  "capabilities": { "tools": ["add_comment", "add_label", "check_auth", "create_page", …], "resources": { "skillIndex": "skill://index.json" } },
  "credentials": [{ "ref": "atlassian-api-token", "env": "ATLASSIAN_API_TOKEN", "status": "configured", "validate": "check_auth", … }]
}
```

List tools and call one (2026-07-28 stateless conventions: `Mcp-Method` /
`Mcp-Name` headers and a per-request `_meta`):

```console
$ curl -s -X POST http://atlassian-confluence-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://atlassian-confluence-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/call' -H 'Mcp-Name: search' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"text":"kubernetes","space_key":"ENG","limit":5},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"{\"cql\":\"(text ~ \\\"kubernetes\\\" AND type = page) AND space = \\\"ENG\\\" ORDER BY lastmodified DESC\",\"count\":5,\"limit\":5,\"next_cursor\":\"…\",\"results\":[{\"id\":\"123456\",\"title\":\"Kubernetes Runbook\",\"spaceKey\":\"ENG\",\"url\":\"https://acme.atlassian.net/wiki/spaces/ENG/pages/123456/Kubernetes+Runbook\",…}],\"totalSize\":42}"}],"structuredContent":{…},"isError":false}}
```

Connect a client:

```console
$ claude mcp add --transport http atlassian-confluence-mcp http://atlassian-confluence-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"atlassian-confluence-mcp":{"type":"http","url":"http://atlassian-confluence-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [sooperset/mcp-atlassian](https://github.com/sooperset/mcp-atlassian) (MIT)
  — the Confluence tool surface and parameter names (`confluence_search`,
  `get_page`, `get_page_children`, `get_comments`, `add_comment`,
  `get_labels`, `add_label`, `create_page`, `update_page`, `delete_page`), the
  read-only mode and spaces-filter ideas, and the markdown↔storage approach.
  No code was copied (it is Python); the conversions are re-implemented in
  Rust.
- [atlassian/atlassian-mcp-server](https://github.com/atlassian/atlassian-mcp-server)
  (Apache-2.0) — hosted/OAuth only, nothing runnable; it confirms the
  `Authorization: Basic base64(email:api_token)` header and the read/write/
  search permission grouping mirrored by `CONFLUENCE_READ_ONLY`.
- [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark) (MIT) —
  Markdown parsing for the markdown→storage conversion.
- The Confluence Cloud REST API v2/v1 reference (Atlassian docs) — endpoints,
  parameter names, limit bounds and response codes.
- `atlassian-jira-mcp` (this repository, Apache-2.0) — the shared
  `atlassian-api-token` ref, the `ATLASSIAN_CLOUD_ID` gateway route and the
  credential/error-description shape, kept consistent between the two servers.

This port is Apache-2.0 (see `LICENSE`).

## Known limitations

- Storage-format conversion is lossy in both directions: `get_page`
  `format=text` reduces macros to `[macro:name]` markers (Jira issue, TOC,
  excerpt-include and similar macros lose their content), and `update_page`
  with markdown replaces the whole body. Read and write `storage` when a page
  carries macros you must keep.
- Attachment download is not a tool (bytes would be base64 in the MCP result);
  `list_attachments` returns an absolute `download_url` to fetch with the same
  Basic credentials. Uploads are not supported.
- Only classic API tokens work against `https://<site>.atlassian.net`; tokens
  created *with scopes* need `ATLASSIAN_CLOUD_ID` (gateway route). OAuth 2.0
  (3LO) and service-account Bearer keys are out of scope.
- `search`, `get_current_user` and `add_label` rely on v1 endpoints that v2
  has no replacement for (not deprecated as of 2026-09).
- XML-illegal control characters are dropped silently (tab/LF/CR are kept);
  ANSI colour codes in pasted terminal output lose their `ESC` byte.
- The MCP transport caps a request at 4 MiB, so page bodies arrive well under
  Confluence's 5 MB limit; the server additionally refuses bodies that grow
  past 4 MiB during markdown→storage conversion.
- `CONFLUENCE_SPACES_FILTER` scopes the listing/search/create tools; it is a
  convenience, not a security boundary — `get_page` by id still honours only
  Confluence's own permissions.
- The single in-process 429 retry only fires when `Retry-After` ≤ 5 s;
  longer waits are returned as `retry_after_seconds` for the agent to back off.
- The shared secret ref is named `atlassian-api-token` (one ref per Atlassian
  account, serving Jira and Confluence) rather than the
  `<name>-<purpose>` pattern.
