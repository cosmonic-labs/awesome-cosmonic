---
name: obsidian-mcp
description: Read, search, create, append, patch and delete notes in the user's running Obsidian vault (via the Local REST API with MCP plugin), list tags, read the active or periodic note, and run Obsidian commands. Use when a task mentions Obsidian, a vault, markdown notes, daily notes, or note tags, and to interpret this server's errors.
---

# Using the obsidian-mcp MCP server

This server is a sandboxed WebAssembly component on Cosmonic Desktop that
talks to the **"Local REST API with MCP"** community plugin running inside the
user's Obsidian. It is **stateless**: nothing you set on one call carries into
the next. It reaches exactly one vault — the one open in the Obsidian window
the plugin is listening from — and only while Obsidian is running.

Full argument tables: [references/TOOLS.md](references/TOOLS.md). PATCH rules:
[references/PATCH.md](references/PATCH.md).

## Start here

1. **Call `check_auth` first.** It answers `status: ok | missing | invalid |
   unreachable` with the exact remediation. `missing`/`invalid` are permanent
   until a human changes the `obsidian-mcp-api-key` secret — never retry them.
   `missing` with `placeholder: true` means the ref exists but still holds the
   registration placeholder `REPLACE_ME` (nothing is sent upstream then).
   `unreachable` with "denied" or `DnsError ... address not available` means a
   Desktop grant is missing (also permanent); "refused" means Obsidian is
   closed or the HTTP listener is off; only "timed out" deserves one retry.
2. `get_server_info` gives the same facts plus the plugin version and the
   PATCH format the server will use. `GET /` upstream always answers 200, even
   with a wrong key: `authenticated: false` is the credential failure signal
   there, and an HTTP 401 `{errorCode: 40101}` on any other tool means the same.
3. Orient with `list_files_in_vault`, then `list_files_in_dir`, `list_tags`,
   `get_recent_changes`, or `simple_search` before reading notes.

## Transport facts you cannot infer

- The plugin listens on HTTPS **27124** by default with a self-signed CA the
  sandbox cannot trust. The server only works against the opt-in plain-HTTP
  listener (Obsidian Settings -> Local REST API with MCP -> *Enable
  Non-encrypted (HTTP) Server*, port **27123**) reached through
  `host.wasmcloud.internal:27123`, which needs three Desktop grants:
  `allowedHosts ["host.wasmcloud.internal:27123"]`,
  `allowedHostLoopbackPorts ["27123"]`, and Settings -> Security -> *allow host
  loopback*. `HttpRequestDenied` / `denied` / `DnsError: address not available` =
  a grant is missing (the sentinel name does not even resolve until the door is open).
