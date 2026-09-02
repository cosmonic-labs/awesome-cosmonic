//! Slack Web API client.
//!
//! Everything that talks to `https://slack.com/api/<method>` lives here:
//! configuration from the environment, the bearer-token request shape, the
//! `{"ok": false, "error": "…"}` envelope Slack uses instead of HTTP status
//! codes, the HTTP 429 / `Retry-After` rate-limit path, id and timestamp
//! validators, and the local write fence (`SLACK_READ_ONLY`,
//! `SLACK_CHANNEL_IDS`). [`crate::server`] holds the tool definitions and
//! result rendering.
//!
//! Tool surface and env contract are ported from the archived official
//! reference server (`modelcontextprotocol/servers-archived`, `src/slack`,
//! MIT); the write fence follows `korotovsky/slack-mcp-server` (MIT).

use std::fmt::Write as _;

use bytes::Bytes;
use serde_json::{json, Value};

/// Env var carrying the Bot User OAuth Token (`xoxb-…`).
pub const TOKEN_ENV: &str = "SLACK_BOT_TOKEN";
/// Desktop secret reference that injects [`TOKEN_ENV`].
pub const SECRET_REF: &str = "slack-mcp-bot-token";
/// Workspace id (`T…`), forwarded as `team_id` to the list methods.
pub const TEAM_ID_ENV: &str = "SLACK_TEAM_ID";
/// Optional comma-separated channel allow-list (list shortcut + write fence).
pub const CHANNEL_IDS_ENV: &str = "SLACK_CHANNEL_IDS";
/// `true` disables every write tool without calling Slack.
pub const READ_ONLY_ENV: &str = "SLACK_READ_ONLY";
/// Upstream base URL override (tests point it at a local fixture).
pub const BASE_URL_ENV: &str = "SLACK_BASE_URL";
/// The real upstream.
pub const DEFAULT_BASE_URL: &str = "https://slack.com";
/// Where a Slack app (and its bot token) is created.
pub const APPS_URL: &str = "https://api.slack.com/apps";
/// Deep link that pre-fills a new Slack app with exactly the scopes below.
pub const MANIFEST_URL: &str = "https://api.slack.com/apps?new_app=1&manifest_json=%7B%22display_information%22%3A%7B%22name%22%3A%22Cosmonic%20Slack%20MCP%22%2C%22description%22%3A%22Slack%20MCP%20server%20on%20Cosmonic%20Desktop%20%28slack-mcp%29%22%7D%2C%22features%22%3A%7B%22bot_user%22%3A%7B%22display_name%22%3A%22cosmonic-mcp%22%2C%22always_online%22%3Afalse%7D%7D%2C%22oauth_config%22%3A%7B%22scopes%22%3A%7B%22bot%22%3A%5B%22channels%3Aread%22%2C%22channels%3Ahistory%22%2C%22channels%3Ajoin%22%2C%22groups%3Aread%22%2C%22groups%3Ahistory%22%2C%22chat%3Awrite%22%2C%22chat%3Awrite.public%22%2C%22reactions%3Awrite%22%2C%22users%3Aread%22%2C%22users.profile%3Aread%22%5D%7D%7D%2C%22settings%22%3A%7B%22org_deploy_enabled%22%3Afalse%2C%22socket_mode_enabled%22%3Afalse%2C%22token_rotation_enabled%22%3Afalse%7D%7D";

/// Bot scopes every tool of this server needs.
pub const REQUIRED_SCOPES: &[&str] = &[
    "channels:read",
    "channels:history",
    "chat:write",
    "reactions:write",
    "users:read",
    "users.profile:read",
];
/// Bot scopes that unlock optional behaviour (private channels, join, posting
/// without an invite).
pub const OPTIONAL_SCOPES: &[&str] = &[
    "groups:read",
    "groups:history",
    "channels:join",
    "chat:write.public",
];

/// Slack's documented `limit` range for the paginated methods used here.
pub const MIN_LIMIT: u32 = 1;
pub const MAX_LIMIT: u32 = 200;
/// `chat.postMessage` rejects longer text with `msg_too_long`.
pub const MAX_TEXT_CHARS: usize = 40_000;
/// Slack truncates rendering past this; the tools warn above it.
pub const TEXT_WARN_CHARS: usize = 4_000;
const MAX_CURSOR_LEN: usize = 1024;
const MAX_ID_LEN: usize = 32;
const MAX_TS_LEN: usize = 32;
const MAX_REACTION_LEN: usize = 100;
/// Upper bound on the `SLACK_CHANNEL_IDS` list (each id costs one
/// `conversations.info` call in `list_channels`).
const MAX_FENCE_IDS: usize = 100;
/// Longest slice of upstream text echoed into an error message.
const SNIPPET_CHARS: usize = 200;

