# Tool reference

Supporting file of the `notion-mcp` skill (`skill://notion-mcp/references/TOOLS.md`).
Every id argument accepts a 32-hex id, a hyphenated UUID, or a `notion.so`
URL. Page sizes are clamped to 1..100. "Gated" tools return an error under
`NOTION_READ_ONLY=true` without calling Notion.

| Tool | Arguments | Output (`structuredContent`) | Upstream | Gated |
|---|---|---|---|---|
| `check_auth` | none | `{status: ok\|missing\|invalid\|insufficient\|error, identity{id,name,workspace_name,workspace_id,owner_type}, api_version, read_only, ref, env, obtainUrl, remediation}` — non-ok results are `isError: true` with the same fields | `GET /v1/users/me` | no |
| `get_self` | none | `{id, name, type, workspace_name, workspace_id, owner_type, workspace_limits}` | `GET /v1/users/me` | no |
| `search` | `query?`, `object?` (`page`\|`data_source`), `sort_direction?` (`ascending`\|`descending` by last_edited_time), `page_size?` (default 20), `start_cursor?` | `{count, page_size, results[{object,id,title,url,parent{type,id},last_edited_time,database_id?}], has_more, next_cursor, request_status?}` | `POST /v1/search` | no |
| `get_page` | `page_id`, `filter_properties?` (≤100 property ids), `raw?` | `{id, url, title, parent, in_trash, created_time, last_edited_time, icon, properties}` (flattened unless raw) | `GET /v1/pages/{id}` | no |
| `get_page_content` | `page_id`, `include_transcript?`, `max_chars?` (1000..500000, default 60000) | text = the Markdown; `{id, markdown, chars_returned, local_truncated, truncated, unknown_block_ids, note}` | `GET /v1/pages/{id}/markdown` (version 2026-03-11 forced) | no |
| `get_block_children` | `block_id`, `page_size?` (default 50), `start_cursor?`, `raw?` | `{block_id, count, page_size, blocks[{id,type,has_children,text?,checked?,language?,title?,url?}], has_more, next_cursor}` | `GET /v1/blocks/{id}/children` | no |
| `create_page` | exactly one of `parent_page_id` / `parent_data_source_id`; `title?`, `properties?` (raw Notion objects), `body_markdown?`, `icon_emoji?` | `{id, url, title, parent}` | (`GET /v1/data_sources/{id}` for the title name) + `POST /v1/pages` (2026-03-11 when a body is sent) | yes |
| `create_data_source_item` | `data_source_id`, `title?`, `properties?` (plain values by name), `body_markdown?` | `{id, url, title, properties}` (flattened) | `GET /v1/data_sources/{id}` (cached 60 s) + `POST /v1/pages` | yes |
| `update_page` | `page_id`, `properties?`, `in_trash?`, `icon_emoji?` (≥1 required) | `{id, url, title, in_trash, last_edited_time, properties}` | `PATCH /v1/pages/{id}` (`archived` under 2025-09-03) | yes |
| `update_page_markdown` | `page_id`, `mode` (`update`\|`replace`), `updates?` (1..50 `{old_str,new_str,replace_all?}`), `new_markdown?`, `allow_deleting_content?`, `max_chars?` | same shape as `get_page_content` (the resulting Markdown) | `PATCH /v1/pages/{id}/markdown` (2026-03-11 forced) | yes |
| `append_blocks` | `block_id`, `markdown` (flat subset, ≤100 blocks), `position?` (`end`\|`start`), `after_block_id?` | `{parent_id, sent, appended, blocks[{id,type}]}` | `PATCH /v1/blocks/{id}/children` (`after` under 2025-09-03) | yes |
| `get_database` | `database_id` | `{id, title, url, is_inline, in_trash, parent, data_sources[{id,name}]}` | `GET /v1/databases/{id}` | no |
| `get_data_source` | `data_source_id`, `raw?` | `{id, title, url, database_id, parent, in_trash, title_property, properties[{name,id,type,options?,groups?,data_source_id?,expression?,config?}]}` | `GET /v1/data_sources/{id}` | no |
| `query_data_source` | `data_source_id`, `filter?` (object), `sorts?` (objects), `page_size?` (default 25), `start_cursor?`, `include_trashed?`, `filter_properties?`, `raw?` | `{data_source_id, count, page_size, rows[{id,url,title,in_trash,last_edited_time,properties}], has_more, next_cursor, request_status?}` | `POST /v1/data_sources/{id}/query` | no |
| `list_comments` | `block_id` (page id for page-level threads), `page_size?` (default 50), `start_cursor?` | `{block_id, count, page_size, comments[{id,discussion_id,parent,created_time,created_by,display_name,text,attachments}], has_more, next_cursor}` | `GET /v1/comments` | no |
| `create_comment` | `text` (1..200000 chars), exactly one of `page_id` / `block_id` / `discussion_id` | `{id, discussion_id, parent, created_time}` | `POST /v1/comments` | yes |
| `list_users` | `page_size?` (default 50), `start_cursor?` | `{count, page_size, users[{id,type,name,email?,workspace_name?}], has_more, next_cursor}` | `GET /v1/users` | no |

## Local limits (checked before any upstream call)

| Check | Limit |
|---|---|
| Ids | ≤ 2048 characters; must reduce to 32 hex characters |
| `search.query` | ≤ 2000 characters |
| `filter_properties` | ≤ 100 entries |
| `append_blocks.markdown` | ≤ 200,000 characters and ≤ 100 resulting blocks; text runs split at 2000 characters |
| `body_markdown`, `new_markdown`, `new_str` | ≤ 400,000 characters; whole request ≤ 500 KB |
| `update_page_markdown.updates` | 1..50 entries, non-empty `old_str` |
| `create_comment.text` | 1..200,000 characters (≤ 100 runs of 2000) |
| `icon_emoji` | 1..16 characters |
| `multi_select` / `relation` / `people` values | ≤ 100 items |
| Markdown returned | `max_chars` clamped to 1000..500,000; cut at a line boundary |

## Capability each tool needs on the integration

Read content: `search`, `get_page`, `get_page_content`, `get_block_children`,
`get_database`, `get_data_source`, `query_data_source`, `get_self`, `check_auth`.
Insert content: `create_page`, `create_data_source_item`, `append_blocks`.
Update content: `update_page`, `update_page_markdown`.
Read comments: `list_comments`. Insert comments: `create_comment`.
User information: `list_users` (personal access tokens cannot list users).