- Only one vault at a time (the running window's). Multiple vaults need
  per-vault ports and separate workloads.

## Paths and targets

- Paths are vault-relative (`Projects/Plan.md`): no leading `/`, no `..`. The
  server pre-validates and percent-encodes **each segment separately**, so a
  `/` inside a heading text is `%2F` within its own segment and real separators
  survive. You never encode anything yourself.
- Directory listings return notes as `name.md` and folders as `dir/`. The
  plugin answers **404 for an empty folder as well as a missing one** — list the
  parent to tell them apart.
- Heading targets are arrays from the top-level heading down:
  `["Projects", "Q3"]`. A single string `"Projects::Q3"` is split for you.
  Block ids are bare (`abc123`, no `^`). Frontmatter targets are the key.
- Duplicate headings: only the first is addressable by text; read
  `get_file_contents format=document_map` and use the exact key it shows.

## Reading

- `get_file_contents` returns markdown by default; `format=metadata` gives
  parsed frontmatter/tags/links/backlinks and `stat` (epoch **milliseconds**);
  `format=document_map` gives the headings tree, block ids, frontmatter fields
  and the `version` token used for optimistic patches.
- Output is clamped to `OBSIDIAN_MAX_CONTENT_CHARS` (default 100 000) with a
  `...[truncated N chars]` marker and `truncated: true`; read a heading section
  instead of the whole note when it trips. Binary attachments are refused with
  their content type, never dumped.
- `batch_get_file_contents` accepts at most 200 entries (longer lists are
  refused, nothing sent) and reads the first 20 distinct paths
  (`clamped: true` when more were given); a missing note becomes an inline
  `Error 404` and the batch continues. Bad key / transport failures abort it,
  since they would repeat for every file. The call has a wall-clock budget of
  2 x `MCP_OUTBOUND_TIMEOUT_MS` (60 s by default): once it is spent the
  remaining paths come back as `skipped: true` / `not_attempted` — re-issue
  the batch with just those paths rather than the whole list.
- `get_active_file` = the note focused in the Obsidian UI (404 when none).

## Searching

- `simple_search` is Obsidian's full-text search: it also matches the note
  **basename** (`source: "filename"`), returns unbounded results upstream and
  walks every markdown file synchronously in Obsidian's renderer. The server
  clamps `limit` (1..100 files) and `max_matches_per_file` (1..50) and sets
  `truncated` — narrow the query rather than paging (there is no paging).
- `complex_search` takes a JsonLogic object over note metadata (`path`,
  `tags` without `#`, `frontmatter.*`, `stat.mtime`/`ctime`/`size` in ms,
  `links`, `backlinks`) with `glob`/`regexp` operators; only truthy results
  come back. **`content` is populated only when the query text literally
  contains `"content"`.** Results are cut to `limit` (1..500) and each value to
  2000 characters.
- `get_recent_changes` is the replacement for the old Dataview `TABLE` query
  (plugin 5.1 dropped Dataview support): a JsonLogic `stat.mtime` filter,
  sorted newest first in the server.
- Large vaults can hit the 30 s outbound deadline on either search. Retry
  once, then narrow (shorter query, a `glob` on `path`).

## Writing (all refused when `OBSIDIAN_READ_ONLY=true`)

- `append_content` appends at the end (plugin adds a newline) or inside a
  heading; it **creates the note and parent folders** silently. Non-`.md` paths
  need `allow_non_markdown=true`. `reject_if_content_preexists` turns a
  duplicate into an "already present" error, which means "already done": with
  a `heading` the plugin enforces it (HTTP 409); without one the server reads
  the note first and refuses in-guest, because the plugin ignores the flag on
  whole-note appends. That check costs one extra request and is not atomic (a
  concurrent writer can slip in between the read and the append); the result
  reports which check ran in `duplicate_check` (`plugin` / `note-read`).
- `put_content` overwrites the whole note; `require_existing=true` refuses to
  create.
- `patch_content` is a structured edit of one target. Plugin >= 5 takes a JSON
  instruction (`targetType`, array `target`, `operation`, exactly one of
  `content`/`value`); older plugins get the deprecated header form, chosen from
  the plugin version cached by `check_auth`/`get_server_info`. The heading line
  itself is never part of `content` scope — do not include it or it doubles.
  Rename via `scope=marker` with the bare text (no `#`). Frontmatter `append`
  merges into a list and there is no remove-item op: read, then `replace` the
  whole list. Details and failure codes in [references/PATCH.md](references/PATCH.md).
- `delete_file` goes to Obsidian's trash unless `permanent=true` and **always
  needs `confirm=true`** (otherwise nothing is sent). Folders cannot be deleted.
- `open_file` **creates** a missing note, which is why it is write-gated.
- `list_commands`/`execute_command` stay registered but refuse unless the
  operator set `OBSIDIAN_ENABLE_COMMANDS=true`: commands can do anything the
  app can. `execute_command` is also write-gated.

## Periodic notes

`/periodic/` left the core plugin in 5.0.2: `get_periodic_note` and
`get_recent_periodic_notes` need the companion plugin **"Local REST API -
Periodic Notes"** plus a configured Daily Notes / Periodic Notes plugin. The
companion answers a 307 redirect to the resolved `/vault/` path, which the
server follows exactly once (same origin, same Accept). Three different
conditions come back as 404 on `/periodic/`, so the server classifies the
first hop by the plugin's `errorCode`, never by the status alone: 404
errorCode 40461 = the period is enabled but has no note for that date yet
(create it with `put_content` / `append_content`, or `open_file`); 404
errorCode 40460 = Obsidian has no such period configured; 404 errorCode 40400
(or a bare 404 with no envelope) = the route is not served at all, i.e. the
companion plugin is missing; 400 errorCode 40060 = the period exists but is
switched off; 500 errorCode 50060 = the companion failed. A 404 *after* the
redirect = the note vanished between the hops (the message names the would-be
path). Any other answer is reported exactly as upstream said it. There is no
"recent" route:
`get_recent_periodic_notes` asks the plugin for the *current* period (undated
route, so it is Obsidian's local "today"), then steps back from today in the
server for the rest — UTC unless you pass `tz_offset_minutes` (the user's UTC
offset, -840..840). Missing periods are `skipped`; if the plugin's day and the
computed one overlap (offset not given near midnight) the note is listed once
and `duplicates` counts it — pass the offset. Each lookup is one exchange;
past 2 x `MCP_OUTBOUND_TIMEOUT_MS` the tool stops with `budget_exhausted:
true` and `attempted < lookups` — call again with a smaller `limit`.

## Error catalogue

| You see | It means | Do |
|---|---|---|
| `check_auth` status `missing`, or "OBSIDIAN_API_KEY is not set" | secret ref `obsidian-mcp-api-key` not registered | Ask the user to register the key; do not retry |
| "still holds the placeholder value REPLACE_ME" (`placeholder: true`) | the ref was created but never filled | Ask the user to overwrite it (`cosmonic_set_secret ... value=<key>`); do not retry |
| "filepaths has N entries; send at most 200" | batch list too long (nothing sent) | Split into calls of <= 20 distinct paths |
| "time budget was spent" / `budget_exhausted: true` | a multi-read call ran out of its 2 x timeout budget (Obsidian is answering slowly) | Re-issue with the `skipped` paths / a smaller `limit`; do not resend the whole list |
| "tz_offset_minutes must be between -840 and 840" | offset outside UTC-14..UTC+14 | Pass the user's real UTC offset in minutes |
| status `invalid`, `authenticated: false`, HTTP 401 errorCode 40101 | key wrong/stale/other vault | Same; the plugin can regenerate keys |
| "HttpRequestDenied" / "denied" / `DnsError ... address not available` on host.wasmcloud.internal | a Desktop grant is missing (the sentinel name does not resolve until the loopback door is open) | Fix manifest / Settings -> Security; retrying is pointless |
| "connection refused" / "reset" | Obsidian closed, plugin off, or HTTP listener off | User opens Obsidian, enables the HTTP listener |
| "timed out" (30 s) | Obsidian busy or huge search | Retry once, then narrow |
| 404 on a folder listing | missing OR empty folder | List the parent |
| 404 on a note/target | note or heading/block/field missing | document_map; or `create_target_if_missing` |
| 405 errorCode 40510 | path is a folder | `list_files_in_dir`; folders are not deletable |
| 400 errorCode 40021 | absolute or `..` path | Use a vault-relative path |
| 400 errorCode 40005 | frontmatter is invalid YAML | Fix it with `put_content`, retry the patch |
| 400 errorCode 40080/40081 | bad patch instruction | See PATCH.md: array heading target, one payload field |
| 400 errorCode 40083/40084 | stale plugin version (legacy headers hit 5.x) | Call `get_server_info`, retry once |
| 400 errorCode 40070/40012 | bad JsonLogic / content type (the plugin matches Content-Type exactly; the server always sends bare `text/markdown`, so 40012 on a write means a proxy rewrote the header) | Send a JSON object with documented operators; for a write, check the path to Obsidian |
| 400 errorCode 40090 | empty search query | Provide one |
| 409 (or 40920), or "already present in '<path>'" without a status | content already present (plugin check on a targeted write, or the server's read-first check on an untargeted append) | Treat as done |
| 412 | `if_match` stale | Re-read document_map, re-patch |
| 422 errorCode 42200 / 400 errorCode 40082 | server bug | Report; not user-fixable |
| 429 | rate limited (rare; another proxy) | Honour Retry-After, no tight loops |
| 500 errorCode 50010/50020 | Obsidian search/vault adapter threw | Retry once after a few seconds |
| 404 errorCode 40461 on /periodic/ ("has no note for ... yet") | period enabled, no note for that date yet | Create it with `put_content` / `append_content` (or `open_file`), then read again; `get_recent_periodic_notes` counts it as `skipped` |
| 404 errorCode 40460 on /periodic/ ("no <period> period configured") | Obsidian has no such period configured | Enable the plugin that provides it (core Daily Notes for daily, community Periodic Notes for the rest) or use another period |
| 404 errorCode 40400 or no envelope on /periodic/ ("route is not served") | companion plugin "Local REST API - Periodic Notes" missing | Install it, or read the note by path with `get_file_contents` |
| 400 errorCode 40060 on /periodic/ | period exists but is switched off | Enable it in Obsidian or use another period |
| 500 errorCode 50060 on /periodic/ | the companion plugin threw | Retry once after a few seconds; then Obsidian's developer console |
| "would be '<path>' but it does not exist" | note vanished between the redirect and the read | Create it at that path, or retry once |
| any other status/errorCode on /periodic/ | reported as upstream said it | Read the message; do not assume the companion is missing |
| "read-only" / "command execution is disabled" / "refusing ... without confirm=true" | a server gate, not upstream | Ask the operator / pass confirm after the user agrees |
| header `Deprecation: ... sunset-version="6.0"` reported in `notes` | plugin < 5 legacy patch engine | Suggest upgrading the plugin |

## Reading results

- `"isError": true` inside a `result` — the tool ran and failed; the text is
  written for you: surface it.
- JSON-RPC `error` `-32602` — the request itself was malformed (missing/ill-typed
  params object). Fix the call; it is not an outage.
- HTTP `403 Forbidden` before any JSON-RPC body — the `Host` header did not
  match `MCP_ALLOWED_HOSTS`. HTTP `413` — request body over the transport cap.
- Every success carries `structuredContent`; the text block is a readable
  rendering (the note text itself for markdown reads).
