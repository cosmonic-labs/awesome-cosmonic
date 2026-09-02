---
name: slack-mcp
description: Use when a task needs to read or write a Slack workspace through a bot token — list channels, read channel history or a thread, look up members and profiles, post a message, reply in a thread, add a reaction, or join a public channel — and when a Slack tool call failed and you need to know whether to retry, fix an ID, or ask a human to invite the bot or add a scope.
---

# Using the slack-mcp MCP server

This server wraps the Slack Web API (`https://slack.com/api/<method>`) with a
**bot token** (`xoxb-…`). It runs as a stateless WebAssembly component on
Cosmonic Desktop: every call is self-contained, nothing carries between calls,
and the only network it has is HTTPS to `slack.com`.

Tool schemas come over `tools/list`; this playbook covers what the schemas
cannot tell you. Reference tables: [Tools](references/TOOLS.md),
[Errors](references/ERRORS.md), [Scopes](references/SCOPES.md).

## Start here: `check_auth`

Call `check_auth` first in any session. It costs one cheap `auth.test` call
and tells you four things you need before planning anything:

1. **`status`** — `ok`, `missing` (secret not registered), `invalid` (Slack
   rejected the token) or `insufficient` (required scopes missing). `missing`
   and `invalid` come back as tool errors with a `remediation` string naming
   the secret ref (`slack-mcp-bot-token`), the env var (`SLACK_BOT_TOKEN`)
   and where to get the token. **Never retry a `missing`/`invalid` result** —
   relay the remediation to the user and stop.
2. **`identity`** — workspace name/id and the bot's user id (`user_id`). The
   bot's own messages in history carry `bot_id`, not that user id.
3. **`team_id_match`** — `false` means `SLACK_TEAM_ID` points at a different
   workspace than the token; the remediation says which value to set.
4. **`read_only`** and **`channel_fence`** — the write policy. If
   `read_only` is true, none of the four write tools will work; if
   `channel_fence` lists ids, writes outside them are refused locally. Check
   this before promising a user you will post something.

## IDs, not names

Every tool takes Slack IDs: channels are `C0123ABCD4` (public), `G…` (legacy
private), `D…` (DM); users are `U…` (or `W…` on Enterprise Grid). `#general`
and `@alice` are rejected before any call is made. Get ids from
`list_channels` and `get_users`; `get_channel_info` confirms an id and tells
you `is_member` — the single most useful bit before reading or posting.

## Sequencing that works

- **Read a channel**: `list_channels` → pick the id → `get_channel_info`
  (check `is_member`) → `get_channel_history` (newest first, default 10,
  max 200) → for any message with `reply_count > 0`, `get_thread_replies`
  with its `ts` as `thread_ts`.
- **Time window**: `get_channel_history` takes `oldest`/`latest` Slack
  timestamps (strings like `1712345678.000000`; seconds since epoch before
  the dot). There is **no search tool** — `search.messages` needs a user
  token — so page through a window instead, or use Slack's hosted MCP server
  for search.
- **Post**: `check_auth` (policy) → `get_channel_info` (`is_member`) → if not a
  member: `join_channel` (public only, needs `channels:join`) or ask a human
  to `/invite` the bot → `post_message`. The result's `ts` is what
  `reply_to_thread` and `add_reaction` need.
- **Reply in a thread**: `thread_ts` is the **parent** message's `ts`, i.e.
  the `thread_ts` field on any reply or the `ts` of the parent. Using a
  reply's own `ts` starts a new thread on that reply — Slack allows it, users
  hate it.
- **Mention someone**: `get_users` → `<@U0123ABCD4>` in the text.

## Timestamps are strings

`ts` values look like `1712345678.123456` and are message identifiers. Keep
them verbatim. Parsing them as floats loses precision and produces
`message_not_found` / `thread_not_found`. `get_channel_history` returns
newest first; `get_thread_replies` returns oldest first with the parent as
element 0.

## Text is mrkdwn, not Markdown

`*bold*`, `_italic_`, `~strike~`, `` `code` ``, `> quote`, `<@U123>` mention,
`<#C123|name>` channel link, `<https://url|label>` link. No `#` headings, no
`[text](url)`, no `**bold**`. Escape literal `&`, `<`, `>` as `&amp;`,
`&lt;`, `&gt;`. Keep messages under 4,000 characters (the tool warns above
that and refuses above 40,000); split long content into a message plus thread
replies.

## Pagination

Every list tool returns `next_cursor`; pass it back verbatim as `cursor`
until it is empty. Cursors are method-specific and expire (`invalid_cursor`
→ restart from page 1). `limit` is clamped to 1..200 silently. In
`SLACK_CHANNEL_IDS` mode `list_channels` has no cursor (it returns exactly the
configured channels, archived ones dropped).

## Rate limits

Slack answers rate limiting with HTTP 429 and a `Retry-After` header; the tool
error says `retry after N s`. Wait that long, do not tighten the loop, and
prefer larger `limit` over more pages. Tiers: `conversations.list`/`users.list`
20 req/min; history/replies/reactions 50 req/min; posting about 1 message
per second per channel. Unlisted commercially-distributed apps get 1 req/min
and at most 15 items on history/replies — internal apps (what this server
expects) are exempt, so a hard cap of 15 means the token comes from the wrong
kind of app.

## Reading errors

Slack signals failure with HTTP 200 and `{"ok": false, "error": "…"}`; the
server turns that into a tool error (`isError: true`) whose
`structuredContent` has `error` (Slack's code), `retryable` and `message`.
The rules that matter most:

| You see | It means | Do |
|---|---|---|
| `not_in_channel` | bot is not a member | `/invite @app` by a human, `join_channel` (public), or `chat:write.public` for posting. Not retryable. |
| `channel_not_found` | not an id, or a private channel/DM the bot cannot see | use the id from `list_channels`; private needs an invite plus `groups:*` scopes |
| `missing_scope` (`needs X, token has Y`) | app lacks a scope | add it under OAuth & Permissions **and reinstall the app** — scopes apply only after reinstall |
| `invalid_auth` / `account_inactive` / `token_revoked` | token wrong, app uninstalled | fix the `slack-mcp-bot-token` secret; `check_auth` |
| `ratelimited` (HTTP 429) | tier exceeded | wait `Retry-After` seconds |
| `is_archived` | channel archived | pick another channel; never retry |
| `thread_not_found` / `message_not_found` | wrong `ts` or wrong channel | copy the exact `ts` from history in the same channel |
| `already_reacted` | reaction already present | the tool reports success; nothing to do |
| `writes disabled by SLACK_READ_ONLY` / `not in SLACK_CHANNEL_IDS` | local policy, no Slack call made | ask the operator; never retry |
| HTTP 5xx / `internal_error` / timeout | Slack-side transient | retry once after a few seconds |

The full catalogue with every code this server distinguishes is in
[references/ERRORS.md](references/ERRORS.md).

## Scope of this server

Bot-token only: no search, no DMs, no message editing or deletion, no file
upload, no canvases/lists. Private channels work only if the bot was invited
and the app has `groups:read` + `groups:history`. GovSlack deployments set
`SLACK_BASE_URL=https://slack-gov.com`. Slack's hosted MCP server
(`mcp.slack.com`, user-token OAuth) is the alternative for search and canvases.
