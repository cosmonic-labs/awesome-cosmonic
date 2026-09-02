# Tool reference

Supporting file of the `obsidian-mcp` skill, served at
`skill://obsidian-mcp/references/TOOLS.md`. Every call returns
`structuredContent` plus a readable text block; failures are `isError: true`
with an actionable message. "Gate" names the deployment setting that can
refuse the call before anything is sent.

| Tool | Arguments | Upstream | Returns | Gate |
|---|---|---|---|---|
| `check_auth` | none | `GET /` | `status` ok/missing/invalid/unreachable, identity (plugin + Obsidian version, base URL), `remediation` | — |
| `get_server_info` | none | `GET /` | service, versions, `authenticated`, `patch_format` (json-v2 / legacy-v1), read_only, commands_enabled, hints | — |
| `list_files_in_vault` | none | `GET /vault/` | `entries` (files as `x.md`, folders as `dir/`), `folders`, `files`, `count` | key |
| `list_files_in_dir` | `dirpath` | `GET /vault/{dir}/` | same shape for one folder; 404 = missing **or** empty | key |
| `get_file_contents` | `filepath`, `format?` (markdown/metadata/document_map), `heading?[]`, `block?`, `frontmatter_key?`, `scope?` | `GET /vault/{path}[/heading/..|/block/..|/frontmatter/..]` with Accept | `content` (markdown) or `data` (JSON), `truncated`, `content_type` | key |
| `batch_get_file_contents` | `filepaths[]` (1..200 entries; the first 20 distinct paths are read) | N x `GET /vault/{path}` | text: `# path` sections separated by `---`; structured per-file status, `clamped`, `not_attempted` (paths left out when the call budget ran out) | key |
| `simple_search` | `query` (1..1000 chars), `context_length?` (0..1000, default 100), `limit?` (1..100, default 20), `max_matches_per_file?` (1..50, default 10) | `POST /search/simple/?query=&contextLength=` | `results[{filename, score, total_matches, matches[{source,start,end,context}]}]`, `total_files`, `truncated` | key |
| `complex_search` | `query` (JsonLogic object, <= 64 KiB), `limit?` (1..500, default 100) | `POST /search/` (`application/vnd.olrapi.jsonlogic+json`) | `results[{filename, result}]` (values cut at 2000 chars), `total`, `truncated`, `hints` | key |
| `get_recent_changes` | `days?` (1..3650, default 90), `limit?` (1..100, default 10) | `POST /search/` JsonLogic on `stat.mtime` | `notes[{path, mtime_ms, mtime}]` newest first, `since` | key |
| `list_tags` | `limit?` (1..2000, default 200), `prefix?` | `GET /tags/` | `tags[{name, count}]`, `total`, `truncated` | key |
| `get_active_file` | `format?` (markdown/metadata) | `GET /active/` | `path` (from Content-Location), `content`/`data` | key |
| `append_content` | `filepath`, `content` (1 B..1 MiB), `heading?[]`, `reject_if_content_preexists?`, `allow_non_markdown?` | `POST /vault/{path}[/heading/..]` bare `text/markdown`; the reject flag is the `Reject-If-Content-Preexists` header on a heading target, and a `GET /vault/{path}` probe first without one (the plugin ignores the header on whole-note appends) | `status: appended`, `bytes`, `duplicate_check` (`plugin` / `note-read` / null), `updated_content` (targeted appends) | READ_ONLY |
| `put_content` | `filepath`, `content` (0..1 MiB), `require_existing?` | (`GET` probe) + `PUT /vault/{path}` | `status: written`, `bytes` | READ_ONLY |
| `patch_content` | `filepath`, `operation` (append/prepend/replace/delete), `target_type` (heading/block/frontmatter), `target` (array or string), `content?` xor `value?`, `scope?`, `within?`, `create_target_if_missing?`, `if_match?` | `PATCH /vault/{path}` JSON instruction (plugin >= 5) or legacy headers | `updated_content` (full note), `warnings` (decoded Markdown-Patch-Warnings), `patch_format`, `notes` | READ_ONLY |
| `delete_file` | `filepath`, `permanent?`, `confirm` (must be true) | `DELETE /vault/{path}?permanent=` | `deleted: true`, `permanent` | READ_ONLY + confirm |
| `get_periodic_note` | `period` (daily/weekly/monthly/quarterly/yearly), `date?` (YYYY-MM-DD), `format?` | `GET /periodic/{period}/[{y}/{m}/{d}/]` -> 307 -> `GET /vault/..` | `path` (resolved), `content`/`data` | key |
| `get_recent_periodic_notes` | `period`, `limit?` (1..10, default 5), `include_content?`, `tz_offset_minutes?` (-840..840, default 0) | undated `GET /periodic/{period}/` for the current period, then dated routes computed client-side | `notes[{date, resolved_by, path, content?}]`, `found`, `skipped`, `duplicates`, `attempted`, `budget_exhausted` | key |
| `open_file` | `filepath`, `new_leaf?` | `POST /open/{path}?newLeaf=` | `opened: true` (note created if missing) | READ_ONLY |
| `list_commands` | `filter?`, `limit?` (1..1000, default 100) | `GET /commands/` | `commands[{id, name}]`, `total`, `truncated` | ENABLE_COMMANDS |
| `execute_command` | `command_id` (1..200 chars) | `POST /commands/{id}/` | `executed: true` | ENABLE_COMMANDS + READ_ONLY |

