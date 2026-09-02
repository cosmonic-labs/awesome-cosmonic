# Markdown in notion-mcp

Two different Markdown paths exist; do not confuse them.

## 1. Notion-flavored Markdown (parsed by Notion)

Used by `get_page_content` (output), `update_page_markdown` and
`create_page` / `create_data_source_item` `body_markdown` (input). Notion
renders and parses its own dialect; the server passes it through untouched.
What Notion produces and accepts (developers.notion.com "Working with Markdown"):

| Construct | Notion-flavored Markdown |
|---|---|
| Headings | `#`, `##`, `###` (`heading_1..3`); a heading wrapped in `<details>` is a toggle heading |
| Lists | `-` bullets, `1.` numbered, `- [ ]` / `- [x]` to-dos; indentation nests |
| Code | fenced ```` ```lang ```` blocks; inline `` `code` `` |
| Quote | `> text` |
| Divider | `---` |
| Callout | `<callout icon="💡" color="gray_background">text</callout>` |
| Toggle | `<details><summary>Title</summary>body</details>` |
| Columns | `<columns><column>…</column><column>…</column></columns>` |
| Table | GitHub-style pipe tables (`\| a \| b \|` with a `---` row) |
| Child page / database | `<page url="https://www.notion.so/…">Title</page>`, `<database url="…">Title</database>` — links, not content |
| Mentions | `<mention-user url="…">Name</mention-user>`, `<mention-page url="…">Title</mention-page>`, `<mention-date start="2026-09-02"/>` |
| Media | `![caption](https://…)` images, `<file url="…"/>`, `<video url="…"/>`, `<audio url="…"/>`, `<embed url="…"/>`, `<bookmark url="…"/>` — file URLs returned by Notion are pre-signed and expire (~1 h) |
| Equations | `$inline$`, `$$block$$` |
| Inline styles | `**bold**`, `*italic*`, `~~strike~~`, `<u>underline</u>`, `<span color="red">…</span>`, `[text](https://…)` |
| Meeting notes | `<meeting-notes>` with a transcript placeholder unless `include_transcript` |
| Synced blocks | `<synced_block>` / `<synced_block_reference>` |
| Unknown blocks | `<unknown_block id="…"/>` — also listed in `unknown_block_ids` |

Editing rules for `update_page_markdown`:

- `mode: "update"` — each `old_str` must appear **exactly once** in Notion's
  rendering of the page (case-sensitive, whitespace-sensitive). Read the page
  with `get_page_content` first and copy the text verbatim. Set
  `replace_all: true` to change every occurrence; an `old_str` that matches 0
  or >1 times returns `400 validation_error` and the server appends
  "make it unique or set replace_all".
- `new_str: ""` deletes the matched text.
- `mode: "replace"` replaces the whole body. If the page contains child pages
  or databases Notion refuses unless `allow_deleting_content: true` — those
  children are then **trashed**, so re-link them with `<page url="…">` inside
  `new_markdown` if they must survive.
- Newlines inside JSON strings must be `\n`; the tool sends whatever string
  you give it.
- Large writes can take several seconds; the outbound deadline is
  `MCP_OUTBOUND_TIMEOUT_MS` (30 s). The server never requests async
  processing, so a 202 is reported as an error.

## 2. The flat subset converted locally by `append_blocks`

`append_blocks` builds block JSON itself so it can target a position (`end`,
`start`, `after_block_id`) or a non-page parent block. It understands only:

| Markdown line | Block |
|---|---|
| `# `, `## `, `### ` (deeper levels clamp to `###`) | `heading_1` / `heading_2` / `heading_3` |
| `- ` or `* ` | `bulleted_list_item` |
| `1. ` or `1) ` | `numbered_list_item` |
| `- [ ] ` / `- [x] ` (also `* [ ]`) | `to_do` |
| ```` ```lang ```` … ```` ``` ```` | `code` (`language` lower-cased; default `plain text`) |
| `> ` (consecutive lines join) | `quote` |
| `---`, `***`, `___` | `divider` |
| anything else (consecutive lines join with `\n`; blank line separates) | `paragraph` |

Inline formatting (`**bold**`, links) is kept as literal text. There is no
nesting: indented lines are treated as top-level. Each text run over 2000
characters is split into several `rich_text` items. More than 100 blocks in
one call is refused locally ("split into multiple calls"); the tool returns
`sent` (blocks built) and `appended` (blocks Notion created) so a mismatch is
visible.

Use it for "add these lines after block X" tasks; use `update_page_markdown`
for anything that needs callouts, toggles, tables, columns, mentions, or
nested lists.
