# Error catalogue

Supporting file of the `slack-mcp` skill (`skill://slack-mcp/references/ERRORS.md`).

Slack reports failure as HTTP 200 with `{"ok": false, "error": "<code>"}`;
only rate limiting uses HTTP 429. The server maps every case to a tool error
(`isError: true`) whose `structuredContent` is `{error, retryable, message}`
and whose text is the same `message`. Local refusals (validation, the write
fence) carry text only. Rule of thumb: **only `retryable: true` errors are
worth a second attempt, and only after waiting.**

| Condition | `error` | Meaning | What to do | Retry? |
|---|---|---|---|---|
| `SLACK_BOT_TOKEN` unset/empty | — | secret ref `slack-mcp-bot-token` not registered or not listed in `secretFrom` | relay the message: create an internal app at https://api.slack.com/apps (or the pre-filled manifest link), install it, register the `xoxb-` token as `slack-mcp-bot-token`; `check_auth` | no |
| HTTP 200 `ok:false` | `invalid_auth`, `not_authed`, `account_inactive`, `token_revoked`, `token_expired` | token wrong, app uninstalled, workspace deleted, token from another workspace | update the secret with the current Bot User OAuth Token (reinstall the app if needed); `check_auth` | no |
| `ok:false` with `needed`/`provided` | `missing_scope` | app lacks a bot scope for this method | add the scope under OAuth & Permissions, **reinstall the app** (scopes apply only after reinstall), retry | after reinstall |
| `ok:false` | `not_in_channel` | bot is not a member (history, post, reaction) | `/invite @app` in Slack, `join_channel` (public, `channels:join`), or `chat:write.public` for posting | after the fix |
| `ok:false` | `channel_not_found` | not a channel id, or a private channel/DM the bot cannot see | use the `C…` id from `list_channels`; private needs an invite plus `groups:read`/`groups:history` | no |
| HTTP 429 (+ `Retry-After: N`) or `ok:false ratelimited` | `ratelimited` | tier exceeded (list/users 20/min, history/replies/reactions 50/min, ~1 msg/s/channel; unlisted distributed apps 1/min & 15 items) | wait N seconds, reduce page count | yes, after waiting |
| `ok:false` | `is_archived` | channel archived; writes and reactions rejected | pick another channel or ask an admin to unarchive | no |
| `ok:false` | `thread_not_found`, `message_not_found`, `bad_timestamp` | ts does not identify a message in that channel (float-rounded ts, wrong channel, reply ts used as parent) | copy the exact `ts` from `get_channel_history` in the same channel; use the parent ts for threads | no |
| `ok:false` | `already_reacted` | bot already added that emoji | `add_reaction` returns success with `already_reacted: true` | n/a |
| `ok:false` | `invalid_name`, `too_many_emoji`, `too_many_reactions` | emoji unknown to the workspace or reaction cap reached | use a standard short name without colons (`thumbsup`, `white_check_mark`, `eyes`) or a workspace custom emoji | no |
| `ok:false` | `invalid_cursor` | cursor expired or from another method | restart from page 1 (omit `cursor`) | yes, from page 1 |
| `ok:false` | `limit_required` | `users.list` on a large workspace without `limit` | the server always sends `limit`; report as a bug | no |
| `ok:false` | `invalid_types` | asked for `private_channel` without `groups:read` | retry with `types=public_channel`, or add `groups:read` and reinstall | with public only |
| `ok:false` | `user_not_found` | bad or deleted user id | get the `U…` id from `get_users` (deactivated users appear with `deleted: true`) | no |
| local refusal | — | empty text, or over 40,000 characters | rejected before the call; split into several messages/thread replies | no |
| `ok:false` | `restricted_action`, `ekm_access_denied`, `method_not_supported_for_channel_type` | workspace policy forbids posting/joining here (admin setting, EKM, or a private/DM join) | report to the user with the channel id | no |
| HTTP 5xx, `internal_error`, `fatal_error`, `service_unavailable`, timeout | varies / none | Slack-side transient failure | retry once after a few seconds | yes |
| local refusal "writes disabled by SLACK_READ_ONLY" / "not in SLACK_CHANNEL_IDS" | — | local policy fence, no Slack call made | ask the operator to change the workload config if the write is intended | no |
| `check_auth.team_id_match: false` | — | token belongs to a different workspace than `SLACK_TEAM_ID` | set `SLACK_TEAM_ID` to `identity.team_id` or install the app in the intended workspace | n/a |
| local refusal "is not a Slack ID" / "not a Slack message timestamp" / "cursor contains…" | — | argument shape wrong (`#name`, `@name`, float ts, pasted junk) | fix the argument using `list_channels`/`get_users`/history output | no |
| "could not reach Slack … HttpRequestDenied" | — | `slack.com` missing from the workload's `allowedHosts` | operator fixes the manifest | no |
| "not the Slack JSON envelope" | — | `SLACK_BASE_URL` points somewhere that is not Slack | operator fixes the config | no |