"key" = needs `OBSIDIAN_API_KEY` (secret ref `obsidian-mcp-api-key`); a missing
key is one actionable error naming the variable, the ref, and where the key
comes from. READ_ONLY = `OBSIDIAN_READ_ONLY=true` refuses the call.
ENABLE_COMMANDS = refused unless `OBSIDIAN_ENABLE_COMMANDS=true`.

## Bounds the server enforces before calling Obsidian

| What | Bound |
|---|---|
| Path | <= 1024 chars, <= 64 segments, no leading `/`, no empty / `.` / `..` segments, no NUL |
| Heading path | 1..16 elements, each <= 512 chars |
| Note content (append/put/patch) | <= 1 MiB (append: >= 1 byte) |
| JsonLogic query | JSON object, <= 64 KiB serialized |
| Search query | 1..1000 chars |
| Batch | <= 200 entries accepted; the first 20 distinct paths are read |
| Multi-exchange calls (batch, recent periodic) | 2 x `MCP_OUTBOUND_TIMEOUT_MS` wall-clock (default 60 s), checked between exchanges; the rest is reported as not attempted |
| Returned note text | `OBSIDIAN_MAX_CONTENT_CHARS` (1 000..2 000 000, default 100 000) per note; batch text capped at twice that |
| Command id | 1..200 chars |
| Outbound exchange | `MCP_OUTBOUND_TIMEOUT_MS` (default 30 000) and `MCP_OUTBOUND_MAX_BYTES` (default 4 MiB), set by the deployment |

## Formats

- `format=metadata` (`application/vnd.olrapi.note+json`): `{path, content,
  frontmatter, tags, links, backlinks, unresolvedLinks, stat{ctime, mtime,
  size}}`; times are epoch milliseconds; tags carry no `#`.
- `format=document_map` (`application/vnd.olrapi.document-map+json`):
  `{headings: [...tree...], blocks: [...], frontmatterFields: [...],
  version}`; `version` feeds `patch_content.if_match`.
- Periodic dates are interpreted by the companion plugin in the user's local
  time. `get_recent_periodic_notes` lets the plugin resolve the current period
  (undated route) and computes the earlier dates from UTC + `tz_offset_minutes`;
  an overlap between the two is listed once (`duplicates`).
- A first-hop `/periodic/` error is classified by the plugin's `errorCode`,
  never by the status alone (three conditions share 404): 40461 = no note for
  that date yet (`get_recent_periodic_notes` skips it), 40460 = period not
  configured, 40400 or no envelope = route not served (companion plugin
  missing), 40060 = period switched off (400), 50060 = the companion threw
  (500); anything else is reported as upstream said it. A 404 after the
  redirect names the would-be path. See the error catalogue in SKILL.md.