/// Runtime configuration, read from the environment on every call (cheap,
/// and it keeps the instance free of state that could go stale).
#[derive(Debug, Clone)]
pub struct Config {
    pub token: Option<String>,
    pub team_id: Option<String>,
    pub channel_ids: Vec<String>,
    pub read_only: bool,
    pub base_url: String,
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

pub fn config() -> Config {
    let read_only = non_empty_env(READ_ONLY_ENV)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    let channel_ids = non_empty_env(CHANNEL_IDS_ENV)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .take(MAX_FENCE_IDS)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let base_url = non_empty_env(BASE_URL_ENV)
        .map(|v| v.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    Config {
        token: non_empty_env(TOKEN_ENV),
        team_id: non_empty_env(TEAM_ID_ENV),
        channel_ids,
        read_only,
        base_url,
    }
}

/// The `credentials` block of the `GET /` discovery document (presence only,
/// never values).
pub fn credentials() -> Value {
    let status = if non_empty_env(TOKEN_ENV).is_some() {
        "configured"
    } else {
        "missing"
    };
    json!([{
        "ref": SECRET_REF,
        "env": TOKEN_ENV,
        "kind": "bearer-token",
        "status": status,
        "description": "Bot User OAuth Token (xoxb-…) of an internal Slack app installed in the workspace; the bot only sees channels it is a member of",
        "obtainUrl": MANIFEST_URL,
        "scopes": REQUIRED_SCOPES,
        "optionalScopes": OPTIONAL_SCOPES,
        "validate": "check_auth",
    }])
}

/// The one-paragraph setup instruction shared by the missing-token and
/// invalid-token errors.
pub fn setup_hint() -> String {
    format!(
        "Create an internal Slack app at {APPS_URL} (the pre-filled manifest link \
         {MANIFEST_URL} adds the right scopes), grant the bot scopes {scopes}, click \
         'Install to Workspace', copy the 'Bot User OAuth Token' (xoxb-…) from OAuth & \
         Permissions, and register it as the `{SECRET_REF}` secret (env {TOKEN_ENV}): \
         paste it in Cosmonic Desktop → Secrets, or run cosmonic_set_secret \
         name={SECRET_REF} uri=keychain://cosmonic/{SECRET_REF} env={TOKEN_ENV} \
         value=<xoxb-…>. Then call check_auth.",
        scopes = REQUIRED_SCOPES.join(", "),
    )
}

/// A failed Slack exchange, already classified so tools can render one
/// actionable message and agents can tell retryable from permanent.
#[derive(Debug)]
pub enum Error {
    /// `SLACK_BOT_TOKEN` unset or empty — nothing was sent.
    MissingToken,
    /// HTTP 2xx with `{"ok": false, "error": code}`.
    Api {
        method: String,
        code: String,
        needed: Option<String>,
        provided: Option<String>,
    },
    /// HTTP 429, or `error: ratelimited` in the body.
    RateLimited {
        method: String,
        retry_after: Option<u64>,
    },
    /// HTTP 5xx or another non-2xx without a Slack envelope.
    Upstream {
        method: String,
        status: u16,
        snippet: String,
    },
    /// The host could not complete the exchange (DNS, TLS, policy, timeout).
    Transport { method: String, detail: String },
    /// A 2xx whose body is not the Slack JSON envelope.
    Malformed { method: String, detail: String },
}

impl Error {
    /// Slack's error code, when there is one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api { code, .. } => Some(code),
            Error::RateLimited { .. } => Some("ratelimited"),
            _ => None,
        }
    }

    /// Whether waiting and retrying could succeed without a human acting.
    pub fn retryable(&self) -> bool {
        match self {
            Error::RateLimited { .. } | Error::Upstream { .. } | Error::Transport { .. } => true,
            Error::Api { code, .. } => {
                matches!(
                    code.as_str(),
                    "internal_error" | "fatal_error" | "service_unavailable"
                )
            }
            Error::MissingToken | Error::Malformed { .. } => false,
        }
    }

