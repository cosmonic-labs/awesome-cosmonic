---
name: atlassian-confluence-mcp
description: Use when a task needs Confluence Cloud content — searching pages with CQL, reading a page or its children/comments/labels/attachments, resolving a space key to its id, or creating, updating, commenting on, labelling or trashing pages — via an Atlassian API token, and when interpreting this server's errors (401/403/404/409/429, gates, version conflicts).
---

# Using the atlassian-confluence-mcp MCP server

This server runs as a sandboxed WebAssembly component on Cosmonic Desktop and
talks to **Confluence Cloud** (`https://<site>.atlassian.net/wiki`) with an
Atlassian API token over HTTP Basic auth. It is **stateless**: every call is
self-contained; nothing carries between calls; there is no session.

Tool reference (arguments, limits, result shapes): [references/TOOLS.md](references/TOOLS.md).
CQL cheat sheet: [references/CQL.md](references/CQL.md).

## Start here

1. **`check_auth` first.** It returns `status: ok|missing|invalid|insufficient`
   plus the account identity, the route (`site` or `gateway`), and the write
   gates in force (`read_only`, `allow_delete`, `spaces_filter`). Never retry
   a `missing` or `invalid` result — relay its `remediation` text to the user:
   the token goes into the `atlassian-api-token` secret (env
   `ATLASSIAN_API_TOKEN`, created at
   <https://id.atlassian.com/manage-profile/security/api-tokens>), the site and
   email are named config. The same secret ref serves `atlassian-jira-mcp`.
2. **Everything in v2 is a numeric id, not a name.** Page ids come from page
   URLs (`.../pages/123456/Title` — the tools accept the URL directly), from
   `search`, or from `list_pages`. Space ids are numeric and are **not** the
   key (`ENG`); `list_spaces`/`get_space` turn a key into the id, and every
   tool that takes `space_id` also accepts the key and resolves it for you.
3. Typical sequences:
   - Find a page by title: `list_pages(space_id="ENG", title="Runbook")` →
     exact match (titles are unique per space) → `get_page(page_id)`.
   - Find by content: `search(text="kubernetes")` or `search(cql=…)` →
     `get_page`.
   - Browse a tree: `get_space(space)` → `homepageId` → `get_page_children`.
   - Create under the space home: `get_space` → `create_page(space_id,
     parent_id=homepageId, …)`. Omitting `parent_id` creates at the space
     **root**, not under the home page.
   - Edit: `get_page(format=storage)` (when the page has macros) or
     `get_page` (text) → `update_page(page_id, body=…, expected_version=N)`.

## Bodies: storage format, not HTML, not markdown

- Confluence stores pages as **storage format**: XHTML with `<ac:*>`/`<ri:*>`
  macro elements. Sending markdown as `storage` yields `400 Error parsing
  xhtml` or a page of literal text.
- The write tools take **markdown by default** and convert it (headings,
  lists, tables, links, task lists; fenced code → the `code` macro with
  CDATA; raw HTML is escaped as text). `body_format=wiki` passes Confluence
  wiki markup through; `body_format=storage` sends XHTML verbatim — only use
  it with content you got from `get_page(format=storage)` and edited
  carefully (every tag closed, `&` written as `&amp;`).
- Control characters XML 1.0 forbids (NUL, ESC and other C0 bytes, form
  feed, vertical tab, DEL, U+FFFE/U+FFFF) are **silently dropped** from
  titles, bodies (every `body_format`), comments and version messages —
  Confluence would otherwise answer `400 Error parsing xhtml`. Pasted
  terminal output keeps its text but loses ANSI colour codes; tab, LF and
  CR survive. Labels containing a control character are refused instead.
- `get_page` `format=text` (default) is **lossy on purpose**: macros become
  `[macro:name]` markers followed by their body, images become `![filename]`,
  Jira/TOC/excerpt-include macros lose their content. When you intend to
  rewrite a page that contains macros, read `storage` and write `storage`;
  otherwise replace the whole body with markdown. `update_page` never merges.
- Bodies over `max_chars` are cut with `…[truncated]` and `body_chars`
  reports the full length; raise `max_chars` (up to 200000) or read a
  narrower part of the tree.

## update_page is optimistic-locked

Confluence requires `version.number = current + 1` on every PUT and answers
`409 Version must be incremented on update. Current version is: N` otherwise.
`update_page` reads the live page first and sends `N + 1` (with the current
title/status when you change only the body). Pass `expected_version` = the
version you read with `get_page` when you want the write refused if someone
edited in between — the refusal (`expected_version mismatch: live version is
N`) means **no PUT was sent**; re-read, reconcile, and call again with the new
number. A 409 that still gets through (rapid double update, propagation lag)
is safe to retry once by calling `update_page` again; never resend the same
number by hand.

## search is CQL, not free text

Use the `text` parameter for the common case — it builds
`text ~ "<escaped>" AND type = page ORDER BY lastmodified DESC`. Write `cql`
yourself for anything else: `space = "ENG"`, `title ~ "Release*"`,
`label = "runbook"`, `type = blogpost`, `lastmodified > now("-7d")`,
`ancestor = 123456`, `creator = currentUser()`. Quote values, escape embedded
quotes with a backslash. `user`, `user.accountid` and similar fields are
**not** supported on `/wiki/rest/api/search` and yield `400 Could not parse
cql`. `space_key` appends `AND space = "KEY"`; when the deployment sets
`CONFLUENCE_SPACES_FILTER`, every search is wrapped in
`(<cql>) AND space in (…)`. Results carry `totalSize` and a cleaned `excerpt`.

## Pagination and limits

Every list is cursor-based: pass `next_cursor` back verbatim as `cursor`
(the tool extracts it from `_links.next`; opaque, may contain `+/=`). v2
limits are 1..250 (default 25; attachments 50); `search` is clamped to 100.
Asking for more silently gets the clamp — the result's `limit` says what was
used; a negative limit is refused before dialing. Rendered comment bodies are
cut at 4000 characters.

## Write gates (policy, never retry)

- `CONFLUENCE_READ_ONLY=true` makes `create_page`, `update_page`,
  `add_comment`, `add_label` and `delete_page` return `write tools are
  disabled` without dialing; the tools stay listed.
- `delete_page` additionally needs `CONFLUENCE_ALLOW_DELETE=true` **and**
  `confirm=true`; it only moves the page to the space trash (restorable
  under Space settings → Content tools → Trash) and never purges.
- `CONFLUENCE_SPACES_FILTER=ENG,DOCS` scopes `search`, `list_spaces` and
  `list_pages` (which then requires `space_id`) and refuses `create_page` /
  `get_space` outside the list. It is a convenience scope, not a security
  boundary — Confluence permissions still apply to every call.
- Labels: 1..20 per call, lowercased, single tokens; a label containing a
  space or `: ; , . ? & ( ) [ ] # ^ * @ !` is refused client-side (use
  `release-notes`).

## Error catalogue

Every tool error carries `structuredContent.error` with `kind`, `status`,
Confluence's own `messages`, `retryable`, `retry_after_seconds` and a `hint`.

| You see | It means | Do this |
|---|---|---|
| `Confluence is not configured: ATLASSIAN_API_TOKEN … not set` (`kind: not_configured`) | The `atlassian-api-token` secret ref / named config is missing; nothing was dialed. | Register the ref and set `ATLASSIAN_SITE` + `ATLASSIAN_EMAIL`; re-apply; `check_auth`. Do not retry. |
| HTTP **401** (`Unauthorized`, `Basic authentication with passwords is deprecated`, `scope does not match`) | Wrong email/token pair, expired or revoked token (max 1 year; pre-2024-12-15 tokens were force-expired in 2026), or a *scoped* token used against the site URL. | Regenerate an unscoped token; for scoped tokens set `ATLASSIAN_CLOUD_ID` (from `https://<site>.atlassian.net/_edge/tenant_info`) and keep `api.atlassian.com` in `allowedHosts`. Never retry with the same credentials. |
| HTTP **403** on `check_auth` / `get_current_user` | The account has no Confluence product access (`Can use`) on that site. | A site admin must grant access; confirm `ATLASSIAN_SITE` is the right site. |
| HTTP **403** on a space/page | Space permission missing for that operation, or the space is archived (writes). | Use a space the account can act in; do not retry. |
| HTTP **404** on `GET /pages/{id}` | Missing, trashed, draft, nonexistent version, **or** unviewable — v2 does not distinguish. Also a non-numeric id. | Verify the id via `search`/`list_pages`; check the space with `get_space`. |
| HTTP **404** on `create_page` | Space id not visible or no *add page* permission there (Confluence folds it into 404). | Use `list_spaces` for the numeric id; confirm permissions. |
| HTTP **400** `A page with this title already exists` | Titles are unique per space (trashed pages count). | `list_pages(space_id, title)` then `update_page`, or pick another title; check the space trash. |
| HTTP **409** `Version must be incremented` / 400 mentioning version | Concurrent edit or propagation lag between read and write. | Call `update_page` again (it re-reads); reconcile if you used `expected_version`. |
| `expected_version mismatch: live version is N` (`kind: version_mismatch`) | The page changed since you read it; **no PUT was sent**. | `get_page` again, reconcile, `update_page` with the new `expected_version`. |
| HTTP **400** `Error parsing xhtml` / `XhtmlException` / `Unexpected close tag` | `body_format=storage` was not well-formed XML (control characters are already stripped, so look at tag structure, unknown macros or attributes). | Send markdown (default) or fix the XHTML. |
| HTTP **400** `Could not parse cql` | Invalid CQL: unquoted values, unknown/unsupported field (`user.*`). | Quote values; use the fields in [references/CQL.md](references/CQL.md); or use `text`. |
| `body_too_large` / HTTP **413** | Body over 4 MiB (client cap) / 5 MB (Confluence). | Split across pages or attach a file. |
| HTTP **429** (`retry_after_seconds`, `rate_limit_reason`) | Per-user or tenant rate limit. The server already retried once when `Retry-After` ≤ 5 s. | Wait `retry_after_seconds` with jitter (double up to 30 s), then resume with smaller pages. |
| HTTP **5xx** or `html_response` | Atlassian outage/maintenance, an edge error page, or a wrong site host redirected to a login page. | Retry once after a short delay; check <https://status.atlassian.com>; verify `ATLASSIAN_SITE`. |
| `Could not reach Confluence … wasi:http error` (`kind: transport`) | The site host is not in the workload's `allowedHosts`, DNS failed (typo in the site), or TLS failed. | Fix `allowedHosts` / `ATLASSIAN_SITE`; re-apply. Do not retry a policy denial. |
| `… timed out` (`retryable: true`) | The 30 s outbound deadline elapsed. | Retry once, then report Confluence as unreachable. |
| `write tools are disabled (CONFLUENCE_READ_ONLY=true)` / `delete_page is disabled` / `confirm=true` | Deployment gate; nothing dialed. | Change the named config and re-apply; for deletes also pass `confirm=true`. |
| `space_filtered` / `space_required` | `CONFLUENCE_SPACES_FILTER` excludes that space, or `list_pages` needs a space under the filter. | Stay inside the configured spaces or ask to widen the filter. |
| `bad_page_id`, `bad_limit`, `bad_option`, `bad_labels`, `bad_title` | Input refused before dialing; the message says what to fix. | Fix the argument; do not report an outage. |

## Reading errors on the wire

- `"isError": true` inside a `result` — the tool ran and failed; the text is
  written for you and `structuredContent.error` is machine-readable.
- JSON-RPC `error` code `-32602` — the request itself was malformed (missing
  or ill-typed params); fix the call.
- HTTP `403 Forbidden` before any JSON-RPC response — the DNS-rebinding guard
  rejected the `Host` header (`MCP_ALLOWED_HOSTS`).
- HTTP `413` — the request body exceeded the transport limit.

## Upstream quirks worth knowing

- Attachments are listed with an absolute `download_url`; downloading is not
  a tool (bytes would have to be base64 in the result). Fetch the URL with
  the same Basic credentials if you need the file.
- `add_label` uses the v1 endpoint because v2 has no label-write endpoint;
  `search` and `get_current_user` are v1 for the same reason. They are not
  deprecated as of 2026-09.
- Personal spaces have keys starting with `~`; the tools accept them.
- `get_page` `version=N` reads a historical version; `list_pages`
  `status=trashed` lists the trash.
- The `GET /` discovery document of this server lists the credential
  block (`credentials[].status`) so a client can see whether the token is
  configured before calling anything.
