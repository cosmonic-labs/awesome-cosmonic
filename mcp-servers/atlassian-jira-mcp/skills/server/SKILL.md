---
name: atlassian-jira-mcp
description: Work with Jira Cloud issues through this server — search with JQL, read issues and comments, create/edit/comment/transition/assign issues, look up projects, issue types, create-screen fields and users. Use when a task mentions Jira tickets, issue keys like PROJ-123, sprints/backlogs in Jira, or needs an accountId, and to interpret this server's errors (401/403/404/400/429).
---

# Using the atlassian-jira-mcp server

This server talks to **Jira Cloud** (REST API v3) with an Atlassian API token
over HTTPS from a sandboxed WebAssembly component. It is **stateless**: every
call is self-contained, nothing carries over, and there is no session. Write
tools may be switched off by the deployment (`JIRA_READ_ONLY=true`).

Tool argument schemas are on the wire (`tools/list`); a compact reference is
in [references/TOOLS.md](references/TOOLS.md). This document is the operating
knowledge a schema cannot carry.

## Start here

1. **`check_auth` first.** It validates the credentials with one cheap call
   and returns `status: ok|missing|invalid|insufficient|error`, the account's
   `accountId`, the route (`site` for classic tokens, `gateway` for scoped
   tokens), whether the server is read-only, and any `projects_filter`.
   **Never retry `missing` or `invalid`** — surface the `remediation` text to
   the user; it names the secret ref, the env var and the token page.
2. Keep the `accountId` from `check_auth`/`get_myself`: JQL `currentUser()`
   resolves to it and `assign_issue` / `create_issue` need accountIds, never
   names or emails.

## Sequencing that avoids wasted calls

- **Find issues**: `search_issues` with explicit, minimal `fields`; only then
  `get_issue` on the few keys you need detail for (`include_comments: true`
  only when you need the thread — it can be large). `count_issues` gives the
  approximate total the search endpoint no longer returns.
- **Create an issue**: `list_projects` (exact key, style) → `list_issue_types`
  (exact type name/id) → `get_create_fields` (required fields, allowed
  values, custom field ids) → `create_issue`. Skip the middle steps only when
  you already know the project accepts plain `Task`/`Bug` with a summary.
- **Change status**: `get_transitions` → `transition_issue`. Transition ids
  differ per workflow; names differ from status names; only transitions the
  account can perform right now are listed. If a transition has a screen
  (`hasScreen: true`), call `get_transitions` with `include_fields: true` and
  pass its `requiredFields` (typically `{"resolution": {"name": "Done"}}`).
- **Assign**: `search_users` with `assignable_to_issue` (or
  `assignable_to_project`) to get an accountId that is actually assignable,
  then `assign_issue`. Omit `account_id` to unassign, `"-1"` for the project
  default.
- **Comment**: `add_comment` with plain text; blank lines make paragraphs.

## JQL rules the upstream enforces

- The query must be **bounded**: at least one restriction (`project = X`,
  `assignee = currentUser()`, `updated >= -30d`). An `ORDER BY`-only query is
  rejected (400 "unbounded"). ORDER BY may name at most 7 fields.
- Pagination is **cursor-based**: pass back `next_page_token` until
  `is_last` is true. There is no offset and no total.
- `fields` defaults to a compact set; `description` is not included unless
  you ask. `*all` on a search is expensive — prefer `get_issue` for one key.
- Search is eventually consistent: an issue created or edited seconds ago may
  not appear yet. Use `get_issue` on the key, or retry after a few seconds.
- If the deployment sets `JIRA_PROJECTS_FILTER`, every search/count is
  wrapped as `(<your jql>) AND project in (KEYS)` and `list_projects` is
  filtered; `check_auth` reports the keys. Don't fight it.

## Rich text is ADF

Descriptions and comment bodies on API v3 are Atlassian Document Format
(JSON), not strings. You never see it: the server renders ADF to readable
text on the way out (headings, lists, code blocks, tables, quotes, panels,
mentions, links, dates, `[media: name]` placeholders; capped at 20 000
characters with `…[truncated]`) and wraps the plain text you send into
paragraphs on the way in. Pass plain text; do not construct ADF yourself.
`expand: "renderedFields"` on `get_issue` additionally returns Jira's HTML.

## Identity, keys and case

- Users are addressed by **accountId** only. `emailAddress` may be null
  (privacy settings). `search_users` returns an **empty list, not 403**, when
  the account lacks *Browse users and groups* — try the assignable scope or
  ask an admin for the id.
- Project keys and issue type names are **case-sensitive** (`PROJ`, `Task`).
  Issue keys are validated (`PROJ-123` or a numeric id); anything else is
  refused before a request is made.
- Labels cannot contain spaces. Summaries are at most 255 characters.
  Custom fields are `customfield_NNNNN` — pass them via `extra_fields`.
- Team-managed ("next-gen", `style: next-gen`) projects use `parent_key` for
  epic children instead of the classic Epic Link custom field.

## Error catalogue

Every failure is a tool error (`isError: true`) whose `structuredContent.error`
carries `kind`, `status`, Jira's own `messages`/`errors`, `retryable`, a
`hint`, and `retry_after_seconds` when known.