    /// The caller-facing message: what happened and what to do about it.
    pub fn message(&self) -> String {
        match self {
            Error::MissingToken => format!("{TOKEN_ENV} is not set. {}", setup_hint()),
            Error::Api {
                method,
                code,
                needed,
                provided,
            } => api_message(method, code, needed.as_deref(), provided.as_deref()),
            Error::RateLimited {
                method,
                retry_after,
            } => {
                let wait = retry_after
                    .map(|s| format!("; Retry-After: {s} s — wait at least {s} seconds"))
                    .unwrap_or_else(|| "; wait about a minute".to_owned());
                format!(
                    "Slack rate limited {method} (ratelimited, HTTP 429){wait} before retrying, \
                     never in a tight loop, and reduce limit/page size. Tiers: \
                     conversations.list/users.list 20 req/min, history/replies/reactions \
                     50 req/min, chat.postMessage about 1 message/s per channel; unlisted \
                     commercially-distributed apps get 1 req/min and 15 items on \
                     history/replies (internal apps are exempt)."
                )
            }
            Error::Upstream {
                method,
                status,
                snippet,
            } => {
                let tail = if snippet.is_empty() {
                    String::new()
                } else {
                    format!(": {snippet}")
                };
                format!(
                    "Slack {method} returned HTTP {status} (transient upstream failure — \
                     retry once after a few seconds){tail}"
                )
            }
            Error::Transport { method, detail } => format!(
                "could not reach Slack for {method}: {detail}. If this mentions \
                 HttpRequestDenied, the workload's allowedHosts must include slack.com."
            ),
            Error::Malformed { method, detail } => format!(
                "Slack {method} returned something that is not the Slack JSON envelope: \
                 {detail}. Check {BASE_URL_ENV} (currently the upstream base) and retry once."
            ),
        }
    }
}

fn api_message(method: &str, code: &str, needed: Option<&str>, provided: Option<&str>) -> String {
    let head = format!("Slack {method} failed: {code}");
    match code {
        "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" | "token_expired"
        | "invalid_token" => {
            let meaning = match code {
                "account_inactive" => "the app was uninstalled or the workspace/user deactivated",
                "token_revoked" | "token_expired" => "the token was revoked or has expired",
                "not_authed" => "no token reached Slack",
                _ => "the token is wrong or belongs to another workspace",
            };
            format!(
                "{head} — Slack rejected the bot token ({meaning}). Update the `{SECRET_REF}` \
                 secret (env {TOKEN_ENV}) with the current Bot User OAuth Token. {}",
                setup_hint()
            )
        }
        "missing_scope" => format!(
            "{head} — needs scope {needed}, token has {provided}. Add the scope under OAuth & \
             Permissions → Bot Token Scopes at {APPS_URL}, then REINSTALL the app to the \
             workspace (new scopes only apply after reinstall), then retry.",
            needed = needed.unwrap_or("<unreported>"),
            provided = provided.unwrap_or("<unreported>"),
        ),
        "not_in_channel" => format!(
            "{head} — the bot is not a member of this channel. Invite it in Slack \
             (/invite @<app name>), call join_channel (public channels only; needs the \
             channels:join scope), or grant chat:write.public to post without joining. \
             get_channel_info reports is_member before you try."
        ),
        "channel_not_found" => format!(
            "{head} — no channel with that ID is visible to the bot. Use the C… id from \
             list_channels (never a #name); private channels need the bot invited and the \
             groups:read/groups:history scopes; DMs are out of scope for this server."
        ),
        "is_archived" => format!(
            "{head} — the channel is archived, so writes and reactions are rejected. Do not \
             retry; pick another channel or ask an admin to unarchive it."
        ),
        "thread_not_found" | "message_not_found" | "bad_timestamp" | "no_such_thread" => {
            format!(
                "{head} — the timestamp does not identify a message in this channel. Copy the \
                 exact ts string from get_channel_history in the same channel (the parent's \
                 ts for threads); never round it through a float."
            )
        }
        "invalid_name" | "too_many_emoji" | "too_many_reactions" => format!(
            "{head} — the emoji name is unknown to this workspace or the reaction cap was \
             reached. Use a standard short name without colons (thumbsup, \
             white_check_mark, eyes) or a custom emoji name from the workspace."
        ),
        "already_reacted" => format!("{head} — the bot already added that reaction (idempotent)."),
        "invalid_cursor" => format!(
            "{head} — the pagination cursor expired or came from another method. Restart from \
             the first page (omit cursor) and never reuse cursors across tools."
        ),
        "limit_required" => format!(
            "{head} — Slack demands a limit on this workspace. This server always sends one, \
             so please report this as a bug in slack-mcp."
        ),
        "invalid_types" => format!(
            "{head} — the token cannot list that channel type (private channels need the \
             groups:read scope plus a reinstall). Retry with types=public_channel."
        ),
        "user_not_found" | "users_not_found" => format!(
            "{head} — no user with that ID. Get the U… id from get_users (deactivated users \
             still appear there with deleted=true)."
        ),
        "no_text" | "msg_too_long" => format!(
            "{head} — the message text is empty or too long. Keep text between 1 and \
             {MAX_TEXT_CHARS} characters and split long content into thread replies."
        ),
        "restricted_action"
        | "ekm_access_denied"
        | "method_not_supported_for_channel_type"
        | "restricted_action_read_only_channel"
        | "restricted_action_thread_only_channel"
        | "not_allowed_token_type" => format!(
            "{head} — workspace policy forbids this action here (admin restriction, enterprise \
             key management, or join on a private/DM channel). Not retryable; report the \
             channel id to the user."
        ),
        "internal_error" | "fatal_error" | "service_unavailable" => {
            format!("{head} — Slack-side transient failure. Retry once after a few seconds.")
        }
        "ratelimited" => {
            format!("{head} — wait about a minute before retrying and reduce limit/page size.")
        }
        _ => format!(
            "{head}. See https://docs.slack.dev/reference/methods/{method} for the error \
             code's meaning; do not retry blindly."
        ),
    }
}

