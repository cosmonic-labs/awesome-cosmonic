//! The MCP server: tool definitions and result rendering for the Slack Web
//! API client in [`crate::slack`].
//!
//! Every tool returns `Ok(CallToolResult)`: shaped results carry
//! `structuredContent` plus a readable text fallback, and every failure —
//! validation, the local write fence, a Slack `ok:false` envelope, a 429, a
//! 5xx — is a tool-level error (`isError: true`) whose text says what to do.
//! `Err(ErrorData)` is reserved for requests rmcp cannot route or parse.
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::skills;
use crate::slack::{self, Config, Error};

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so do not keep per-session
/// state on this struct.
#[derive(Clone)]
pub struct TemplateServer {
    tool_router: ToolRouter<Self>,
}

/// Message text longer than this is cut in history/replies output (the
/// `text_truncated` flag says so); Slack allows up to 40,000 characters.
const MESSAGE_TEXT_LIMIT: usize = 4_000;
/// Topic/purpose/title strings are cut here.
const SHORT_TEXT_LIMIT: usize = 500;

/// Channel kinds `list_channels` can ask Slack for. `im`/`mpim` are not
/// offered: DMs are out of scope for a bot-token server.
#[derive(Debug, Deserialize, JsonSchema)]
pub enum ChannelTypes {
    /// Public channels only (default).
    #[serde(rename = "public_channel")]
    Public,
    /// Public channels plus the private channels the bot is a member of
    /// (needs the `groups:read` scope).
    #[serde(rename = "public_channel,private_channel")]
    PublicAndPrivate,
}

