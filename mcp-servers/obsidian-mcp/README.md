# obsidian-mcp

An MCP server for the user's **Obsidian vault**, running as a sandboxed
WebAssembly component on [Cosmonic Desktop](https://cosmonic.com/docs/desktop).
It talks to the **"Local REST API with MCP"** community plugin
([coddingtonbear/obsidian-local-rest-api](https://github.com/coddingtonbear/obsidian-local-rest-api))
inside the running Obsidian and lets an agent list, read, search, create,
append, patch and delete notes, list tags, read the active or periodic note,
and (opt-in) run Obsidian commands.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://obsidian-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://obsidian-mcp.localhost:8200/>.

## How it reaches Obsidian

The plugin runs inside Obsidian on the developer's machine. By default it
listens on **HTTPS 27124 with a self-signed CA**, which a sandboxed component
cannot trust (webpki roots only), so this server uses the plugin's opt-in
**plain-HTTP listener on 27123** ("Enable Non-encrypted (HTTP) Server" in the
plugin settings), reached through the Desktop loopback sentinel
`host.wasmcloud.internal:27123`. Only the vault open in that Obsidian window is
reachable, and only while Obsidian is running.

Every request carries `Authorization: Bearer <API key>`; the key is shown
(with a copy button) in Obsidian Settings -> Community plugins -> Local REST API
with MCP. No OAuth, no callback server.

## Tools

| Tool | Params | Output | Gated? |
|---|---|---|---|
| `check_auth` | none | `status` ok/missing/invalid/unreachable, identity (plugin + Obsidian version), `remediation` | no |
| `get_server_info` | none | service, versions, `authenticated`, `patch_format`, gates, hints | no (works without a key) |
| `list_files_in_vault` | none | `entries` (`x.md` / `dir/`), `folders`, `files`, `count` | key |
| `list_files_in_dir` | `dirpath` | same for one folder (404 = missing **or** empty) | key |
| `get_file_contents` | `filepath`, `format?` markdown/metadata/document_map, `heading?[]`, `block?`, `frontmatter_key?`, `scope?` | `content` or `data`, `truncated`, `content_type` | key |
| `batch_get_file_contents` | `filepaths[]` (<= 200 entries; first 20 distinct read) | `# path` sections joined by `---`; per-file status; inline `Error 404`; `not_attempted` when the call budget runs out | key |
| `simple_search` | `query`, `context_length?` (0..1000), `limit?` (1..100), `max_matches_per_file?` (1..50) | ranked `results[]` with snippets, `total_files`, `truncated` | key |
| `complex_search` | `query` (JsonLogic object <= 64 KiB), `limit?` (1..500) | `results[{filename, result}]`, `total`, `truncated` | key |
| `get_recent_changes` | `days?` (1..3650), `limit?` (1..100) | `notes[{path, mtime_ms, mtime}]` newest first | key |
| `list_tags` | `limit?` (1..2000), `prefix?` | `tags[{name, count}]` | key |
| `get_active_file` | `format?` | `path`, `content`/`data` | key |
| `append_content` | `filepath`, `content` (<= 1 MiB), `heading?[]`, `reject_if_content_preexists?`, `allow_non_markdown?` | `status: appended`, `bytes`, `duplicate_check`, `updated_content` (targeted) | `OBSIDIAN_READ_ONLY` |
| `put_content` | `filepath`, `content` (<= 1 MiB), `require_existing?` | `status: written`, `bytes` | `OBSIDIAN_READ_ONLY` |
| `patch_content` | `filepath`, `operation`, `target_type`, `target`, `content?` xor `value?`, `scope?`, `within?`, `create_target_if_missing?`, `if_match?` | `updated_content`, `warnings`, `patch_format` | `OBSIDIAN_READ_ONLY` |
| `delete_file` | `filepath`, `permanent?`, `confirm` (must be true) | `deleted`, `permanent` | `OBSIDIAN_READ_ONLY` + `confirm` |
| `get_periodic_note` | `period`, `date?` (YYYY-MM-DD), `format?` | resolved `path`, `content`/`data` | key |
| `get_recent_periodic_notes` | `period`, `limit?` (1..10), `include_content?`, `tz_offset_minutes?` (-840..840) | `notes[{date, resolved_by, path, content?}]`, `found`, `skipped`, `duplicates`, `budget_exhausted` | key |
| `open_file` | `filepath`, `new_leaf?` | `opened` (creates a missing note) | `OBSIDIAN_READ_ONLY` |
| `list_commands` | `filter?`, `limit?` (1..1000) | `commands[{id, name}]` | `OBSIDIAN_ENABLE_COMMANDS` |
| `execute_command` | `command_id` | `executed` | `OBSIDIAN_ENABLE_COMMANDS` + `OBSIDIAN_READ_ONLY` |

Every success carries `structuredContent` plus a readable text block; upstream
and policy failures are `isError: true` with an actionable message (the error
catalogue lives in the skill). Full argument tables:
`skill://obsidian-mcp/references/TOOLS.md`; PATCH rules:
`skill://obsidian-mcp/references/PATCH.md`.

## Skill

- `skill://index.json` — catalog
- `skill://obsidian-mcp/SKILL.md` — when to use which tool, sequencing, transport
  facts, PATCH rules, periodic-note caveats, the error catalogue
- `skill://obsidian-mcp/references/TOOLS.md`, `skill://obsidian-mcp/references/PATCH.md`

## Configuration

| Env var | Kind | Default | Required |
|---|---|---|---|
| `OBSIDIAN_API_KEY` | secret ref `obsidian-mcp-api-key` | — | yes (all tools except `check_auth` / `get_server_info`) |
| `OBSIDIAN_BASE_URL` | named config | `http://host.wasmcloud.internal:27123` | no (the e2e points it at a fixture) |
| `OBSIDIAN_READ_ONLY` | named config | `false` | no — `true` refuses append/put/patch/delete/open_file/execute_command |
| `OBSIDIAN_ENABLE_COMMANDS` | named config | `false` | no — `true` enables list_commands/execute_command |
| `OBSIDIAN_MAX_CONTENT_CHARS` | named config | `100000` (1000..2000000) | no |
| `MCP_ALLOWED_HOSTS` | named config | `obsidian-mcp.localhost` | yes (DNS-rebinding guard = ingress host) |
| `RUST_LOG` | named config | `info` | no |
| `MCP_OUTBOUND_TIMEOUT_MS` / `MCP_OUTBOUND_MAX_BYTES` | template overrides | 30000 / 4 MiB | no |

`GET /` carries a `credentials` block (presence only, never values) naming the
ref, the env var, where to obtain the key, and `check_auth` as the validator.
`status` is `configured` only for a real key: a ref registered with the
placeholder value `REPLACE_ME` (what an automated first deploy creates) reports
`missing` with `placeholder: true`, and every tool refuses with an error naming
the ref until the value is overwritten — so `configured` on `GET /` does mean
a key is present. The same list, minus the runtime fields, is the
`desktop.cosmonic.com/credentials` annotation in `deploy/workload.yaml`
(`scripts/e2e.sh` fails if the two drift).

### Register the API key

In Obsidian: Settings -> Community plugins -> Browse -> install and enable
**Local REST API with MCP** (deep link `obsidian://show-plugin?id=obsidian-local-rest-api`).
Open its settings tab, copy the API key, and switch on **Enable Non-encrypted
(HTTP) Server** (port 27123). Keep Obsidian running with the vault open.

Paste the key in Cosmonic Desktop -> Secrets as `obsidian-mcp-api-key`
(env `OBSIDIAN_API_KEY`), or:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock     # Linux; macOS: "$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"obsidian-mcp-api-key","uri":"keychain://cosmonic/obsidian-mcp-api-key","env":"OBSIDIAN_API_KEY","value":"<key>"}'
```

or with the MCP tool: `cosmonic_set_secret name=obsidian-mcp-api-key
uri=keychain://cosmonic/obsidian-mcp-api-key env=OBSIDIAN_API_KEY value=<key>`.
Verify from the host first: `curl -H 'Authorization: Bearer <key>' http://127.0.0.1:27123/`
must answer `"authenticated": true`.

## Outbound policy and the loopback grant

`allowedHosts` lists exactly one host: `host.wasmcloud.internal:27123` (the
plugin's HTTP listener on this machine). Reaching a service on the developer's
own machine needs three things, in both `deploy/workload.yaml` and Desktop:

1. `localResources.allowedHosts: ["host.wasmcloud.internal:27123"]`
2. `localResources.allowedHostLoopbackPorts: ["27123"]`
3. Desktop Settings -> Security -> **allow host loopback** (default off;
   `PUT /v1/egress {"allow_host_loopback": true}`)

Until all three line up, every tool returns `could not reach Obsidian ...`
with either `HttpRequestDenied` or `DnsError(... "address not available")`
(the sentinel name does not resolve until the door is open) plus that
checklist; `check_auth` reports `status: unreachable`. `connection refused` means Obsidian is closed
or the HTTP listener is off. No volumes and no other host interfaces are used:
the server never touches the vault on disk.

## Build and test

```console
$ cargo build --release                       # wasm32-wasip2 by default (.cargo/config.toml)
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ scripts/e2e.sh                              # hermetic: Python fixture impersonates plugin 5.1.0
$ E2E_LIVE=1 OBSIDIAN_API_KEY=<key> scripts/e2e.sh   # + read-only smoke against a running Obsidian
```

The e2e starts `scripts/obsidian_fixture.py` (a threaded stand-in for the
plugin with an in-memory vault, the `{errorCode, message}` envelope, both
PATCH formats, the periodic-notes 307 redirect and each of its error codes)
and two `wasmtime serve`
instances: one with the key and commands enabled, one without the key and
read-only (missing-secret path, gates, Host guard). `cargo test` is not used
(wasm target).

## Deploy on Cosmonic Desktop

`deploy/workload.yaml` is the only manifest. After registering the secret and
enabling the loopback grant:

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/obsidian-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/obsidian-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"obsidian-mcp:0.1.0","rebuild":true}'
# -> {"image":"oci.localhost:8200/apps/obsidian-mcp:0.1.0@sha256:...", ...}
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "<that image>" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads -H 'Content-Type: application/json' --data-binary @-
```

(or `cosmonic_apply_workload` with the pinned image). Then:

```console
$ curl -s http://obsidian-mcp.localhost:8200/ | jq '.status, .credentials[0].status'
$ curl -s -X POST http://obsidian-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
$ curl -s -X POST http://obsidian-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: check_auth' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"check_auth","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

`check_auth` answers `status: ok` once the key, the HTTP listener and the
loopback grants are in place; otherwise it names exactly what is missing.

### Connect a client

```console
$ claude mcp add --transport http obsidian-mcp http://obsidian-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"obsidian-mcp":{"type":"http","url":"http://obsidian-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

If you only need the plugin's own MCP surface from a client on the same
machine, the plugin also serves `POST http://127.0.0.1:27123/mcp/` with the
same Bearer header; this server exists for the sandboxed, Desktop-catalogued
path with the skill, clamps and the read-only / command gates.

## Borrowed from

- [coddingtonbear/obsidian-local-rest-api](https://github.com/coddingtonbear/obsidian-local-rest-api)
  (MIT) — endpoint list, content types, the 5.x PATCH instruction schema, the
  5-digit `errorCode` table and its messages (reproduced in the error
  catalogue), NoteJson / document-map shapes.
- [coddingtonbear/obsidian-local-rest-api-periodic-notes](https://github.com/coddingtonbear/obsidian-local-rest-api-periodic-notes)
  (MIT) — `/periodic/{period}/[{y}/{m}/{d}/]` routes and the 307 redirect.
- [MarkusPfundstein/mcp-obsidian](https://github.com/MarkusPfundstein/mcp-obsidian)
  (MIT) — tool naming and semantics (`list_files_in_vault`, `batch_get_file_contents`
  with `---` separators and continue-on-error, `simple_search`, `complex_search`,
  `append_content`, `put_content`, `patch_content`, `delete_file`, periodic tools,
  `get_recent_changes`). Its header-based PATCH, `/periodic/{period}/recent` and
  Dataview DQL calls are stale against plugin 5.x and were not borrowed.
- [cyanheads/obsidian-mcp-server](https://github.com/cyanheads/obsidian-mcp-server)
  (Apache-2.0) — the config pattern: base-URL override, `OBSIDIAN_ENABLE_COMMANDS`
  opt-in, `OBSIDIAN_READ_ONLY` gate, mandatory `confirm` on delete.

This port is Apache-2.0 (see `LICENSE`); nothing was copied verbatim.

## Known limitations

- Requires the plugin's plain-HTTP listener: the default HTTPS listener
  (27124) uses a plugin-generated self-signed CA that the sandbox cannot trust.
  The API key therefore travels in cleartext on loopback.
- One vault per server (the running window's); multi-vault setups need a port
  and a workload per vault.
- Periodic-note tools need the companion plugin "Local REST API - Periodic
  Notes" (the core plugin dropped `/periodic/` in 5.0.2). Its routes answer
  404 for three different things, so the server classifies by the plugin's
  `errorCode` rather than the status: 40461 = no note for that date yet
  (create it with `put_content` / `append_content`; `get_recent_periodic_notes`
  counts it as skipped), 40460 = period not configured, 40400 or no envelope =
  route not served (companion missing); 400/40060 = period switched off;
  anything else is reported as upstream said it. Dataview DQL search is gone
  in 5.1 (use `get_recent_changes` / `complex_search`).
- Search endpoints walk the whole vault synchronously in Obsidian's renderer
  and return unbounded results; the server clamps output and the 30 s outbound
  deadline can trip on very large vaults. Multi-read tools
  (`batch_get_file_contents`, `get_recent_periodic_notes`) stop issuing
  requests after 2 x that deadline and report what they skipped.
- `get_recent_periodic_notes` computes the earlier dates from UTC unless
  `tz_offset_minutes` is passed; the current period always comes from the
  plugin (Obsidian's local day), and an overlap is listed once.
- The plugin < 5 legacy PATCH path is implemented from the documented header
  format and exercised only against the fixture.
- `reject_if_content_preexists` without a `heading` is emulated: the plugin
  only honours the header on heading-targeted writes, so the server reads the
  note first and refuses when the text is already there. That is one extra
  request and not atomic; a concurrent writer can still produce a duplicate.
- Write bodies are sent as bare `text/markdown` (no `charset` parameter): the
  plugin's Content-Type guard on targeted writes is an exact string match.
- `open_file` creates a missing note and `execute_command` runs any palette
  command; both are gated but an operator enabling commands hands the agent
  app-level control.
- No live upstream in CI: Obsidian is an Electron desktop app; fidelity rests
  on the fixture mirroring plugin 5.1.0.
