# Tool reference

Supporting file of the `slack-mcp` skill, served at
`skill://slack-mcp/references/TOOLS.md`. Every tool returns
`structuredContent` plus a readable text block; every failure is a tool
error (`isError: true`) with `{error, retryable, message}` in
`structuredContent` — see [ERRORS.md](ERRORS.md).

`limit` is clamped to 1..200 (Slack's documented range) on every paginated
tool. `cursor` is the previous result's `next_cursor`, verbatim. Channel ids
match `^[CGD][A-Z0-9]{8,}$`, user ids `^[UW][A-Z0-9]{8,}$`, timestamps
`^\d+(\.\d+)?$`; anything else is refused before a call is made.

| Tool | Arguments | Upstream | Returns | Write? |
|---|---|---|---|---|
| `check_auth` | none | `POST auth.test` | `status` (`ok`/`missing`/`invalid`/`insufficient`), `identity` {team, team_id, url, user, user_id, bot_id}, `team_id_match`, `granted_scopes`, `missing_required_scopes`, `read_only`, `channel_fence`, `remediation` | no |
| `list_channels` | `limit?` (default 100), `cursor?`, `types?` (`public_channel` default, or `public_channel,private_channel`) | `GET conversations.list?types&exclude_archived=true&limit&team_id[&cursor]` — or one `conversations.info` per configured `SLACK_CHANNEL_IDS` entry | `channels[]` {id, name, is_private, is_member, is_archived, is_general, num_members, created, topic, purpose}, `count`, `next_cursor`, `source` | no |
| `get_channel_info` | `channel_id` | `GET conversations.info?channel&include_num_members=true` | one channel object as above | no |
| `get_channel_history` | `channel_id`, `limit?` (default 10), `cursor?`, `oldest?`, `latest?`, `inclusive?` | `GET conversations.history` | `messages[]` {ts, thread_ts, user, bot_id, username, subtype, text (≤4,000 chars, `text_truncated`), reply_count, reply_users_count, latest_reply, reactions[{name,count}], has_files, has_attachments, has_blocks, edited}, `count`, `has_more`, `next_cursor`, `order` (newest first) | no |
| `get_thread_replies` | `channel_id`, `thread_ts`, `limit?` (default 100), `cursor?` | `GET conversations.replies?channel&ts&limit[&cursor]` | same message shape, oldest first, parent at index 0 | no |
| `get_users` | `limit?` (default 100, always sent), `cursor?` | `GET users.list?limit&team_id[&cursor]` | `members[]` {id, name, real_name, display_name, title, is_bot, is_admin, is_restricted, deleted, tz}, `count`, `next_cursor` | no |
| `get_user_profile` | `user_id`, `include_labels?` (default false) | `GET users.profile.get?user[&include_labels=true]` | {user_id, real_name, display_name, first_name, last_name, title, status_text, status_emoji, status_expiration, pronouns, email, phone, tz, avatar_url, fields} | no |
| `post_message` | `channel_id`, `text` (1..40,000 chars mrkdwn), `unfurl_links?`, `unfurl_media?` | `POST chat.postMessage` JSON | {ok, channel, ts, permalink_hint, warnings[]} | **yes** |
| `reply_to_thread` | `channel_id`, `thread_ts` (parent ts), `text`, `reply_broadcast?` | `POST chat.postMessage` JSON with `thread_ts` | {ok, channel, ts, thread_ts, permalink_hint, warnings[]} | **yes** |
| `add_reaction` | `channel_id`, `timestamp`, `reaction` (short name, colons optional) | `POST reactions.add` JSON | {ok, channel, timestamp, reaction, already_reacted?} — `already_reacted` is success | **yes** |
| `join_channel` | `channel_id` | `POST conversations.join` JSON | {ok, channel, already_in_channel, warning} | **yes** |

## Write gate

The four write tools check, in order: argument validity → token present →
`SLACK_READ_ONLY` → `SLACK_CHANNEL_IDS` membership → Slack. A refusal from
the local gate says so explicitly ("no Slack call was made") and is never
retryable.

## Text limits

`post_message`/`reply_to_thread` refuse empty text and text over 40,000
characters (Slack's `msg_too_long`), and attach a warning above 4,000. Text
is counted in Unicode characters, never bytes, and is sent byte-exact.

## Environment

| Env var | Kind | Default | Effect |
|---|---|---|---|
| `SLACK_BOT_TOKEN` | secret ref `slack-mcp-bot-token` | — | required; missing → actionable tool error on every tool |
| `SLACK_TEAM_ID` | named config | — | forwarded as `team_id` to `conversations.list`/`users.list`; `check_auth` cross-checks it |
| `SLACK_CHANNEL_IDS` | named config | empty | comma-separated ids: `list_channels` returns exactly these; writes fenced to them |
| `SLACK_READ_ONLY` | named config | `false` | `true` disables all write tools |
| `SLACK_BASE_URL` | named config | `https://slack.com` | upstream base (`https://slack-gov.com` for GovSlack; a fixture in tests) |
