# Property values: reading, writing, coercing

## How `get_page` / `query_data_source` flatten values (`raw: false`)

| Property type | Flattened to |
|---|---|
| `title`, `rich_text` | plain string (all runs concatenated) |
| `select`, `status` | option name or `null` |
| `multi_select` | `[names]` |
| `date` | `{start, end, time_zone}` or `null` |
| `people` | `[{id, name}]` |
| `relation` | `[page ids]`; `{ids: […], has_more: true}` when Notion capped it at 25 |
| `number`, `checkbox`, `url`, `email`, `phone_number`, `created_time`, `last_edited_time` | the scalar |
| `formula` | the computed value (string/number/boolean/date) |
| `rollup` | number/date, or an array of flattened values |
| `created_by`, `last_edited_by` | `{id, name}` |
| `files` | `[{name, url}]` (URLs expire) |
| `unique_id` | `"PREFIX-42"` (or the number without a prefix) |
| `verification` | the state string |
| anything else (`button`, `place`, …) | Notion's inner object unchanged |

`raw: true` returns Notion's property objects verbatim — use it when you need
option ids/colors, rich-text annotations, or the exact shape to echo back in
`update_page.properties`.

## What `create_data_source_item.properties` accepts (plain values)

Names are matched exactly, then case-insensitively (an ambiguous match is an
error naming the candidates; a miss lists the available names). A value that
is already a Notion property object (`{"select": {"name": …}}`) passes through.

| Schema type | Plain value | Sent as |
|---|---|---|
| `title`, `rich_text` | string (numbers/booleans are stringified) | `{title\|rich_text: [runs of ≤2000 chars]}` |
| `number` | number, or a numeric string | `{number: n}` |
| `checkbox` | `true`/`false` (or `"true"`/`"false"`) | `{checkbox: b}` |
| `select`, `status` | option name string; `null` clears | `{select: {name}}` — the option must exist for `status`; new `select` options are created by Notion |
| `multi_select` | `[names]` or a single string (≤100) | `{multi_select: [{name}]}` |
| `date` | ISO-8601 string (`2026-09-02` or `2026-09-02T09:00:00+02:00`) or `{start, end?, time_zone?}`; `null` clears | `{date: {…}}` |
| `url`, `email`, `phone_number` | string; `null` clears | `{url: s}` etc. |
| `relation` | `[page ids or URLs]` or one (≤100); ids are normalized | `{relation: [{id}]}` |
| `people` | `[user ids]` from `list_users` (≤100) | `{people: [{object:"user", id}]}` |
| `files`, `rollup`, `formula`, `verification`, `button`, `place`, `unique_id`, `created_*`, `last_edited_*` | rejected by type name | use `create_page` with raw `properties` for `files`; the rest are read-only |

## Writing raw objects (`create_page.properties`, `update_page.properties`)

Mirror Notion's property-value objects exactly, keyed by the property **name**
from `get_data_source` (or its id):

```json
{
  "Task":   {"title": [{"type": "text", "text": {"content": "Write docs"}}]},
  "Status": {"select": {"name": "In progress"}},
  "Due":    {"date": {"start": "2026-09-30"}},
  "Points": {"number": 3},
  "Done":   {"checkbox": false},
  "Tags":   {"multi_select": [{"name": "alpha"}, {"name": "beta"}]},
  "Owner":  {"people": [{"object": "user", "id": "01234567-89ab-cdef-0123-456789abcdef"}]},
  "Project":{"relation": [{"id": "66666666-7777-8888-9999-aaaaaaaaaaaa"}]},
  "Link":   {"url": "https://example.com"}
}
```

A page whose parent is another page has exactly one writable property,
`title`. Typical `400 validation_error` messages: "X is not a property that
exists" (wrong name), "X is expected to be select" (wrong type), "body.properties
should be defined" (missing) — all mean the payload is wrong, not the token.

## Filters and sorts for `query_data_source`

Passed through unchanged. Property filters are keyed by the property's
**type**, timestamp filters by `timestamp`:

```json
{"and": [
  {"property": "Status", "select": {"equals": "Done"}},
  {"property": "Due", "date": {"on_or_before": "2026-12-31"}},
  {"property": "Tags", "multi_select": {"contains": "alpha"}},
  {"property": "Points", "number": {"greater_than": 2}},
  {"property": "Task", "title": {"contains": "docs"}},
  {"timestamp": "last_edited_time", "last_edited_time": {"past_week": {}}}
]}
```

Sorts: `[{"property": "Due", "direction": "ascending"},
{"timestamp": "created_time", "direction": "descending"}]` — earlier entries
take precedence. `include_trashed: true` returns trashed rows instead of live
ones. `filter_properties` (ids or names) trims wide tables and speeds queries.
