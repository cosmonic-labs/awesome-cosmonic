---
name: notion-mcp
description: Use when a task needs to find, read, create, edit, query or comment on Notion pages, databases (data sources), blocks or users through the Notion REST API with an internal-integration token — including deciding which notion-mcp tool to call first, how to sequence them, and what a Notion error actually means.
---

# Operating the notion-mcp server

A sandboxed WebAssembly component that speaks the Notion REST API
(`api.notion.com/v1`) with an **internal integration token**. It is stateless:
every call is self-contained; nothing carries over between calls except a
60-second per-instance cache of data source schemas.

Full argument tables: [references/TOOLS.md](references/TOOLS.md). Endpoint and
version facts: [references/ENDPOINTS.md](references/ENDPOINTS.md). The Markdown
dialect: [references/MARKDOWN.md](references/MARKDOWN.md). Property value
shapes: [references/PROPERTIES.md](references/PROPERTIES.md).

## Start here

1. **`check_auth` first.** `status: ok` gives the bot name and workspace.
   `missing` means the `notion-mcp-token` secret (env `NOTION_TOKEN`) is not
   registered; `invalid` means Notion rejected it (401); `insufficient` means
   a capability is unchecked (403). None of those is retryable — relay the
   `remediation` text and stop.
2. **Find content** with `search` (title match only, eventually consistent —
   a page created seconds ago may not show yet) or take a `notion.so` URL
   straight from the user: every id argument accepts a 32-hex id, a
   hyphenated UUID, or the URL (the id is the trailing 32 hex characters of the
   path; `?v=` on database URLs is a **view** id, never pass that).
3. **Read** properties with `get_page` (flattened values; `raw: true` for
   Notion's objects) and the body with `get_page_content` (Notion-flavored
   Markdown in one call — no block walking). `get_block_children` only when
   you need block ids (to comment on, or insert after, a specific block).
4. **Tables**: a *database* is a container; its `data_sources[]` hold the
   rows. `get_database` (database id/URL) → pick a data source id →
   `get_data_source` (schema: names, types, options) → `query_data_source`
   (Notion filter/sorts JSON passthrough) or `create_data_source_item` (plain
   values coerced through the schema).
5. **Write**: `create_page` (page parent or data source parent, optional
   Markdown body parsed by Notion), `update_page` (properties, trash, icon),
   `update_page_markdown` (find-and-replace or whole-body replace),
   `append_blocks` (flat Markdown subset → blocks, positioned), `create_comment`.

## Sequencing rules that save calls

- Never `query_data_source` with a database id: it 404s (`object_not_found`),
  not a type error. Resolve via `get_database` first.
- `search` has no `"database"` filter: `object` is `"page"` or
  `"data_source"`; data-source rows carry `database_id` for the container.
- Read `get_data_source` **before** writing properties with raw objects; the
  title property has a per-database name (`Name`, `Task`, ...). Prefer
  `create_data_source_item` with plain values — it resolves names
  (case-insensitively) and types for you and rejects what it cannot coerce
  (`files`, `rollup`, `formula`, `verification`) by type name.
- Whole-page edits: `update_page_markdown`. `mode: "update"` needs each
  `old_str` to match **exactly once** (case-sensitive, against Notion's own
  Markdown rendering — read it with `get_page_content` first) or
  `replace_all: true`; `mode: "replace"` refuses to delete child pages or
  databases unless `allow_deleting_content: true`. Positional inserts under a
  non-page block: `append_blocks` (max 100 blocks/call, flat, 2000-char runs
  split automatically).
- Paginate with `next_cursor` and `page_size <= 100` rather than hammering;
  the average budget is ~3 requests/second per integration.
- File and image URLs inside Markdown/blocks are pre-signed and expire within
  about an hour; re-read before reusing one.
- `list_comments` on a whole page uses the **page id** as `block_id`; the API
  cannot anchor comments to text ranges, only to blocks or existing
  `discussion_id`s.
- `list_users` never includes guests, and emails appear only if the
  integration's User information capability includes them.

## Error catalogue

Every failure is a tool error (`isError: true`) whose text says what happened
and what to do. Match on these:

| Text starts with / contains | Meaning | Do |
|---|---|---|
| `NOTION_TOKEN is not set` | Secret ref `notion-mcp-token` not registered / not in `secretFrom` | Stop; relay the registration steps. Never retry. |
| `Notion API error 401 unauthorized` | Token wrong, revoked, other workspace, pasted with junk | Stop; user re-copies the Internal Integration Secret and re-registers. |
| `Notion API error 403 restricted_resource` | Capability unchecked on the integration (the text names which) or plan limit | Stop; user ticks the capability on the Capabilities tab. |
| `Notion API error 404 object_not_found` | Not **shared** with the integration (most likely), wrong object kind for the endpoint, view id from `?v=`, or trashed | Ask the user to share it (… → Connections), or fix the id kind per the hint. Never retry unchanged. |
| `Notion API error 400 validation_error` | Payload wrong: property name/type, filter shape, >100 children, >2000-char run, `old_str` matched 0 or >1 times | Fix the payload (the hint says how); the token is fine. |
| `Notion API error 400 missing_version` / `invalid_request` | `NOTION_VERSION` misconfigured or endpoint unsupported at that version | Deployment config problem; report it. |
| `Notion API error 409 conflict_error` | Concurrent edit collision | Retry the same call once after ~1 s; then re-read the page. |
| `Notion API error 429 rate_limited` … `retry_after_seconds=N` | Over ~3 rps or the workspace budget | Wait N seconds, then continue; paginate instead of looping. The server never sleeps or retries for you. |
| `Notion API error 500/502/503/504/529` | Transient Notion-side failure or >60 s processing | Retry with exponential backoff; split very large Markdown writes. |
| `Notion request failed before a response arrived` | Host not on `allowedHosts`, DNS/TLS, or the outbound deadline | Deployment problem (add `api.notion.com` / the `NOTION_BASE_URL` host to `allowedHosts`); a timeout may be retried once. |
| `notion-mcp is deployed read-only` | `NOTION_READ_ONLY=true` | Policy; do not retry. Use a write-enabled deployment. |
| `… converts to N blocks` / `longer than …` / `at most 100` / `exactly one of …` / `Request not sent (payload_too_large)` | Local pre-check caught a limit before any upstream call (the last one: the serialized request exceeds Notion's 500 KB cap) | Split the input or fix the arguments. |
| Result has `truncated: true` or non-empty `unknown_block_ids` | Page exceeds Notion's ~20k-block render cap, or some blocks are unreadable | Do not treat the Markdown as the whole page; `get_block_children` on the listed ids or ask the user to share nested pages. |
| Result has `request_status.incomplete_reason: query_result_limit_reached` | Upstream capped the result set (10,000 rows / search index cap) | Narrow the filter; the cursor still walks what was returned. |

`page_size` out of range is clamped silently (the result reports the value
used). JSON-RPC `-32602` means the request itself was malformed (missing or
ill-typed arguments) — fix the call, not the deployment.

## Version note

Field names depend on `NOTION_VERSION`: `2026-03-11` (default) uses `in_trash`
and `position`; `2025-09-03` uses `archived` and `after`. The server maps
`update_page.in_trash` and `append_blocks.position` for you, and the Markdown
tools (`get_page_content`, `update_page_markdown`, `create_page` with a body)
always send `2026-03-11` because those endpoints exist only there.