/// A successful Slack reply: the JSON body plus the response headers that
/// matter to callers.
#[derive(Debug)]
pub struct Reply {
    pub body: Value,
    /// `X-OAuth-Scopes`: the scopes granted to the token, comma-separated.
    pub oauth_scopes: Option<String>,
    /// Slack's `warning` field (`missing_charset`, `already_in_channel`, …).
    pub warning: Option<String>,
}

/// `GET /api/<method>?<params>` with the bearer token.
pub async fn get(cfg: &Config, method: &str, params: &[(&str, &str)]) -> Result<Reply, Error> {
    let token = cfg.token.as_deref().ok_or(Error::MissingToken)?;
    let mut url = format!("{}/api/{method}", cfg.base_url);
    let mut first = true;
    for (key, value) in params {
        url.push(if first { '?' } else { '&' });
        first = false;
        url.push_str(key);
        url.push('=');
        url.push_str(&percent_encode(value));
    }
    let request = http::Request::get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("User-Agent", user_agent())
        .body(Bytes::new())
        .map_err(|err| Error::Transport {
            method: method.to_owned(),
            detail: format!("could not build request: {err}"),
        })?;
    call(method, request).await
}

/// `POST /api/<method>` with a JSON body and the bearer token. Slack needs
/// the `charset=utf-8` on JSON posts or it answers with `warning:
/// missing_charset`.
pub async fn post(cfg: &Config, method: &str, body: &Value) -> Result<Reply, Error> {
    let token = cfg.token.as_deref().ok_or(Error::MissingToken)?;
    let url = format!("{}/api/{method}", cfg.base_url);
    let payload = Bytes::from(body.to_string());
    let request = http::Request::post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Content-Length", payload.len())
        .header("Accept", "application/json")
        .header("User-Agent", user_agent())
        .body(payload)
        .map_err(|err| Error::Transport {
            method: method.to_owned(),
            detail: format!("could not build request: {err}"),
        })?;
    call(method, request).await
}

fn user_agent() -> String {
    format!(
        "{}/{} (Cosmonic Desktop; +https://github.com/cosmonic-labs/awesome-cosmonic)",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    )
}

