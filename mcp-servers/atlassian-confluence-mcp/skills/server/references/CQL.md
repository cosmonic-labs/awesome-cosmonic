# CQL cheat sheet for `search`

Confluence Query Language, as accepted by `GET /wiki/rest/api/search` (the
endpoint the `search` tool uses). Served at
`skill://atlassian-confluence-mcp/references/CQL.md`.

## Shape

```
<field> <operator> <value> [AND|OR|NOT ...] [ORDER BY <field> [ASC|DESC]]
```

- Quote every value that is not a bare number or a function:
  `space = "ENG"`, `title ~ "Release notes"`. Escape an embedded quote or
  backslash with a backslash. The `text` parameter of the tool does this for
  you and builds `text ~ "<text>" AND type = page ORDER BY lastmodified DESC`.
- `~` is *contains* (full-text, with `*` wildcards and `"exact phrase"`);
  `!~` is *does not contain*; `=`/`!=`; `<`, `<=`, `>`, `>=` for dates;
  `in`/`not in` with a parenthesised list.
- Parentheses group; `AND` binds tighter than `OR`.
- `ORDER BY` must be last; only one `ORDER BY` clause.

## Fields that work on this endpoint

| Field | Values | Example |
|---|---|---|
| `type` | `page`, `blogpost`, `comment`, `attachment` | `type = page` |
| `space` | space key | `space = "ENG"`, `space in ("ENG","DOCS")` |
| `title` | text | `title ~ "Runbook*"`, `title = "Home"` |
| `text` | full text (body + title) | `text ~ "kubernetes upgrade"` |
| `label` | label name | `label = "runbook"`, `label in ("a","b")` |
| `ancestor` | numeric page id | `ancestor = 123456` (everything under that page) |
| `parent` | numeric page id | `parent = 123456` (direct children) |
| `id` | numeric content id | `id in (1,2,3)` |
| `creator`, `contributor`, `mention`, `watcher` | `currentUser()` or an account id | `creator = currentUser()` |
| `created`, `lastmodified` | date `"yyyy-MM-dd"`, `now("-7d")`, `startOfDay()`, `startOfWeek()`, `startOfMonth()` | `lastmodified > now("-30d")` |
| `container` | numeric space id | `container = 100` |
| `macro` | macro name | `macro = "jira"` |
| `favourite` | `currentUser()` | `favourite = currentUser()` |

**Not supported here** (400 `Could not parse cql`): `user`, `user.fullname`,
`user.accountid`, `user.email`, `group`, and the site-search-only fields.
Use `creator`/`contributor` with `currentUser()` or an account id instead.

## Sort keys

`ORDER BY created`, `lastmodified`, `title`, `type`, `space` (each
`ASC`/`DESC`). The tool's `text` shortcut sorts by `lastmodified DESC`.

## Recipes

```
text ~ "incident" AND type = page AND space = "OPS" ORDER BY lastmodified DESC
title ~ "Release*" AND type = page AND created > startOfMonth()
label = "adr" AND space in ("ENG","ARCH") ORDER BY created DESC
ancestor = 123456 AND lastmodified > now("-7d")
type = blogpost AND creator = currentUser() ORDER BY created DESC
type = attachment AND title ~ "*.pdf" AND space = "DOCS"
```

## Results

Each hit carries `id`, `type`, `status`, `title`, `spaceKey`, `url`,
`lastModified` and an `excerpt` with the highlight markers removed; the
result carries `totalSize` (all matches, not just this page) and
`next_cursor`. Pass the id to `get_page` (pages/blog posts), `get_comments`
(comments — the id is the comment's; its page is in the excerpt), or
`list_attachments` on the owning page.
