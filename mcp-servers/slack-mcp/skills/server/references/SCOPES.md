# Bot token scopes

Supporting file of the `slack-mcp` skill (`skill://slack-mcp/references/SCOPES.md`).

The token is a **Bot User OAuth Token** (`xoxb-…`) of an internal Slack app.
Scopes are granted under *OAuth & Permissions → Bot Token Scopes* at
https://api.slack.com/apps and only take effect after the app is (re)installed
to the workspace. `check_auth` reports `granted_scopes` (from Slack's
`X-OAuth-Scopes` header) and `missing_required_scopes`.

The pre-filled manifest link in `check_auth`'s `obtain_url` (and the README)
creates an app with every scope below.

## Required

| Scope | Used by | Without it |
|---|---|---|
| `channels:read` | `list_channels`, `get_channel_info` (public) | `missing_scope` |
| `channels:history` | `get_channel_history`, `get_thread_replies` (public) | `missing_scope` |
| `chat:write` | `post_message`, `reply_to_thread` | `missing_scope` |
| `reactions:write` | `add_reaction` | `missing_scope` |
| `users:read` | `get_users` | `missing_scope` |
| `users.profile:read` | `get_user_profile` | `missing_scope` |

## Optional

| Scope | Unlocks |
|---|---|
| `groups:read` | private channels (the bot is a member of) in `list_channels` with `types=public_channel,private_channel` and in `get_channel_info` |
| `groups:history` | history/replies in those private channels |
| `channels:join` | `join_channel` (public channels only) |
| `chat:write.public` | posting to public channels the bot has not joined (history still needs membership) |
| `users:read.email` | the `email` field in `get_user_profile` |

## Not possible with a bot token

`search:read` (`search.messages`), reading DMs the bot is not part of,
editing or deleting other users' messages, canvases and lists. Those need a
user token (`xoxp-…`) obtained through an OAuth redirect flow, which this
sandboxed server does not run — Slack's hosted MCP server is the alternative.

## Membership is the other half

Scopes say what the app *may* do; a bot still only sees channels it is a
**member** of. `not_in_channel` is a membership problem, not a scope problem:
`/invite @app`, `join_channel`, or `chat:write.public` (posting only).