async fn call(method: &str, request: http::Request<Bytes>) -> Result<Reply, Error> {
    let response = crate::bridge::outbound::fetch(request)
        .await
        .map_err(|err| Error::Transport {
            method: method.to_owned(),
            detail: err.to_string(),
        })?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let retry_after = header("retry-after").and_then(|v| v.trim().parse::<u64>().ok());
    let oauth_scopes = header("x-oauth-scopes");
    if status == 429 {
        return Err(Error::RateLimited {
            method: method.to_owned(),
            retry_after,
        });
    }
    let body_bytes = response.body();
    if !(200..300).contains(&status) {
        return Err(Error::Upstream {
            method: method.to_owned(),
            status,
            snippet: snippet(body_bytes),
        });
    }
    let body: Value = serde_json::from_slice(body_bytes).map_err(|err| Error::Malformed {
        method: method.to_owned(),
        detail: format!("{err} (body starts with: {})", snippet(body_bytes)),
    })?;
    let ok = body.get("ok").and_then(Value::as_bool);
    let warning = body
        .get("warning")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match ok {
        Some(true) => Ok(Reply {
            body,
            oauth_scopes,
            warning,
        }),
        Some(false) => {
            let code = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_owned();
            if code == "ratelimited" {
                return Err(Error::RateLimited {
                    method: method.to_owned(),
                    retry_after,
                });
            }
            let field = |name: &str| body.get(name).and_then(Value::as_str).map(str::to_owned);
            Err(Error::Api {
                method: method.to_owned(),
                code: truncate_chars(&code, 64).0,
                needed: field("needed"),
                provided: field("provided"),
            })
        }
        None => Err(Error::Malformed {
            method: method.to_owned(),
            detail: "no `ok` field".to_owned(),
        }),
    }
}

fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let (cut, _) = truncate_chars(text.trim(), SNIPPET_CHARS);
    cut.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Percent-encodes a query value: everything but RFC 3986 unreserved
/// characters is escaped, so user-supplied cursors and timestamps can never
/// smuggle a second parameter.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                // Writing to a String cannot fail.
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Cuts `s` to at most `max_chars` characters (never mid-code-point) and
/// reports whether anything was dropped.
pub fn truncate_chars(s: &str, max_chars: usize) -> (String, bool) {
    match s.char_indices().nth(max_chars) {
        Some((index, _)) => (s[..index].to_owned(), true),
        None => (s.to_owned(), false),
    }
}

/// Clamps a client-supplied `limit` into Slack's documented range.
pub fn clamp_limit(requested: Option<i64>, default: u32) -> u32 {
    let value = requested.unwrap_or(i64::from(default));
    let value = value.clamp(i64::from(MIN_LIMIT), i64::from(MAX_LIMIT));
    // In range by construction; the fallback only guards the cast.
    u32::try_from(value).unwrap_or(default)
}

/// A short, control-character-free echo of client input for error text.
fn short(raw: &str) -> String {
    let (cut, more) = truncate_chars(raw.trim(), 48);
    let mut out: String = cut
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if more {
        out.push('…');
    }
    out
}

fn validate_slack_id(
    raw: &str,
    what: &str,
    prefixes: &[char],
    example: &str,
    lookup: &str,
) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err(format!("{what} is required (a Slack ID like {example})"));
    }
    if id.starts_with('#') || id.starts_with('@') {
        return Err(format!(
            "{what} must be a Slack ID like {example}, not a name ({}); call {lookup} and copy the id",
            short(id)
        ));
    }
    let mut chars = id.chars();
    let valid = id.len() <= MAX_ID_LEN
        && chars.next().is_some_and(|c| prefixes.contains(&c))
        && id.len() >= 9
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !valid {
        return Err(format!(
            "{what} `{}` is not a Slack ID (expected {} followed by 8 or more uppercase letters or digits, like {example}); call {lookup} and copy the id",
            short(id),
            prefixes
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join("/"),
        ));
    }
    Ok(id.to_owned())
}

/// Validates a channel id (`C…` public, `G…` legacy private, `D…` DM).
pub fn channel_id(raw: &str) -> Result<String, String> {
    validate_slack_id(
        raw,
        "channel_id",
        &['C', 'G', 'D'],
        "C0123ABCD4",
        "list_channels",
    )
}

/// Validates a user id (`U…`, or `W…` on Enterprise Grid).
pub fn user_id(raw: &str) -> Result<String, String> {
    validate_slack_id(raw, "user_id", &['U', 'W'], "U0123ABCD4", "get_users")
}