| What you see | Meaning | Do |
|---|---|---|
| `kind: not_configured` — "Jira is not configured: ATLASSIAN_API_TOKEN … is not set" | The secret ref `atlassian-api-token` (or named config `ATLASSIAN_SITE`/`ATLASSIAN_EMAIL`) is missing from the deployment | Stop. Relay the remediation: create a token at id.atlassian.com/manage-profile/security/api-tokens, register it as `atlassian-api-token` (env `ATLASSIAN_API_TOKEN`), set site/email in named config, redeploy, `check_auth`. |
| `kind: read_only` — "refused: … JIRA_READ_ONLY=true" | Deployment policy, not a transient failure | Do not retry; read tools still work. |
| `kind: invalid_input` | The server refused the arguments before dialing (bad key format, empty JQL, label with spaces, >255-char summary, both `labels` and `add_labels`, …) | Fix the arguments as the message says. |
| HTTP **401**, `kind: unauthorized` | Wrong email/token, token expired (all tokens expire, max 1 year) or revoked, or a token created "with scopes" used against the site URL | Do not retry. The user must re-check `ATLASSIAN_EMAIL`, create a new token, and for scoped tokens set `ATLASSIAN_CLOUD_ID` (from `https://<site>/_edge/tenant_info`) so calls route via `api.atlassian.com`. Then `check_auth`. |
| HTTP **403**, `kind: forbidden` | Authenticated but lacking a Jira permission (Browse Projects, Create/Edit/Assign Issues, Add Comments, Transition Issues) or the scoped token lacks `read:jira-work` / `write:jira-work` / `read:jira-user` | Do not retry. Name the operation and likely permission; suggest another project or a Jira admin. |
| HTTP **404** on an issue | The issue does not exist **or** the account cannot browse it — Jira does not distinguish | Check the key (project prefix is case-sensitive) or `search_issues` with `key = X`; do not loop. |
| HTTP **404** on a project / createmeta | Wrong (case-sensitive) key, not browsable, or no Create Issues permission | `list_projects` with `query` to find the exact key. |
| HTTP **400** from search/count | JQL error: unknown field, syntax, unbounded query, >7 ORDER BY fields | Jira's message is precise — fix the JQL accordingly. |
| HTTP **400** from create/update with per-field `errors` | Missing required field, field not on the screen, invalid allowed value, wrong custom field id, non-ADF body | `get_create_fields` (or `get_issue` with `expand: editmeta`) for the exact names, ids and allowed values; never guess priority/component names. |
| HTTP **400** from transition | Transition not valid now, or its screen needs fields (e.g. resolution) | `get_transitions` with `include_fields: true`; pass `requiredFields` in `fields`. |
| HTTP **400** from assign | accountId unknown or not assignable in this project | `search_users` with `assignable_to_issue`. |
| HTTP **409** | Concurrent update | Re-read (`get_issue`) and retry once. |
| HTTP **413** | A per-issue limit (comments, worklogs, attachments) was reached | Do not retry. |
| HTTP **422** | Workflow validator / post-function / field configuration blocks the change | Needs a Jira admin; do not retry. |
| HTTP **429**, `kind: rate_limited` | Per-second burst, hourly points budget, or per-issue write limit (20 writes / 2 s, 100 / 30 s) | Wait at least `retry_after_seconds` (exponential backoff with jitter, cap 4 tries), request fewer fields, smaller pages; the server never retries for you. |
| HTTP **410** | The removed legacy `/search` endpoint was reached — only possible with a stale `ATLASSIAN_BASE_URL` | Report the configuration problem. |
| HTTP **5xx**, `retryable: true` | Jira-side problem | Retry once after a short delay. |
| `kind: html_response` | HTML instead of JSON: `ATLASSIAN_SITE` names a site that does not exist, or a login redirect | Report; the site host needs fixing. |
| `kind: transport` — "Could not reach Jira … host not allowed / denied" | The site host (or `api.atlassian.com` for scoped tokens) is not in the workload's `allowedHosts` | Policy decision; do not retry. The deployment must list `https://<site>.atlassian.net`. |
| `kind: transport` — "… timed out" | Upstream deadline (30 s) | Retry once, then report Jira as unreachable. |
| `kind: no_such_transition` / `no_such_issue_type` | The name you gave matches nothing available; the error lists what is | Pick from the list. |
| JSON-RPC `-32602` (no `result`) | The request itself was malformed — a missing or ill-typed arguments object | Fix the call; not an outage. |
| HTTP 403 before any JSON-RPC body | The server's Host guard (`MCP_ALLOWED_HOSTS`) rejected the name you connected under | Deployment issue, not a Jira error. |

## Rate budget etiquette

Jira Cloud limits are per user and points-based (each returned user costs
extra). Keep `fields` minimal, use `max_results` ≤ 100, do not spin on 429,
and batch writes to the same issue (one `update_issue` with several fields
beats five). Prefer `count_issues` to paging through everything just to count.

## Deployment notes an agent may need to relay

- Classic API tokens work against `https://<site>.atlassian.net`. Tokens
  created "with scopes" only work through
  `https://api.atlassian.com/ex/jira/<cloudId>` — that needs
  `ATLASSIAN_CLOUD_ID` **and** `api.atlassian.com` in `allowedHosts`.
- The secret ref is `atlassian-api-token` (env `ATLASSIAN_API_TOKEN`) and is
  shared with the Confluence server — one Atlassian account, one ref. Values
  should be pasted into Cosmonic Desktop → Secrets rather than typed into a
  chat.
- `GET /` on the server shows a `credentials` block (configured/missing, never
  values) without an MCP handshake.
