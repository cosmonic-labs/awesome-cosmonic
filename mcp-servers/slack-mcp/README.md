# slack-mcp

Slack Web API MCP server for [Cosmonic Desktop](https://cosmonic.com/docs/desktop),
driven by a **bot token** (`xoxb-…`): list and inspect channels, read channel
history and threads, list members and profiles, and — gated by a local write
policy — post messages, reply in threads, add reactions and join public
channels. Every call is `https://slack.com/api/<method>` with
`Authorization: Bearer`, so the sandbox needs exactly one outbound host and no
OAuth flow.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://slack-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://slack-mcp.localhost:8200/>.

## Tools

| Tool | Params | Output (`structuredContent` + text) | Gated? |
|---|---|---|---|
| `check_auth` | — | `status` (`ok`/`missing`/`invalid`/`insufficient`), identity (team, team_id, url, user, user_id, bot_id), `team_id_match`, `granted_scopes`, `missing_required_scopes`, `read_only`, `channel_fence`, `remediation` | no |
| `list_channels` | `limit?` 1..200 (100), `cursor?`, `types?` (`public_channel` \| `public_channel,private_channel`) | `channels[]` {id, name, is_private, is_member, is_archived, is_general, num_members, created, topic, purpose}, `count`, `next_cursor`, `source` | no |
| `get_channel_info` | `channel_id` | one channel object | no |
| `get_channel_history` | `channel_id`, `limit?` 1..200 (10), `cursor?`, `oldest?`, `latest?`, `inclusive?` | `messages[]` {ts, thread_ts, user, bot_id, username, subtype, text, text_truncated, reply_count, reply_users_count, latest_reply, reactions[], has_files, has_attachments, has_blocks, edited}, `has_more`, `next_cursor`, newest first | no |
| `get_thread_replies` | `channel_id`, `thread_ts`, `limit?` 1..200 (100), `cursor?` | same message shape, oldest first, parent at index 0 | no |
| `get_users` | `limit?` 1..200 (100), `cursor?` | `members[]` {id, name, real_name, display_name, title, is_bot, is_admin, is_restricted, deleted, tz}, `next_cursor` | no |
| `get_user_profile` | `user_id`, `include_labels?` | {real_name, display_name, first/last name, title, status_text, status_emoji, status_expiration, pronouns, email, phone, tz, avatar_url, fields} | no |
| `post_message` | `channel_id`, `text` (1..40,000 chars mrkdwn), `unfurl_links?`, `unfurl_media?` | {ok, channel, ts, permalink_hint, warnings[]} | **yes** |
| `reply_to_thread` | `channel_id`, `thread_ts` (parent ts), `text`, `reply_broadcast?` | {ok, channel, ts, thread_ts, permalink_hint, warnings[]} | **yes** |
| `add_reaction` | `channel_id`, `timestamp`, `reaction` (short name) | {ok, channel, timestamp, reaction, already_reacted?} (`already_reacted` is success) | **yes** |
| `join_channel` | `channel_id` | {ok, channel, already_in_channel} | **yes** |

"Gated" tools are refused locally — no Slack call — when `SLACK_READ_ONLY=true`
or when `SLACK_CHANNEL_IDS` is set and the channel is not in it. All ids are
Slack IDs (`C…`, `U…`), never `#names`; `limit` is clamped to Slack's 1..200;
query values are percent-encoded; every failure is a tool error whose
`structuredContent` carries `{error, retryable, message}`.

The playbook (which tool first, sequencing, the error catalogue, mrkdwn,
rate limits) is the skill: `skill://slack-mcp/SKILL.md`, with
`references/TOOLS.md`, `references/ERRORS.md` and `references/SCOPES.md`.

## Configuration

| Env var | Kind | Default | Required | Purpose |
|---|---|---|---|---|
| `SLACK_BOT_TOKEN` | secret ref `slack-mcp-bot-token` | — | yes | Bot User OAuth Token (`xoxb-…`). Missing → every tool returns an actionable error naming the ref; invalid → Slack's `invalid_auth` plus the same hint. |
| `SLACK_TEAM_ID` | named config | `""` | recommended | Workspace id (`T…`), forwarded as `team_id` to `conversations.list`/`users.list` (required on Enterprise Grid). `check_auth` reports the token's team id and flags a mismatch. |
| `SLACK_CHANNEL_IDS` | named config | `""` | no | Comma-separated channel ids. When set, `list_channels` returns exactly these (archived dropped, no cursor) and writes are fenced to them. |
| `SLACK_READ_ONLY` | named config | `false` | no | `true` disables `post_message`, `reply_to_thread`, `add_reaction`, `join_channel` without calling Slack. |
| `SLACK_BASE_URL` | named config (test override) | `https://slack.com` | no | Upstream base; `https://slack-gov.com` for GovSlack (add that host to `allowedHosts`). The e2e points it at a local fixture. |
| `MCP_ALLOWED_HOSTS` | named config | `slack-mcp.localhost` | yes | DNS-rebinding guard; must equal the ingress host. |
| `RUST_LOG` | named config | `info` | no | Log filter. |
| `MCP_OUTBOUND_TIMEOUT_MS` / `MCP_OUTBOUND_MAX_BYTES` | named config | `30000` / 4 MiB | no | Template outbound deadline and body cap. |

### Getting the bot token

1. Create an internal Slack app. The quickest path is the pre-filled manifest
   link (creates the app with every scope below; you pick the workspace):
   <https://api.slack.com/apps?new_app=1&manifest_json=%7B%22display_information%22%3A%7B%22name%22%3A%22Cosmonic%20Slack%20MCP%22%2C%22description%22%3A%22Slack%20MCP%20server%20on%20Cosmonic%20Desktop%20%28slack-mcp%29%22%7D%2C%22features%22%3A%7B%22bot_user%22%3A%7B%22display_name%22%3A%22cosmonic-mcp%22%2C%22always_online%22%3Afalse%7D%7D%2C%22oauth_config%22%3A%7B%22scopes%22%3A%7B%22bot%22%3A%5B%22channels%3Aread%22%2C%22channels%3Ahistory%22%2C%22channels%3Ajoin%22%2C%22groups%3Aread%22%2C%22groups%3Ahistory%22%2C%22chat%3Awrite%22%2C%22chat%3Awrite.public%22%2C%22reactions%3Awrite%22%2C%22users%3Aread%22%2C%22users.profile%3Aread%22%5D%7D%7D%2C%22settings%22%3A%7B%22org_deploy_enabled%22%3Afalse%2C%22socket_mode_enabled%22%3Afalse%2C%22token_rotation_enabled%22%3Afalse%7D%7D>
   — or manually at <https://api.slack.com/apps> → *Create New App* → *From
   scratch* → *OAuth & Permissions* → *Bot Token Scopes*.
2. Scopes. Required: `channels:read`, `channels:history`, `chat:write`,
   `reactions:write`, `users:read`, `users.profile:read`. Optional:
   `groups:read` + `groups:history` (private channels the bot is in),
   `channels:join` (`join_channel`), `chat:write.public` (post to public
   channels without an invite), `users:read.email` (profile emails).
3. *Install to Workspace* (a workspace admin may need to approve), then copy
   the **Bot User OAuth Token** (`xoxb-…`). Any later scope change needs a
   reinstall before it takes effect.
4. Register it as the secret. Prefer pasting it in Cosmonic Desktop → Secrets
   (ref `slack-mcp-bot-token`, env `SLACK_BOT_TOKEN`); otherwise:

   ```console
   $ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock      # Linux; macOS: "$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
   $ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
       -H 'Content-Type: application/json' \
       -d '{"name":"slack-mcp-bot-token","uri":"keychain://cosmonic/slack-mcp-bot-token","env":"SLACK_BOT_TOKEN","value":"xoxb-…"}'
   ```

   or with the MCP tool: `cosmonic_set_secret name=slack-mcp-bot-token
   uri=keychain://cosmonic/slack-mcp-bot-token env=SLACK_BOT_TOKEN value=<xoxb-…>`.
5. Set `SLACK_TEAM_ID` in `deploy/workload.yaml` (run `check_auth` once — it
   reports the token's `team_id`), and invite the bot to the channels it should
   read (`/invite @Cosmonic Slack MCP`), or let it `join_channel`.

`GET /` shows whether the secret is configured (never its value) in a
`credentials` block, and `check_auth` validates it end to end.

## Outbound policy

`allowedHosts: ["slack.com"]` in `deploy/workload.yaml` and
`.wash/config.yaml` — the only host the tools dial. No loopback ports, no
volumes, no extra host interfaces. GovSlack: set
`SLACK_BASE_URL=https://slack-gov.com` and add `slack-gov.com`.

## Build and test

```console
$ cargo build --release                      # target/wasm32-wasip2/release/slack_mcp.wasm
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ wasm-tools component wit target/wasm32-wasip2/release/slack_mcp.wasm | grep 'export wasi:http/handler@0.3.0'
$ scripts/e2e.sh                             # hermetic: Python fixture stands in for slack.com
$ E2E_LIVE=1 SLACK_BOT_TOKEN=xoxb-… scripts/e2e.sh --no-build   # + two read-only live calls
```

The e2e runs five wasmtime instances (primary, guard without the secret,
fenced, read-only, bad-token) against a threaded fixture that impersonates
the Slack API (auth checks, scripted error ids, HTTP 429 + `Retry-After`,
a 503) and echoes each request so the suite can assert limit clamping,
percent-encoding, the JSON bodies and the bearer/charset headers.
`cargo test` is not used (wasm target).

## Deploy on Cosmonic Desktop

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/slack-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/slack-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"slack-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/slack-mcp:0.1.0@sha256:…", …}
```

Register the secret (above), then apply `deploy/workload.yaml` with `image`
replaced by that digest-pinned reference (`cosmonic_apply_workload`, or
`POST /v1/workloads`):

```console
$ IMAGE='oci.localhost:8200/apps/slack-mcp:0.1.0@sha256:…'
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads -H 'Content-Type: application/json' --data-binary @-
```

The manifest carries the `mcp.ai/*` labels, the
`desktop.cosmonic.com/credentials` annotation, `MCP_ALLOWED_HOSTS`,
`secretFrom: [{name: slack-mcp-bot-token}]` and `allowedHosts: [slack.com]`.

### Talk to it

```console
$ curl -s http://slack-mcp.localhost:8200/ | jq '.status, .capabilities.tools, .credentials[0].status'
"ok"
["add_reaction","check_auth","get_channel_history","get_channel_info","get_thread_replies","get_user_profile","get_users","join_channel","list_channels","post_message","reply_to_thread"]
"configured"

$ curl -s -X POST http://slack-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://slack-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: check_auth' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"check_auth","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"token ok: workspace Acme (T0123ABCD), bot user cosmonic-mcp (U0…). read_only=false, channel_fence=none"}],"structuredContent":{"status":"ok",…},"isError":false}}

$ curl -s -X POST http://slack-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: get_channel_history' \
    -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_channel_history","arguments":{"channel_id":"C0123ABCD4","limit":5},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Connect a client (the server is stateless, any streamable-HTTP client works):

```console
$ claude mcp add --transport http slack-mcp http://slack-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"slack-mcp":{"type":"http","url":"http://slack-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [modelcontextprotocol/servers-archived — `src/slack`](https://github.com/modelcontextprotocol/servers-archived/tree/main/src/slack)
  (MIT): the archived official reference server. Tool surface and parameter
  names (`list_channels`, `post_message`, `reply_to_thread`, `add_reaction`,
  `get_channel_history`, `get_thread_replies`, `get_users`,
  `get_user_profile`), the `SLACK_BOT_TOKEN` / `SLACK_TEAM_ID` /
  `SLACK_CHANNEL_IDS` env contract (per-id `conversations.info` shortcut with
  archived channels dropped), the 1..200 limit clamp and the bot scope list.
  Re-implemented in Rust; nothing copied verbatim.
- [korotovsky/slack-mcp-server](https://github.com/korotovsky/slack-mcp-server)
  (MIT): the idea of a write fence (`SLACK_READ_ONLY`, `SLACK_CHANNEL_IDS`
  as a posting allow-list). Its browser-cookie (`xoxc`/`xoxd`) auth and
  user-token search were deliberately not borrowed.
- Slack Web API reference (<https://docs.slack.dev/reference/methods/>):
  endpoints, argument limits, error codes, tiers and `Retry-After` semantics.

This port is Apache-2.0 (see `LICENSE`).

## Known limitations

- **No search**: `search.messages` needs a user token (`search:read`), which
  requires an OAuth redirect flow this sandboxed server does not run. Use
  `get_channel_history` with `oldest`/`latest` windows, or Slack's hosted MCP
  server (`mcp.slack.com`, user-token OAuth, partner clients only) for search,
  canvases and lists.
- **Membership-bound visibility**: the bot only sees channels it is in.
  `not_in_channel` is the most common failure; `get_channel_info.is_member`,
  `join_channel` (public, `channels:join`) and `chat:write.public` (posting
  only) mitigate it, but private channels still need a human `/invite`.
- **No DMs, edits, deletes or uploads**: `im`/`mpim` listing, `chat.update`,
  `chat.delete` and `files.upload` are not exposed. Posts and reactions are
  irreversible from here — hence `SLACK_READ_ONLY` and the channel fence.
- **Rate limits depend on the app type**: unlisted commercially-distributed
  apps get 1 req/min and 15 items on history/replies; use an internal app.
- `users.profile.get` with `include_labels=true` is heavily rate limited by
  Slack; it defaults to `false`.
- Output is bounded: message text is cut at 4,000 characters
  (`text_truncated: true`), topics/titles at 500; upstream bodies over 4 MiB
  fail (`MCP_OUTBOUND_MAX_BYTES`).