impl ChannelTypes {
    fn as_str(&self) -> &'static str {
        match self {
            ChannelTypes::Public => "public_channel",
            ChannelTypes::PublicAndPrivate => "public_channel,private_channel",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListChannelsParams {
    /// Channels per page, 1..200 (default 100; out-of-range values are
    /// clamped). Ignored when SLACK_CHANNEL_IDS is configured.
    #[serde(default)]
    pub limit: Option<i64>,
    /// `next_cursor` from the previous page. Omit for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// "public_channel" (default) or "public_channel,private_channel"
    /// (private channels need the groups:read scope and bot membership).
    #[serde(default)]
    pub types: Option<ChannelTypes>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelParams {
    /// Slack channel ID such as C0123ABCD4 (from list_channels) — never a
    /// #name.
    pub channel_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChannelHistoryParams {
    /// Slack channel ID such as C0123ABCD4 (from list_channels). The bot must
    /// be a member of the channel.
    pub channel_id: String,
    /// Messages per page, 1..200 (default 10; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// `next_cursor` from the previous page. Omit for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Only messages after this Slack timestamp (e.g. "1712345678.000000").
    #[serde(default)]
    pub oldest: Option<String>,
    /// Only messages before this Slack timestamp.
    #[serde(default)]
    pub latest: Option<String>,
    /// Include messages exactly at `oldest`/`latest` (default false).
    #[serde(default)]
    pub inclusive: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadRepliesParams {
    /// Slack channel ID containing the thread.
    pub channel_id: String,
    /// The PARENT message's ts (a reply's `thread_ts` field), copied
    /// verbatim from get_channel_history.
    pub thread_ts: String,
    /// Replies per page, 1..200 (default 100; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// `next_cursor` from the previous page. Omit for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UsersParams {
    /// Users per page, 1..200 (default 100; clamped). Always sent, so large
    /// workspaces never answer limit_required.
    #[serde(default)]
    pub limit: Option<i64>,
    /// `next_cursor` from the previous page. Omit for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UserProfileParams {
    /// Slack user ID such as U0123ABCD4 (from get_users) — never an @name.
    pub user_id: String,
    /// Also return custom profile field labels (default false — Slack rate
    /// limits this variant heavily).
    #[serde(default)]
    pub include_labels: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PostMessageParams {
    /// Slack channel ID to post to (from list_channels).
    pub channel_id: String,
    /// Message text in Slack mrkdwn (*bold*, _italic_, <@U123> mentions,
    /// <https://url|label> links); 1..40,000 characters, ideally under
    /// 4,000.
    pub text: String,
    /// Unfurl links in the text (Slack default: true for most links).
    #[serde(default)]
    pub unfurl_links: Option<bool>,
    /// Unfurl media in the text (Slack default: true).
    #[serde(default)]
    pub unfurl_media: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplyToThreadParams {
    /// Slack channel ID containing the thread.
    pub channel_id: String,
    /// The PARENT message's ts, copied verbatim from get_channel_history.
    pub thread_ts: String,
    /// Reply text in Slack mrkdwn; 1..40,000 characters.
    pub text: String,
    /// Also show the reply in the channel (default false).
    #[serde(default)]
    pub reply_broadcast: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddReactionParams {
    /// Slack channel ID containing the message.
    pub channel_id: String,
    /// The message's ts, copied verbatim from get_channel_history.
    pub timestamp: String,
    /// Emoji short name without colons, e.g. "thumbsup", "white_check_mark",
    /// "eyes" (surrounding colons are stripped if present).
    pub reaction: String,
}

#[tool_router]
impl TemplateServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Names of the tools this server exposes, read off the generated router
    /// so the discovery document (see [`crate::discovery`]) cannot drift from
    /// what `tools/list` actually returns.
    pub fn tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    /// Verifies the bot token (`auth.test`) and reports identity, granted
    /// scopes, and the local write policy.
    #[tool(
        description = "Verify the Slack bot token and report the workspace, bot user, granted \
                       scopes, SLACK_TEAM_ID match and the write policy (SLACK_READ_ONLY, \
                       SLACK_CHANNEL_IDS). Call this first; a missing/invalid result names the \
                       secret to fix — never retry it."
    )]
    #[tracing::instrument(name = "tool.check_auth", skip(self))]
    async fn check_auth(&self) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        if cfg.token.is_none() {
            return Ok(auth_report(
                &cfg,
                "missing",
                None,
                Error::MissingToken.message(),
                true,
            ));
        }
        match slack::post(&cfg, "auth.test", &json!({})).await {
            Ok(reply) => {
                let body = &reply.body;
                let token_team = str_field(body, "team_id");
                let configured = cfg.team_id.clone();
                let team_match = match (&configured, &token_team) {
                    (Some(want), Some(have)) => Some(want == have),
                    _ => None,
                };
                let missing_scopes: Vec<&str> = reply
                    .oauth_scopes
                    .as_deref()
                    .map(slack::missing_required_scopes)
                    .unwrap_or_default();
                let mut remediation = Vec::new();
                let mut status = "ok";
                if !missing_scopes.is_empty() {
                    status = "insufficient";
                    remediation.push(format!(
                        "token lacks required scopes {}: add them under OAuth & Permissions at {} and REINSTALL the app (scopes apply only after reinstall)",
                        missing_scopes.join(", "),
                        slack::APPS_URL
                    ));
                }
                if team_match == Some(false) {
                    remediation.push(format!(
                        "{} is {} but the token belongs to workspace {} ({}); set {}={} in the workload config or install the app in the intended workspace",
                        slack::TEAM_ID_ENV,
                        configured.as_deref().unwrap_or(""),
                        token_team.as_deref().unwrap_or("?"),
                        str_field(body, "team").unwrap_or_default(),
                        slack::TEAM_ID_ENV,
                        token_team.as_deref().unwrap_or("<team_id>")
                    ));
                } else if configured.is_none() {
                    remediation.push(format!(
                        "{} is not set; set it to {} so conversations.list/users.list are scoped to this workspace (required on Enterprise Grid)",
                        slack::TEAM_ID_ENV,
                        token_team.as_deref().unwrap_or("<team_id>")
                    ));
                }
                let identity = json!({
                    "team": str_field(body, "team"),
                    "team_id": token_team,
                    "url": str_field(body, "url"),
                    "user": str_field(body, "user"),
                    "user_id": str_field(body, "user_id"),
                    "bot_id": str_field(body, "bot_id"),
                    "enterprise_id": str_field(body, "enterprise_id"),
                });
                let mut report =
                    auth_report(&cfg, status, Some(identity), remediation.join(" | "), false);
                if let Some(obj) = report
                    .structured_content
                    .as_mut()
                    .and_then(Value::as_object_mut)
                {
                    obj.insert("team_id_match".into(), json!(team_match));
                    obj.insert(
                        "granted_scopes".into(),
                        json!(reply.oauth_scopes.as_deref().map(|s| {
                            s.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                        })),
                    );
                    obj.insert("missing_required_scopes".into(), json!(missing_scopes));
                }
                Ok(report)
            }
            Err(err @ Error::Api { .. }) => {
                let status = match err.code() {
                    Some("missing_scope") => "insufficient",
                    _ => "invalid",
                };
                Ok(auth_report(&cfg, status, None, err.message(), true))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Lists channels (`conversations.list`), or exactly the configured
    /// `SLACK_CHANNEL_IDS` via `conversations.info`.
    #[tool(
        description = "List Slack channels the bot can see (public by default; add private with \
                       types). Returns id, name, is_member, is_archived, member count, topic, \
                       purpose and next_cursor for paging. When SLACK_CHANNEL_IDS is configured, \
                       returns exactly those channels."
    )]
    #[tracing::instrument(name = "tool.list_channels", skip(self))]
    async fn list_channels(
        &self,
        Parameters(params): Parameters<ListChannelsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let cursor = match slack::cursor(params.cursor.as_deref()) {
            Ok(cursor) => cursor,
            Err(msg) => return Ok(refuse(msg)),
        };
        if !cfg.channel_ids.is_empty() {
            return Ok(self.list_configured_channels(&cfg).await);
        }
        let limit = slack::clamp_limit(params.limit, 100).to_string();
        let types = params
            .types
            .as_ref()
            .map_or("public_channel", ChannelTypes::as_str);
        let mut query: Vec<(&str, &str)> = vec![
            ("types", types),
            ("exclude_archived", "true"),
            ("limit", &limit),
        ];
        if let Some(team) = cfg.team_id.as_deref() {
            query.push(("team_id", team));
        }
        if let Some(cursor) = cursor.as_deref() {
            query.push(("cursor", cursor));
        }
        match slack::get(&cfg, "conversations.list", &query).await {
            Ok(reply) => {
                let channels: Vec<Value> = reply
                    .body
                    .get("channels")
                    .and_then(Value::as_array)
                    .map(|list| list.iter().map(channel_summary).collect())
                    .unwrap_or_default();
                let next_cursor = next_cursor(&reply.body);
                let text = format!(
                    "{} channel(s){}:\n{}",
                    channels.len(),
                    if next_cursor.is_empty() {
                        String::new()
                    } else {
                        " (more pages: pass next_cursor)".to_owned()
                    },
                    channels
                        .iter()
                        .map(|c| {
                            format!(
                                "- {} #{}{}{}{}",
                                c["id"].as_str().unwrap_or("?"),
                                c["name"].as_str().unwrap_or("?"),
                                if c["is_private"].as_bool().unwrap_or(false) {
                                    " (private)"
                                } else {
                                    ""
                                },
                                if c["is_member"].as_bool().unwrap_or(false) {
                                    " member"
                                } else {
                                    " not a member"
                                },
                                c["num_members"]
                                    .as_u64()
                                    .map(|n| format!(", {n} members"))
                                    .unwrap_or_default(),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                Ok(shaped(
                    text,
                    json!({
                        "channels": channels,
                        "count": channels.len(),
                        "next_cursor": next_cursor,
                        "types": types,
                        "source": "conversations.list",
                        "warning": reply.warning,
                    }),
                ))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Details for one channel (`conversations.info`).
    #[tool(
        description = "Get one Slack channel's details: name, is_member (whether the bot can read \
                       and post there), is_archived, is_private, member count, topic, purpose."
    )]
    #[tracing::instrument(name = "tool.get_channel_info", skip(self))]
    async fn get_channel_info(
        &self,
        Parameters(params): Parameters<ChannelParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let channel = match slack::channel_id(&params.channel_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        match channel_info(&cfg, &channel).await {
            Ok(summary) => {
                let text = format!(
                    "{} #{}: {}{}{}, {} members. Topic: {}. Purpose: {}",
                    summary["id"].as_str().unwrap_or("?"),
                    summary["name"].as_str().unwrap_or("?"),
                    if summary["is_private"].as_bool().unwrap_or(false) {
                        "private"
                    } else {
                        "public"
                    },
                    if summary["is_archived"].as_bool().unwrap_or(false) {
                        ", archived"
                    } else {
                        ""
                    },
                    if summary["is_member"].as_bool().unwrap_or(false) {
                        ", bot is a member"
                    } else {
                        ", bot is NOT a member (invite it or call join_channel)"
                    },
                    summary["num_members"].as_u64().unwrap_or(0),
                    summary["topic"].as_str().unwrap_or(""),
                    summary["purpose"].as_str().unwrap_or(""),
                );
                Ok(shaped(text, summary))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Recent messages in a channel (`conversations.history`), newest first.
    #[tool(
        description = "Read recent messages in a Slack channel, newest first, with cursor paging \
                       and an optional oldest/latest timestamp window. The bot must be a member \
                       (not_in_channel otherwise). Message ts values are strings — copy them \
                       verbatim for threads and reactions."
    )]
    #[tracing::instrument(name = "tool.get_channel_history", skip(self))]
    async fn get_channel_history(
        &self,
        Parameters(params): Parameters<ChannelHistoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let channel = match slack::channel_id(&params.channel_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        let cursor = match slack::cursor(params.cursor.as_deref()) {
            Ok(cursor) => cursor,
            Err(msg) => return Ok(refuse(msg)),
        };
        let oldest = match params
            .oldest
            .as_deref()
            .map(|t| slack::timestamp(t, "oldest"))
        {
            Some(Ok(ts)) => Some(ts),
            Some(Err(msg)) => return Ok(refuse(msg)),
            None => None,
        };
        let latest = match params
            .latest
            .as_deref()
            .map(|t| slack::timestamp(t, "latest"))
        {
            Some(Ok(ts)) => Some(ts),
            Some(Err(msg)) => return Ok(refuse(msg)),
            None => None,
        };
        let limit = slack::clamp_limit(params.limit, 10).to_string();
        let mut query: Vec<(&str, &str)> = vec![("channel", &channel), ("limit", &limit)];
        if let Some(cursor) = cursor.as_deref() {
            query.push(("cursor", cursor));
        }
        if let Some(oldest) = oldest.as_deref() {
            query.push(("oldest", oldest));
        }
        if let Some(latest) = latest.as_deref() {
            query.push(("latest", latest));
        }
        if params.inclusive.unwrap_or(false) {
            query.push(("inclusive", "true"));
        }
        match slack::get(&cfg, "conversations.history", &query).await {
            Ok(reply) => Ok(render_messages(
                &reply,
                &channel,
                "conversations.history",
                None,
            )),
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// All replies in a thread (`conversations.replies`), parent first.
    #[tool(
        description = "Read a Slack thread: the parent message followed by its replies, oldest \
                       first, with cursor paging. thread_ts is the PARENT message's ts (a \
                       reply's thread_ts field)."
    )]
    #[tracing::instrument(name = "tool.get_thread_replies", skip(self))]
    async fn get_thread_replies(
        &self,
        Parameters(params): Parameters<ThreadRepliesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let channel = match slack::channel_id(&params.channel_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        let thread_ts = match slack::timestamp(&params.thread_ts, "thread_ts") {
            Ok(ts) => ts,
            Err(msg) => return Ok(refuse(msg)),
        };
        let cursor = match slack::cursor(params.cursor.as_deref()) {
            Ok(cursor) => cursor,
            Err(msg) => return Ok(refuse(msg)),
        };
        let limit = slack::clamp_limit(params.limit, 100).to_string();
        let mut query: Vec<(&str, &str)> =
            vec![("channel", &channel), ("ts", &thread_ts), ("limit", &limit)];
        if let Some(cursor) = cursor.as_deref() {
            query.push(("cursor", cursor));
        }
        match slack::get(&cfg, "conversations.replies", &query).await {
            Ok(reply) => Ok(render_messages(
                &reply,
                &channel,
                "conversations.replies",
                Some(&thread_ts),
            )),
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Workspace members (`users.list`).
    #[tool(
        description = "List Slack workspace members (id, name, real name, display name, title, \
                       is_bot, deleted, timezone) with cursor paging. Use it to find the U… id \
                       for get_user_profile or a <@U…> mention."
    )]
    #[tracing::instrument(name = "tool.get_users", skip(self))]
    async fn get_users(
        &self,
        Parameters(params): Parameters<UsersParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let cursor = match slack::cursor(params.cursor.as_deref()) {
            Ok(cursor) => cursor,
            Err(msg) => return Ok(refuse(msg)),
        };
        let limit = slack::clamp_limit(params.limit, 100).to_string();
        let mut query: Vec<(&str, &str)> = vec![("limit", &limit)];
        if let Some(team) = cfg.team_id.as_deref() {
            query.push(("team_id", team));
        }
        if let Some(cursor) = cursor.as_deref() {
            query.push(("cursor", cursor));
        }
        match slack::get(&cfg, "users.list", &query).await {
            Ok(reply) => {
                let members: Vec<Value> = reply
                    .body
                    .get("members")
                    .and_then(Value::as_array)
                    .map(|list| list.iter().map(user_summary).collect())
                    .unwrap_or_default();
                let next_cursor = next_cursor(&reply.body);
                let text = format!(
                    "{} user(s){}:\n{}",
                    members.len(),
                    if next_cursor.is_empty() {
                        String::new()
                    } else {
                        " (more pages: pass next_cursor)".to_owned()
                    },
                    members
                        .iter()
                        .map(|u| {
                            format!(
                                "- {} @{} ({}){}{}",
                                u["id"].as_str().unwrap_or("?"),
                                u["name"].as_str().unwrap_or("?"),
                                u["real_name"].as_str().unwrap_or(""),
                                if u["is_bot"].as_bool().unwrap_or(false) {
                                    " bot"
                                } else {
                                    ""
                                },
                                if u["deleted"].as_bool().unwrap_or(false) {
                                    " deactivated"
                                } else {
                                    ""
                                },
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                Ok(shaped(
                    text,
                    json!({
                        "members": members,
                        "count": members.len(),
                        "next_cursor": next_cursor,
                        "warning": reply.warning,
                    }),
                ))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// One user's profile (`users.profile.get`).
    #[tool(
        description = "Get a Slack user's profile: real/display name, title, status text and \
                       emoji, timezone, pronouns, email (only with users:read.email) and avatar \
                       URL. Takes the U… id from get_users."
    )]
    #[tracing::instrument(name = "tool.get_user_profile", skip(self))]
    async fn get_user_profile(
        &self,
        Parameters(params): Parameters<UserProfileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let user = match slack::user_id(&params.user_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        let mut query: Vec<(&str, &str)> = vec![("user", &user)];
        if params.include_labels.unwrap_or(false) {
            query.push(("include_labels", "true"));
        }
        match slack::get(&cfg, "users.profile.get", &query).await {
            Ok(reply) => {
                let profile = reply.body.get("profile").cloned().unwrap_or(Value::Null);
                let summary = profile_summary(&user, &profile);
                let text = format!(
                    "{} — {} ({}){}{}{}",
                    user,
                    summary["real_name"].as_str().unwrap_or(""),
                    summary["display_name"].as_str().unwrap_or(""),
                    summary["title"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!(", {t}"))
                        .unwrap_or_default(),
                    summary["status_text"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!(", status: {t}"))
                        .unwrap_or_default(),
                    summary["email"]
                        .as_str()
                        .map(|e| format!(", {e}"))
                        .unwrap_or_default(),
                );
                Ok(shaped(text, summary))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Posts a new message (`chat.postMessage`). Gated write.
    #[tool(
        description = "Post a new message to a Slack channel (mrkdwn text). A write: refused when \
                       SLACK_READ_ONLY=true or the channel is outside SLACK_CHANNEL_IDS. Needs \
                       chat:write and bot membership (or chat:write.public)."
    )]
    #[tracing::instrument(name = "tool.post_message", skip(self))]
    async fn post_message(
        &self,
        Parameters(params): Parameters<PostMessageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let channel = match slack::channel_id(&params.channel_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        let (text, warning) = match slack::message_text(&params.text) {
            Ok(text) => text,
            Err(msg) => return Ok(refuse(msg)),
        };
        if let Err(msg) = write_gate(&cfg, &channel) {
            return Ok(refuse(msg));
        }
        let mut body = json!({ "channel": channel, "text": text });
        if let Some(v) = params.unfurl_links {
            body["unfurl_links"] = json!(v);
        }
        if let Some(v) = params.unfurl_media {
            body["unfurl_media"] = json!(v);
        }
        match slack::post(&cfg, "chat.postMessage", &body).await {
            Ok(reply) => Ok(render_posted(&reply, &channel, None, warning)),
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Replies in a thread (`chat.postMessage` with `thread_ts`). Gated write.
    #[tool(
        description = "Reply inside an existing Slack thread (thread_ts = the parent message's \
                       ts), optionally broadcasting to the channel. A write: refused when \
                       SLACK_READ_ONLY=true or the channel is outside SLACK_CHANNEL_IDS."
    )]
    #[tracing::instrument(name = "tool.reply_to_thread", skip(self))]
    async fn reply_to_thread(
        &self,
        Parameters(params): Parameters<ReplyToThreadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let channel = match slack::channel_id(&params.channel_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        let thread_ts = match slack::timestamp(&params.thread_ts, "thread_ts") {
            Ok(ts) => ts,
            Err(msg) => return Ok(refuse(msg)),
        };
        let (text, warning) = match slack::message_text(&params.text) {
            Ok(text) => text,
            Err(msg) => return Ok(refuse(msg)),
        };
        if let Err(msg) = write_gate(&cfg, &channel) {
            return Ok(refuse(msg));
        }
        let mut body = json!({ "channel": channel, "thread_ts": thread_ts, "text": text });
        if params.reply_broadcast.unwrap_or(false) {
            body["reply_broadcast"] = json!(true);
        }
        match slack::post(&cfg, "chat.postMessage", &body).await {
            Ok(reply) => Ok(render_posted(&reply, &channel, Some(&thread_ts), warning)),
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Adds an emoji reaction (`reactions.add`). Gated write; idempotent.
    #[tool(
        description = "Add an emoji reaction (short name like thumbsup, no colons) to a Slack \
                       message identified by channel_id + timestamp. already_reacted counts as \
                       success. A write: refused when SLACK_READ_ONLY=true or the channel is \
                       outside SLACK_CHANNEL_IDS."
    )]
    #[tracing::instrument(name = "tool.add_reaction", skip(self))]
    async fn add_reaction(
        &self,
        Parameters(params): Parameters<AddReactionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let channel = match slack::channel_id(&params.channel_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        let timestamp = match slack::timestamp(&params.timestamp, "timestamp") {
            Ok(ts) => ts,
            Err(msg) => return Ok(refuse(msg)),
        };
        let name = match slack::reaction_name(&params.reaction) {
            Ok(name) => name,
            Err(msg) => return Ok(refuse(msg)),
        };
        if let Err(msg) = write_gate(&cfg, &channel) {
            return Ok(refuse(msg));
        }
        let body = json!({ "channel": channel, "timestamp": timestamp, "name": name });
        let result = json!({
            "ok": true,
            "channel": channel,
            "timestamp": timestamp,
            "reaction": name,
        });
        match slack::post(&cfg, "reactions.add", &body).await {
            Ok(_) => Ok(shaped(
                format!("added :{name}: to message {timestamp} in {channel}"),
                result,
            )),
            Err(Error::Api { ref code, .. }) if code == "already_reacted" => {
                let mut value = result;
                value["already_reacted"] = json!(true);
                Ok(shaped(
                    format!(
                        ":{name}: was already on message {timestamp} in {channel} (nothing to do)"
                    ),
                    value,
                ))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Joins a public channel (`conversations.join`). Gated write.
    #[tool(
        description = "Make the bot join a public Slack channel so history and posting work \
                       there (fixes not_in_channel without a human /invite). Needs the \
                       channels:join scope; private channels and DMs cannot be joined this way. \
                       A write: refused when SLACK_READ_ONLY=true or the channel is outside \
                       SLACK_CHANNEL_IDS."
    )]
    #[tracing::instrument(name = "tool.join_channel", skip(self))]
    async fn join_channel(
        &self,
        Parameters(params): Parameters<ChannelParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = slack::config();
        let channel = match slack::channel_id(&params.channel_id) {
            Ok(id) => id,
            Err(msg) => return Ok(refuse(msg)),
        };
        if let Err(msg) = write_gate(&cfg, &channel) {
            return Ok(refuse(msg));
        }
        match slack::post(&cfg, "conversations.join", &json!({ "channel": channel })).await {
            Ok(reply) => {
                let summary = reply
                    .body
                    .get("channel")
                    .map(channel_summary)
                    .unwrap_or_else(|| json!({ "id": channel }));
                let already = reply.warning.as_deref() == Some("already_in_channel");
                let text = if already {
                    format!(
                        "bot was already a member of {} #{}",
                        channel,
                        summary["name"].as_str().unwrap_or("?")
                    )
                } else {
                    format!(
                        "bot joined {} #{}",
                        channel,
                        summary["name"].as_str().unwrap_or("?")
                    )
                };
                Ok(shaped(
                    text,
                    json!({
                        "ok": true,
                        "channel": summary,
                        "already_in_channel": already,
                        "warning": reply.warning,
                    }),
                ))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// `list_channels` in `SLACK_CHANNEL_IDS` mode: one `conversations.info`
    /// per configured id, archived channels dropped, no cursor.
    async fn list_configured_channels(&self, cfg: &Config) -> CallToolResult {
        let mut channels = Vec::with_capacity(cfg.channel_ids.len());
        let mut errors = Vec::new();
        for id in &cfg.channel_ids {
            let id = match slack::channel_id(id) {
                Ok(id) => id,
                Err(msg) => {
                    errors.push(format!("{id}: {msg}"));
                    continue;
                }
            };
            match channel_info(cfg, &id).await {
                Ok(summary) => {
                    if !summary["is_archived"].as_bool().unwrap_or(false) {
                        channels.push(summary);
                    }
                }
                // A bad token or a rate limit stops the loop: every further
                // call would fail the same way.
                Err(err @ Error::MissingToken)
                | Err(err @ Error::RateLimited { .. })
                | Err(err @ Error::Transport { .. }) => return tool_error(&err),
                Err(Error::Api { ref code, .. })
                    if matches!(
                        code.as_str(),
                        "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked"
                    ) =>
                {
                    return tool_error(&Error::Api {
                        method: "conversations.info".into(),
                        code: code.clone(),
                        needed: None,
                        provided: None,
                    });
                }
                Err(err) => errors.push(format!("{id}: {}", err.message())),
            }
        }
        let text = format!(
            "{} configured channel(s) from {}:\n{}{}",
            channels.len(),
            slack::CHANNEL_IDS_ENV,
            channels
                .iter()
                .map(|c| format!(
                    "- {} #{}",
                    c["id"].as_str().unwrap_or("?"),
                    c["name"].as_str().unwrap_or("?")
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            if errors.is_empty() {
                String::new()
            } else {
                format!("\nskipped: {}", errors.join("; "))
            },
        );
        shaped(
            text,
            json!({
                "channels": channels,
                "count": channels.len(),
                "next_cursor": "",
                "source": "conversations.info",
                "configured_ids": cfg.channel_ids,
                "skipped": errors,
            }),
        )
    }
}

/// The write gate shared by the four write tools: the token must be present
/// (so the missing-secret error wins), then the local fence applies.
fn write_gate(cfg: &Config, channel: &str) -> Result<(), String> {
    if cfg.token.is_none() {
        return Err(Error::MissingToken.message());
    }
    slack::write_fence(cfg, channel)
}

async fn channel_info(cfg: &Config, channel: &str) -> Result<Value, Error> {
    let reply = slack::get(
        cfg,
        "conversations.info",
        &[("channel", channel), ("include_num_members", "true")],
    )
    .await?;
    Ok(reply
        .body
        .get("channel")
        .map(channel_summary)
        .unwrap_or_else(|| json!({ "id": channel })))
}

/// A `check_auth` report. `is_error` marks the missing/invalid outcomes so a
/// client that ignores `structuredContent` still sees a failure.
fn auth_report(
    cfg: &Config,
    status: &str,
    identity: Option<Value>,
    remediation: String,
    is_error: bool,
) -> CallToolResult {
    let value = json!({
        "status": status,
        "identity": identity,
        "secret_ref": slack::SECRET_REF,
        "env": slack::TOKEN_ENV,
        "obtain_url": slack::MANIFEST_URL,
        "required_scopes": slack::REQUIRED_SCOPES,
        "configured_team_id": cfg.team_id,
        "read_only": cfg.read_only,
        "channel_fence": cfg.channel_ids,
        "remediation": if remediation.is_empty() { Value::Null } else { Value::String(remediation.clone()) },
    });
    let mut text = match status {
        "ok" => format!(
            "token ok: workspace {} ({}), bot user {} ({}). read_only={}, channel_fence={}",
            identity
                .as_ref()
                .and_then(|i| i["team"].as_str())
                .unwrap_or("?"),
            identity
                .as_ref()
                .and_then(|i| i["team_id"].as_str())
                .unwrap_or("?"),
            identity
                .as_ref()
                .and_then(|i| i["user"].as_str())
                .unwrap_or("?"),
            identity
                .as_ref()
                .and_then(|i| i["user_id"].as_str())
                .unwrap_or("?"),
            cfg.read_only,
            if cfg.channel_ids.is_empty() {
                "none".to_owned()
            } else {
                cfg.channel_ids.join(",")
            },
        ),
        other => format!("credential status: {other}"),
    };
    if !remediation.is_empty() {
        text.push_str(". ");
        text.push_str(&remediation);
    }
    let mut result = if is_error {
        CallToolResult::structured_error(value)
    } else {
        CallToolResult::structured(value)
    };
    result.content = vec![ContentBlock::text(text)];
    result
}

/// A shaped success: `structuredContent` plus a readable text fallback.
fn shaped(text: String, value: Value) -> CallToolResult {
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

/// A tool-level refusal (validation or policy) — no Slack call was made.
fn refuse(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

/// A tool-level error for a failed Slack exchange, with the classification
/// in `structuredContent` so agents can branch on `retryable`.
fn tool_error(err: &Error) -> CallToolResult {
    let mut result = CallToolResult::structured_error(json!({
        "error": err.code(),
        "retryable": err.retryable(),
        "message": err.message(),
    }));
    result.content = vec![ContentBlock::text(err.message())];
    result
}

fn str_field(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).map(str::to_owned)
}

fn next_cursor(body: &Value) -> String {
    body.get("response_metadata")
        .and_then(|m| m.get("next_cursor"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn short_text(value: &Value, limit: usize) -> Value {
    match value.as_str() {
        Some(s) => Value::String(slack::truncate_chars(s, limit).0),
        None => Value::Null,
    }
}

fn channel_summary(channel: &Value) -> Value {
    json!({
        "id": channel.get("id"),
        "name": channel.get("name"),
        "is_private": channel.get("is_private").and_then(Value::as_bool).unwrap_or(false),
        "is_member": channel.get("is_member").and_then(Value::as_bool).unwrap_or(false),
        "is_archived": channel.get("is_archived").and_then(Value::as_bool).unwrap_or(false),
        "is_general": channel.get("is_general").and_then(Value::as_bool).unwrap_or(false),
        "num_members": channel.get("num_members"),
        "created": channel.get("created"),
        "topic": short_text(channel.get("topic").and_then(|t| t.get("value")).unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "purpose": short_text(channel.get("purpose").and_then(|t| t.get("value")).unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
    })
}

fn user_summary(user: &Value) -> Value {
    let profile = user.get("profile").cloned().unwrap_or(Value::Null);
    json!({
        "id": user.get("id"),
        "name": user.get("name"),
        "real_name": short_text(user.get("real_name").or_else(|| profile.get("real_name")).unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "display_name": short_text(profile.get("display_name").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "title": short_text(profile.get("title").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "is_bot": user.get("is_bot").and_then(Value::as_bool).unwrap_or(false),
        "is_admin": user.get("is_admin").and_then(Value::as_bool).unwrap_or(false),
        "is_restricted": user.get("is_restricted").and_then(Value::as_bool).unwrap_or(false),
        "deleted": user.get("deleted").and_then(Value::as_bool).unwrap_or(false),
        "tz": user.get("tz"),
    })
}

fn profile_summary(user_id: &str, profile: &Value) -> Value {
    let avatar = profile
        .get("image_512")
        .or_else(|| profile.get("image_192"))
        .or_else(|| profile.get("image_72"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "user_id": user_id,
        "real_name": short_text(profile.get("real_name").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "display_name": short_text(profile.get("display_name").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "first_name": short_text(profile.get("first_name").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "last_name": short_text(profile.get("last_name").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "title": short_text(profile.get("title").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "status_text": short_text(profile.get("status_text").unwrap_or(&Value::Null), SHORT_TEXT_LIMIT),
        "status_emoji": short_text(profile.get("status_emoji").unwrap_or(&Value::Null), 100),
        "status_expiration": profile.get("status_expiration"),
        "pronouns": short_text(profile.get("pronouns").unwrap_or(&Value::Null), 100),
        "email": profile.get("email"),
        "phone": short_text(profile.get("phone").unwrap_or(&Value::Null), 100),
        "tz": profile.get("tz"),
        "avatar_url": avatar,
        "fields": profile.get("fields"),
    })
}

fn message_summary(message: &Value) -> Value {
    let (text, truncated) = message
        .get("text")
        .and_then(Value::as_str)
        .map(|t| slack::truncate_chars(t, MESSAGE_TEXT_LIMIT))
        .unwrap_or_default();
    let reactions: Vec<Value> = message
        .get("reactions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(50)
                .map(|r| json!({ "name": r.get("name"), "count": r.get("count") }))
                .collect()
        })
        .unwrap_or_default();
    let has = |key: &str| {
        message
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    };
    json!({
        "ts": message.get("ts"),
        "thread_ts": message.get("thread_ts"),
        "user": message.get("user"),
        "bot_id": message.get("bot_id"),
        "username": short_text(message.get("username").unwrap_or(&Value::Null), 100),
        "subtype": message.get("subtype"),
        "text": text,
        "text_truncated": truncated,
        "reply_count": message.get("reply_count"),
        "reply_users_count": message.get("reply_users_count"),
        "latest_reply": message.get("latest_reply"),
        "reactions": reactions,
        "has_files": has("files"),
        "has_attachments": has("attachments"),
        "has_blocks": has("blocks"),
        "edited": message.get("edited").is_some(),
    })
}

fn render_messages(
    reply: &slack::Reply,
    channel: &str,
    method: &str,
    thread_ts: Option<&str>,
) -> CallToolResult {
    let messages: Vec<Value> = reply
        .body
        .get("messages")
        .and_then(Value::as_array)
        .map(|list| list.iter().map(message_summary).collect())
        .unwrap_or_default();
    let has_more = reply
        .body
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next_cursor = next_cursor(&reply.body);
    let lines: Vec<String> = messages
        .iter()
        .map(|m| {
            let who = m["user"]
                .as_str()
                .map(|u| format!("<@{u}>"))
                .or_else(|| m["bot_id"].as_str().map(|b| format!("bot {b}")))
                .unwrap_or_else(|| "?".to_owned());
            format!(
                "[{}] {}{}: {}{}",
                m["ts"].as_str().unwrap_or("?"),
                who,
                m["reply_count"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .map(|n| format!(" (thread, {n} replies)"))
                    .unwrap_or_default(),
                m["text"].as_str().unwrap_or(""),
                if m["text_truncated"].as_bool().unwrap_or(false) {
                    " …[truncated]"
                } else {
                    ""
                },
            )
        })
        .collect();
    let order = if thread_ts.is_some() {
        "oldest first (parent is element 0)"
    } else {
        "newest first"
    };
    let text = format!(
        "{} message(s) in {}{} ({}){}:\n{}",
        messages.len(),
        channel,
        thread_ts
            .map(|t| format!(" thread {t}"))
            .unwrap_or_default(),
        order,
        if has_more || !next_cursor.is_empty() {
            ", more available: pass next_cursor"
        } else {
            ""
        },
        lines.join("\n")
    );
    shaped(
        text,
        json!({
            "channel": channel,
            "thread_ts": thread_ts,
            "messages": messages,
            "count": messages.len(),
            "has_more": has_more,
            "next_cursor": next_cursor,
            "order": order,
            "source": method,
            "warning": reply.warning,
        }),
    )
}

fn render_posted(
    reply: &slack::Reply,
    channel: &str,
    thread_ts: Option<&str>,
    warning: Option<String>,
) -> CallToolResult {
    let ts = str_field(&reply.body, "ts").unwrap_or_default();
    let posted_channel = str_field(&reply.body, "channel").unwrap_or_else(|| channel.to_owned());
    let permalink_hint = format!(
        "https://slack.com/archives/{posted_channel}/p{}{}",
        ts.replace('.', ""),
        thread_ts
            .map(|t| format!("?thread_ts={t}&cid={posted_channel}"))
            .unwrap_or_default()
    );
    let warnings: Vec<String> = warning.into_iter().chain(reply.warning.clone()).collect();
    let text = format!(
        "posted {} in {} at ts {}{}",
        if thread_ts.is_some() {
            "reply"
        } else {
            "message"
        },
        posted_channel,
        ts,
        if warnings.is_empty() {
            String::new()
        } else {
            format!(" (warning: {})", warnings.join("; "))
        },
    );
    shaped(
        text,
        json!({
            "ok": true,
            "channel": posted_channel,
            "ts": ts,
            "thread_ts": thread_ts,
            "permalink_hint": permalink_hint,
            "warnings": warnings,
        }),
    )
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TemplateServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                // Skills over MCP rides on the resources primitive: declaring
                // it is what makes `skill://` URIs discoverable at all.
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "Slack MCP server (bot token) running as a WebAssembly component on \
             Cosmonic Desktop. Reads: check_auth (call it first), list_channels, \
             get_channel_info, get_channel_history, get_thread_replies, get_users, \
             get_user_profile. Writes (may be fenced by SLACK_READ_ONLY / \
             SLACK_CHANNEL_IDS): post_message, reply_to_thread, add_reaction, \
             join_channel. Tools take Slack IDs (C…/U…) and verbatim ts strings, never \
             #names; there is no search (needs a user token).\n\n\
             This server publishes skills — playbooks describing when and how \
             to use its tools and what each Slack error means. Read `skill://index.json` \
             for the catalog, then `skill://slack-mcp/SKILL.md` before planning a \
             sequence of calls.",
        )
    }

    /// Skills over MCP: every skill file, plus the catalog, as resources.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(skills::resources()))
    }

    /// Parameterized `skill://` URIs, so a client can construct a skill
    /// request without having enumerated every resource first.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            skills::resource_templates(),
        ))
    }

    #[tracing::instrument(name = "resources.read", skip(self, _context), fields(uri = %request.uri))]
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let (mime_type, text) = skills::read(&request.uri).ok_or_else(|| {
            ErrorData::resource_not_found(
                format!(
                    "no resource at {}; read {} for the skills this server serves",
                    request.uri,
                    skills::INDEX_URI
                ),
                None,
            )
        })?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type(mime_type)
        ])
        .into())
    }
}