/// Validates a Slack message timestamp (`1712345678.123456`), kept verbatim.
pub fn timestamp(raw: &str, what: &str) -> Result<String, String> {
    let ts = raw.trim();
    let (whole, frac) = ts.split_once('.').unwrap_or((ts, "0"));
    let digits = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
    };
    if ts.is_empty() || ts.len() > MAX_TS_LEN || !digits(whole, 16) || !digits(frac, 9) {
        return Err(format!(
            "{what} `{}` is not a Slack message timestamp (expected digits like 1712345678.123456, copied verbatim from get_channel_history)",
            short(ts)
        ));
    }
    Ok(ts.to_owned())
}

/// Validates an optional pagination cursor; an empty cursor means page 1.
pub fn cursor(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(cursor) = raw.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    if cursor.len() > MAX_CURSOR_LEN {
        return Err(format!(
            "cursor is {} bytes; Slack cursors are short opaque strings (max {MAX_CURSOR_LEN} here) — pass next_cursor back verbatim",
            cursor.len()
        ));
    }
    if !cursor.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err(
            "cursor contains whitespace or non-ASCII characters; pass next_cursor back verbatim"
                .to_owned(),
        );
    }
    Ok(Some(cursor.to_owned()))
}

/// Validates an emoji short name: surrounding colons stripped, lowercase
/// `[a-z0-9_+-]` with an optional `::skin-tone-N` suffix.
pub fn reaction_name(raw: &str) -> Result<String, String> {
    let name = raw.trim().trim_matches(':');
    let (base, tone) = name.split_once("::").unwrap_or((name, ""));
    let base_ok = !base.is_empty()
        && base.len() <= MAX_REACTION_LEN
        && base.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-')
        });
    let tone_ok = tone.is_empty()
        || tone
            .strip_prefix("skin-tone-")
            .is_some_and(|n| matches!(n, "2" | "3" | "4" | "5" | "6"));
    if !base_ok || !tone_ok {
        return Err(format!(
            "reaction `{}` is not an emoji short name (lowercase letters, digits, _ + -, optional ::skin-tone-2..6; no colons needed), e.g. thumbsup, white_check_mark, eyes",
            short(raw)
        ));
    }
    Ok(name.to_owned())
}

/// Validates message text: 1..=40,000 characters. Returns a warning when the
/// text is longer than Slack renders comfortably.
pub fn message_text(raw: &str) -> Result<(String, Option<String>), String> {
    let count = raw.chars().count();
    if raw.trim().is_empty() {
        return Err("text is required (Slack rejects empty messages with no_text)".to_owned());
    }
    if count > MAX_TEXT_CHARS {
        return Err(format!(
            "text is {count} characters; Slack rejects messages over {MAX_TEXT_CHARS} (msg_too_long) — split it into several messages or thread replies"
        ));
    }
    let warning = (count > TEXT_WARN_CHARS).then(|| {
        format!(
            "text is {count} characters; Slack truncates messages past {TEXT_WARN_CHARS} in most views — consider splitting"
        )
    });
    Ok((raw.to_owned(), warning))
}

/// The local write fence: `SLACK_READ_ONLY=true` refuses every write, and a
/// non-empty `SLACK_CHANNEL_IDS` restricts writes to those channels. Neither
/// path calls Slack.
pub fn write_fence(cfg: &Config, channel: &str) -> Result<(), String> {
    if cfg.read_only {
        return Err(format!(
            "writes disabled by {READ_ONLY_ENV}=true on this deployment (no Slack call was made). Do not retry; ask the operator to change the workload config if the write is intended."
        ));
    }
    if !cfg.channel_ids.is_empty() && !cfg.channel_ids.iter().any(|id| id == channel) {
        return Err(format!(
            "channel {channel} is not in {CHANNEL_IDS_ENV} ({}); writes are fenced to those channels and no Slack call was made. Do not retry; ask the operator to extend the list if the write is intended.",
            cfg.channel_ids.join(",")
        ));
    }
    Ok(())
}

/// Scopes from `X-OAuth-Scopes` that are missing from [`REQUIRED_SCOPES`].
pub fn missing_required_scopes(granted: &str) -> Vec<&'static str> {
    let have: Vec<&str> = granted.split(',').map(str::trim).collect();
    REQUIRED_SCOPES
        .iter()
        .copied()
        .filter(|scope| !have.contains(scope))
        .collect()
}
