# Notion endpoints and versions used by notion-mcp

Vendored facts (checked against developers.notion.com on 2026-09-02) so the
server's behaviour can be reasoned about without the upstream docs.

## Headers on every request

```
Authorization: Bearer <NOTION_TOKEN>     # internal integration secret (ntn_... / secret_...)
Notion-Version: 2026-03-11               # NOTION_VERSION; 2025-09-03 also accepted
Content-Type: application/json           # on POST / PATCH
```

## Versions

| Version | What it changed | How the server handles it |
|---|---|---|
| `2026-03-11` (current, default) | `archived` → `in_trash` everywhere; `after` → `position` object (`{type: end\|start\|after_block, after_block:{id}}`); `transcription` block → `meeting_notes`; Markdown endpoints (`GET`/`PATCH /v1/pages/{id}/markdown`) and `markdown` on `POST /v1/pages` | Native |
| `2025-09-03` | Databases split into container + `data_sources[]`; queries moved to `POST /v1/data_sources/{id}/query`; search filter values `page` / `data_source`; parent `data_source_id` | Accepted; `update_page.in_trash` is sent as `archived`, `append_blocks` uses `after`, `position: start` is refused, Markdown tools force `2026-03-11` |
| older (`2022-06-28`, …) | No data sources | Rejected with a config error |

## Endpoints

| Tool | Method + path | Notes |
|---|---|---|
| `check_auth`, `get_self` | `GET /v1/users/me` | Bot user: `{id, name, type: "bot", bot:{owner, workspace_name, workspace_id, workspace_limits}}` |
| `search` | `POST /v1/search` | Body `{query?, filter:{property:"object", value:"page"\|"data_source"}?, sort:{timestamp:"last_edited_time", direction}?, start_cursor?, page_size}`; response has `request_status{type: complete\|incomplete, incomplete_reason}` |
| `get_page` | `GET /v1/pages/{id}?filter_properties=…` | Each relation/rollup property returns at most 25 references (`has_more`) |
| `get_page_content` | `GET /v1/pages/{id}/markdown?include_transcript=` | `{object:"page_markdown", id, markdown, truncated, unknown_block_ids[≤100]}`; `truncated` at ~20k blocks; unknown ids can be re-requested as `page_id` |
| `get_block_children` | `GET /v1/blocks/{id}/children?page_size=&start_cursor=` | First level only; `has_children` tells you to descend |
| `create_page` | `POST /v1/pages` | `{parent:{page_id}\|{data_source_id}, properties, markdown?, icon?, children?}`; page parents accept only `title`; `markdown` needs 2026-03-11 |
| `create_data_source_item` | `GET /v1/data_sources/{id}` then `POST /v1/pages` | Schema cached 60 s per instance |
| `update_page` | `PATCH /v1/pages/{id}` | `{properties?, in_trash?\|archived?, icon?}`; rollups cannot be written; cannot move a page |
| `update_page_markdown` | `PATCH /v1/pages/{id}/markdown` | `{type:"update_content", update_content:{content_updates:[{old_str,new_str,replace_all_matches}], allow_deleting_content}}` or `{type:"replace_content", replace_content:{new_str, allow_deleting_content}}`; `allow_async` is never sent, so a 202 is treated as an error |
| `append_blocks` | `PATCH /v1/blocks/{id}/children` | `{children[≤100], position?}`; `after` and `position` are mutually exclusive upstream |
| `get_database` | `GET /v1/databases/{id}` | `{data_sources:[{id,name}], is_inline, in_trash, parent}` |
| `get_data_source` | `GET /v1/data_sources/{id}` | Relation properties pointing at unshared databases are hidden |
| `query_data_source` | `POST /v1/data_sources/{id}/query?filter_properties=…` | `{filter?, sorts?, start_cursor?, page_size, is_archived?}`; hard cap 10,000 results per query |
| `list_comments` | `GET /v1/comments?block_id=&page_size=&start_cursor=` | Only unresolved threads |
| `create_comment` | `POST /v1/comments` | `{parent:{page_id}\|{block_id}}` xor `{discussion_id}`, `rich_text[≤100 runs]` |
| `list_users` | `GET /v1/users?page_size=&start_cursor=` | No guests; order not guaranteed |

## Limits (developers.notion.com/reference/request-limits)

- ~3 requests/second average per integration, bursts allowed; a separate
  workspace-wide budget shared by every integration in the workspace.
- `429 rate_limited` carries `Retry-After` (whole seconds) and
  `additional_data.rate_limit_reason`; `529 service_overload` also honours
  `Retry-After`.
- 100 elements per array (block children, rich-text runs, multi-select,
  relation, people); 2000 characters per rich-text run and per URL;
  1000 characters per equation; 200 for email/phone; 500 KB per payload;
  1000 block elements per payload; two levels of block nesting per request.

## Status codes

| Status | `code` | Meaning |
|---|---|---|
| 400 | `invalid_json`, `invalid_request_url`, `invalid_request`, `validation_error`, `missing_version`, `invalid_beta` | Request malformed / unsupported at this version |
| 401 | `unauthorized` | "API token is invalid." |
| 403 | `restricted_resource` | Capability missing or workspace limit |
| 404 | `object_not_found` | "Could not find … Make sure the relevant pages and databases are shared with your integration." |
| 409 | `conflict_error` | "Conflict occurred while saving. Please try again." |
| 429 | `rate_limited` | see limits |
| 500 | `internal_server_error` | transient |
| 502 | `bad_gateway` | transient |
| 503 | `service_unavailable`, `database_connection_unavailable` | transient, or the request exceeded 60 s |
| 504 | `gateway_timeout` | transient |
| 529 | `service_overload` | transient, honour `Retry-After` |

Error body: `{"object":"error","status":404,"code":"object_not_found","message":"…","request_id":"…"}`.
