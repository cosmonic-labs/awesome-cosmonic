# Tool reference

Supporting file of the `atlassian-confluence-mcp` skill, served at
`skill://atlassian-confluence-mcp/references/TOOLS.md`. The JSON schemas on the
wire (`tools/list`) are authoritative for argument names and types; this file
adds limits, defaults, upstream endpoints and result shapes.

Common conventions:

- `page_id` accepts a numeric id **or** a page URL (`.../pages/<id>/Title`,
  `.../pages/edit-v2/<id>`, `viewpage.action?pageId=<id>`).
- `space_id` accepts a numeric id **or** a space key; keys are resolved with
  one extra call to `GET /wiki/api/v2/spaces?keys=`.
- `limit` is clamped to the stated range (negative → refused); `cursor` is the
  previous result's `next_cursor`, passed verbatim (≤ 4096 chars).
- List results: `{count, limit, results: [...], next_cursor}` plus a few
  echo fields (`pageId`, `spaceId`, `cql`, `totalSize`, …).
- Errors: `isError: true`, readable text, and `structuredContent.error =
  {kind, status?, messages?, codes?, retryable, retry_after_seconds?, hint}`.

| Tool | Arguments | Upstream | Returns | Gated |
|---|---|---|---|---|
| `check_auth` | none | `GET /wiki/rest/api/user/current` | `{status: ok\|missing\|invalid\|insufficient\|error, account, site, base_url, route, email, read_only, allow_delete, spaces_filter, credential, remediation}` | no |
| `get_current_user` | none | `GET /wiki/rest/api/user/current` | `{accountId, accountType, email, displayName, publicName, timeZone, site, route}` | no |
| `search` | `cql?` or `text?`, `space_key?`, `limit` 1..100 (25), `cursor?` | `GET /wiki/rest/api/search?cql=&limit=&cursor=&excerpt=highlight` | results `{id, type, status, title, spaceKey, url, lastModified, excerpt, entityType}`; `cql` (as echoed), `totalSize` | no |
| `list_pages` | `space_id?` (id or key), `title?` (exact), `status?` (current), `sort?`, `limit` 1..250 (25), `cursor?` | `GET /wiki/api/v2/pages?space-id=&title=&status=&sort=&limit=&cursor=` | results = page metadata (below); `spaceId`, `spaceKey` | no |
| `get_page` | `page_id`, `format?` text\|storage\|atlas_doc_format\|view (text), `max_chars` 1000..200000 (60000), `version?` | `GET /wiki/api/v2/pages/{id}?body-format=&include-labels=true&include-version=true[&version=]` | page metadata + `labels`, `format`, `body`, `body_chars`, `body_truncated` (+ `adf` for ADF) | no |
| `get_page_children` | `page_id`, `sort?`, `limit` 1..250 (25), `cursor?` | `GET /wiki/api/v2/pages/{id}/children` | results = page metadata + `childPosition`; `parentId` | no |
| `list_spaces` | `keys?` (≤ 50), `type?`, `status?` (current), `limit` 1..250 (25), `cursor?` | `GET /wiki/api/v2/spaces?keys=&type=&status=&limit=&cursor=&description-format=plain` | results `{id, key, name, type, status, homepageId, description, url}`; `keys` | no |
| `get_space` | `space` (id or key) | `GET /wiki/api/v2/spaces/{id}` or `?keys=` | one space (shape above) | no |
| `create_page` | `space_id`, `title` 1..255, `body`, `body_format?` markdown\|storage\|wiki (markdown), `parent_id?` | `POST /wiki/api/v2/pages` `{spaceId, status: current, title, parentId?, body: {representation, value}}` | page metadata + `created: true`, `spaceKey`, `representation` | `CONFLUENCE_READ_ONLY` |
| `update_page` | `page_id`, `title?`, `body?`, `body_format?`, `version_message?` ≤ 255, `minor_edit?`, `expected_version?` | `GET /wiki/api/v2/pages/{id}?body-format=storage` then `PUT /wiki/api/v2/pages/{id}` `{id, status, title, body, version: {number: N+1, message?, minorEdit}}` | page metadata + `updated: true`, `previous_version`, `representation` | `CONFLUENCE_READ_ONLY` |
| `delete_page` | `page_id`, `confirm` (must be `true`) | `DELETE /wiki/api/v2/pages/{id}` (trash only) | `{deleted: true, id, status: 204, note}` | `CONFLUENCE_READ_ONLY`, `CONFLUENCE_ALLOW_DELETE`, `confirm` |
| `get_comments` | `page_id`, `kind?` footer\|inline (footer), `sort?`, `limit` 1..250 (25), `cursor?` | `GET /wiki/api/v2/pages/{id}/footer-comments` or `/inline-comments` `?body-format=storage` | results `{id, status, title, pageId, parentCommentId, version, body (text ≤ 4000 chars), url, resolutionStatus?, inlineOriginalSelection?}`; `pageId`, `kind` | no |
| `add_comment` | `page_id?` or `reply_to_comment_id?`, `body` 1..100000, `body_format?` | `POST /wiki/api/v2/footer-comments` `{pageId | parentCommentId, body}` | comment (shape above) + `created: true` | `CONFLUENCE_READ_ONLY` |
| `get_labels` | `page_id`, `prefix?` global\|my\|team\|system, `limit` 1..250 (25), `cursor?` | `GET /wiki/api/v2/pages/{id}/labels` | results `{id, name, prefix}`; `pageId` | no |
| `add_label` | `page_id`, `labels` 1..20 names | `POST /wiki/rest/api/content/{id}/label` `[{prefix: global, name}]` | `{pageId, added, count, labels: [{id, name, prefix}]}` | `CONFLUENCE_READ_ONLY` |
| `list_attachments` | `page_id`, `media_type?`, `filename?`, `limit` 1..250 (50), `cursor?` | `GET /wiki/api/v2/pages/{id}/attachments?mediaType=&filename=` | results `{id, title, status, pageId, mediaType, mediaTypeDescription, fileSize, comment, createdAt, version, url, download_url}`; `pageId` | no |

