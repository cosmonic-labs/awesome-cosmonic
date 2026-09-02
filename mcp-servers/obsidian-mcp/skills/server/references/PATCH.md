# patch_content rules

Supporting file of the `obsidian-mcp` skill, served at
`skill://obsidian-mcp/references/PATCH.md`.

`patch_content` edits exactly one target in one note and returns the full
updated note. The wire format depends on the plugin version the server has
cached from `check_auth` / `get_server_info` (an instance that has not seen
either yet performs one `GET /` first):

| Plugin | Format (`patch_format`) | What is sent |
|---|---|---|
| >= 5.0 | `json-v2` | `PATCH /vault/{path}` with `Content-Type: application/vnd.olrapi.patch-instruction+json` and a JSON body |
| < 5.0 | `legacy-v1` | `PATCH /vault/{path}` with headers `Operation`, `Target-Type`, `Target` (percent-encoded, headings joined by `::`), `Create-Target-If-Missing`, and the payload as `text/markdown` or `application/json` |

The legacy engine is deprecated (`Deprecation: true; sunset-version="6.0"`,
reported in `notes`) and ignores `scope`, `within`, `if_match`; `delete` is
refused there. If the plugin was upgraded while the server was running, the
cache goes stale and the plugin answers 400 errorCode 40084/40083 — call
`get_server_info` and retry once.

## The JSON instruction (plugin >= 5)

```json
{
  "targetType": "heading" | "block" | "frontmatter",
  "target": ["Projects", "Q3"]      // heading: array top-down; block: "abc123"; frontmatter: "tags"
  "operation": "append" | "prepend" | "replace" | "delete",
  "scope": "content" | "marker" | "markerAndContent",   // optional, default content
  "content": "markdown text",       // OR
  "value": <any JSON>,              // frontmatter values, lists, table rows
  "within": 0,                      // optional sub-target selector
  "createTargetIfMissing": true,    // optional
  "ifMatch": "<document-map version>"  // optional optimistic concurrency
}
```

Rules the plugin enforces (400 errorCode 40080 PatchFailed / 40081
InvalidPatchInstruction otherwise):

- Heading `target` is an **array** from the top-level heading down, never
  `"A::B"` on the wire (the tool splits a `::` string for you) and never the
  `#` characters.
- Exactly one of `content` / `value`, except `operation: delete`, which takes
  neither. `value` is for frontmatter fields (typed values, lists, objects) and
  for table rows at a block target; `content` is markdown.
- `content` scope on a heading covers the text **under** the heading only: do
  not put the heading line in `content` or it appears twice. Heading levels
  inside `content` are relative and are rebased under the target for you.
- `scope: marker` on a heading renames it: `content` is the bare new text.
- Frontmatter: `append` on a list field merges items in; `replace` sets the
  field; `delete` removes it. There is no remove-one-item op — read the field
  (`get_file_contents frontmatter_key=<key>` or `format=metadata`), then
  `replace` with the full new list.
- Block targets address the paragraph carrying `^id`; `append` adds after it,
  `prepend` before, `replace` swaps it.
- Duplicate headings: only the first is addressable by text; take the exact
  key from `get_file_contents format=document_map`.
- `createTargetIfMissing: true` creates a heading/field instead of 404;
  otherwise a missing target is 404.

## Responses

- 200 with the whole updated note in `updated_content` (clamped to
  `OBSIDIAN_MAX_CONTENT_CHARS`, `truncated: true` when cut).
- `warnings`: the decoded `Markdown-Patch-Warnings` header (a JSON array such
  as `[{"code": "heading-depth-overflow", "message": "..."}]`), empty when the
  plugin had nothing to say.
- 404: note or target missing. 409: the plugin's duplicate guard on a
  targeted write (content already present) or the destination exists.
  412: `ifMatch` stale — re-read the document
  map. 400 errorCode 40005: the note's frontmatter is not valid YAML — fix it
  with `put_content` first. 405 errorCode 40510: the path is a folder.
