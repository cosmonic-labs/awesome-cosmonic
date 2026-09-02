# Tool reference — atlassian-jira-mcp

All results are `structuredContent` (plus a text fallback). Failures are tool
errors (`isError: true`) with `structuredContent.error` = `{kind, status?,
messages?, errors?, retryable, hint, retry_after_seconds?}`. Write tools are
refused with `kind: read_only` when the deployment sets `JIRA_READ_ONLY=true`.

| Tool | Arguments (— = optional) | Upstream | Returns | Gated |
|---|---|---|---|---|
| `check_auth` | none | `GET /myself` | `status` ok/missing/invalid/insufficient/error, `account` (accountId, displayName, emailAddress, timeZone, active), `site`, `base_url`, `route` site/gateway, `read_only`, `projects_filter`, `credential` {ref, env, obtainUrl, scopes}, `remediation` | no |
| `get_myself` | none | `GET /myself` | accountId, displayName, emailAddress (may be absent), timeZone, active, accountType, locale, site, route | no |
| `search_issues` | `jql` (bounded), — `fields` [default summary,status,assignee,priority,issuetype,created,updated], — `max_results` 1..100 (25), — `next_page_token`, — `expand` | `POST /search/jql` | `jql` (effective, filter applied), `fields`, `count`, `issues[]` {id, key, fields{…rendered}}, `next_page_token` (absent on last page), `is_last`, `names` when expanded | no |
| `count_issues` | `jql` | `POST /search/approximate-count` | `jql`, `count`, `approximate: true` | no |
| `get_issue` | `issue_key`, — `fields` [default `*navigable,-comment,-attachment,-worklog`], — `expand` (renderedFields, changelog, transitions, names, editmeta), — `include_comments` | `GET /issue/{key}` | id, key, `fields` (ADF rendered; users/statuses compacted), `url`, plus `renderedFields`/`changelog`/… when expanded | no |
| `create_issue` | `project_key`, `issue_type` (name or id), `summary` ≤255, — `description` (plain text → ADF), — `priority` (name), — `labels[]` (no spaces), — `assignee_account_id`, — `parent_key`, — `extra_fields` {} (not project/summary) | `POST /issue` | `ok`, `id`, `key`, `url` | yes |
| `update_issue` | `issue_key`, — `summary`, — `description`, — `priority`, — `labels[]` (replace) **or** — `add_labels[]`/`remove_labels[]`, — `assignee_account_id` ("" unassigns), — `extra_fields` {} (not project), — `notify_users` (true), — `return_issue` (false); at least one change | `PUT /issue/{key}?notifyUsers&returnIssue` | `ok`, `issue_key`, `url` — or the updated issue when `return_issue` | yes |
| `add_comment` | `issue_key`, `body` ≤32767 (plain text → ADF), — `visibility_type` role/group + — `visibility_value` | `POST /issue/{key}/comment` | `ok`, `issue_key`, `id`, `author`, `created`, `url` | yes |
| `get_comments` | `issue_key`, — `start_at` ≥0, — `max_results` 1..100 (50), — `order_by` created/-created (-created) | `GET /issue/{key}/comment` | `issue_key`, `total`, `startAt`, `maxResults`, `comments[]` {id, author, created, updated, body (text), visibility}, `has_more`, `order_by` | no |
| `get_transitions` | `issue_key`, — `include_fields` | `GET /issue/{key}/transitions[?expand=transitions.fields]` | `issue_key`, `count`, `transitions[]` {id, name, to{id,name,statusCategory}, hasScreen, isAvailable, …, `requiredFields[]`, `screenFields[]` when included} | no |
| `transition_issue` | `issue_key`, `transition` (id, transition name or target status name; case-insensitive), — `comment`, — `fields` {} for screen fields | `GET …/transitions` then `POST /issue/{key}/transitions` | `ok`, `issue_key`, `transition` (the one applied), `url` | yes |
| `assign_issue` | `issue_key`, — `account_id` (omit/"" = unassign, "-1" = project default) | `PUT /issue/{key}/assignee` | `ok`, `issue_key`, `account_id`, `result` assigned/unassigned/assigned to the project default, `url` | yes |
| `list_projects` | — `query`, — `start_at` ≥0, — `max_results` 1..100 (50), — `type_key` software/business/service_desk | `GET /project/search?orderBy=key&expand=lead,description` (+ `keys` from `JIRA_PROJECTS_FILTER`) | `start_at`, `max_results`, `count`, `total`, `is_last`, `next_start_at`, `projects_filter`, `projects[]` {id, key, name, projectTypeKey, style, simplified, isPrivate, lead, description, url} | no |
| `get_project` | `project_key` (key or id) | `GET /project/{key}?expand=issueTypes,lead,description` | id, key, name, description, projectTypeKey, style, lead, isPrivate, `issueTypes[]`, `components[]`, `versions[]`, url | no |
| `list_issue_types` | `project_key`, — `max_results` 1..200 (50) | `GET /issue/createmeta/{key}/issuetypes` | `project_key`, `count`, `total`, `issue_types[]` {id, name, description, subtask, hierarchyLevel} | no |
| `get_create_fields` | `project_key`, `issue_type` (id or name) | `GET /issue/createmeta/{key}/issuetypes/{id}?maxResults=200` | `project_key`, `issue_type` {id, name}, `required[]` ids, `count`, `fields[]` {id, name, required, type, items, custom, hasDefaultValue, operations, allowedValues[] {id, name/value}, autoCompleteUrl} | no |
| `search_users` | `query` (≥1 char), — `max_results` 1..50 (20), — `assignable_to_project` **or** — `assignable_to_issue` | `GET /user/search` or `GET /user/assignable/search` | `query`, `scope`, `count`, `users[]` {accountId, displayName, emailAddress, active, accountType, timeZone}, `note` (why a list can be empty) | no |

## Limits and clamps

| What | Limit |
|---|---|
| `search_issues.max_results` | clamped 1..100 (Jira allows more, but page size shrinks with field count) |
| `get_comments.max_results`, `list_projects.max_results` | clamped 1..100 |
| `list_issue_types.max_results` | clamped 1..200 |
| `search_users.max_results` | clamped 1..50 |
| `jql` | ≤ 10 000 characters, newlines become spaces |
| `summary` | ≤ 255 characters, trimmed |
| `description`, comment `body`, transition `comment` | ≤ 32 767 characters |
| `labels` | ≤ 100 entries, no whitespace, ≤ 255 characters each |
| `extra_fields` / transition `fields` | ≤ 64 KiB serialized |
| rendered ADF per field | ≤ 20 000 characters, then `…[truncated]`; nesting deeper than 32 levels is dropped (and a JSON body nested past 128 levels is refused as unreadable) |
| upstream response body | 4 MiB (outbound cap) |
| upstream deadline | 30 s per call |

## Key formats accepted

- Issue: `PROJ-123` (project key = letter then letters/digits/underscore, ≤ 64) or a numeric id.
- Project: key as above or numeric id.
- accountId: letters, digits, `:`, `-`, `_`, ≤ 128 characters (`5b10ac8d82e05b22cc7d4ef5`, `712020:<uuid>`).
- Transition / issue type ids: digits.

Anything else is refused with `kind: invalid_input` before a request is
made, so a key can never reach the URL path unvalidated.