**Page metadata** (from `list_pages`, `get_page`, `get_page_children`,
`create_page`, `update_page`): `{id, title, status, spaceId, parentId,
parentType, authorId, createdAt, version: {number, createdAt, authorId,
message, minorEdit}, url, edit_url?, labels?}`. `url` is absolute
(`https://<site>/wiki` + the `webui` link).

## Sort values

- `list_pages`: `id`, `-id`, `title`, `-title`, `created-date`,
  `-created-date`, `modified-date`, `-modified-date`.
- `get_page_children`: the above plus `child-position`, `-child-position`.
- `get_comments`: `created-date`, `-created-date`, `modified-date`,
  `-modified-date`.

## Body formats

| `body_format` | What is sent | When to use |
|---|---|---|
| `markdown` (default) | Converted to storage XHTML: `#` headings, lists, `- [ ]` task lists (as `ac:task-list`), tables, links (http/https/mailto only), images (`![alt](https://…)` → `ri:url`, `![alt](file.png)` → `ri:attachment`), fenced code → `<ac:structured-macro ac:name="code">` with `language` and CDATA; raw HTML escaped as text | Almost always |
| `storage` | Verbatim; must be well-formed XML | Round-tripping `get_page(format=storage)` output |
| `wiki` | Verbatim Confluence wiki markup (`h1.`, `{code}`, `||header||`) | Legacy content |

Size cap: 4 MiB before and after conversion (Confluence's own limit is 5 MB).
Characters XML 1.0 forbids (`U+0000..U+0008`, `U+000B`, `U+000C`,
`U+000E..U+001F`, `U+007F`, `U+FFFE`, `U+FFFF`) are dropped from every
format, and from titles and `version_message`, before the request is built.

## `get_page` formats

| `format` | Request | `body` |
|---|---|---|
| `text` | `body-format=storage` | Storage rendered to text: `#` headings, `-`/`1.` lists, pipe tables, fenced code from code macros, `[text](href)` links, `![file]` images, `[macro:name]` markers, `- [x]` tasks |
| `storage` | `body-format=storage` | Raw XHTML |
| `atlas_doc_format` | `body-format=atlas_doc_format` | ADF rendered to the same text dialect; the parsed ADF is in `adf` (omitted when larger than 4×`max_chars`) |
| `view` | `body-format=view` | Rendered HTML as Confluence serves it |

## Environment (deployment)

| Variable | Kind | Default | Effect |
|---|---|---|---|
| `ATLASSIAN_SITE` | named config, required | — | `acme` or `acme.atlassian.net` |
| `ATLASSIAN_EMAIL` | named config, required | — | Basic-auth user |
| `ATLASSIAN_API_TOKEN` | secret ref `atlassian-api-token`, required | — | Basic-auth password |
| `ATLASSIAN_CLOUD_ID` | named config | unset | Scoped-token route via `https://api.atlassian.com/ex/confluence/<id>` |
| `ATLASSIAN_BASE_URL` | test override | `https://<site>` | Replaces the origin (e2e fixture) |
| `CONFLUENCE_READ_ONLY` | named config | `false` | Refuses every write tool |
| `CONFLUENCE_ALLOW_DELETE` | named config | `false` | Enables `delete_page` |
| `CONFLUENCE_SPACES_FILTER` | named config | unset | Comma-separated space keys to scope to |
