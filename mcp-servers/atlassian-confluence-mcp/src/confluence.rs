//! Confluence Cloud REST API client.
//!
//! Configuration from the environment (named config + the shared
//! `atlassian-api-token` secret), HTTP Basic authentication, request plumbing
//! over the [`crate::bridge::outbound`] bridge, upstream error mapping, and
//! the small result shapers the tools use.
//!
//! Two API generations are involved:
//!
//! - **v2** (`/wiki/api/v2/...`) for pages, spaces, comments, labels and
//!   attachments — cursor paginated, addressed by numeric ids;
//! - **v1** (`/wiki/rest/api/...`) for the three operations v2 still lacks:
//!   CQL search, the current user, and adding labels.
//!
//! Two routes exist for the same API:
//!
//! - classic API tokens: `https://<site>/wiki/...`
//! - "API tokens with scopes": `https://api.atlassian.com/ex/confluence/<cloudId>/wiki/...`
//!
//! [`Config::from_env`] picks the route from `ATLASSIAN_CLOUD_ID`.
//! `ATLASSIAN_BASE_URL` overrides the *origin* (the e2e points it at a local
//! fixture); the `/ex/confluence/<cloudId>` prefix still applies when a cloud
//! id is set, so the gateway route is testable hermetically.

use base64::Engine as _;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Value};

use crate::body;

/// Secret: the Atlassian API token (Basic-auth password).
pub const TOKEN_ENV: &str = "ATLASSIAN_API_TOKEN";
/// Named config: Atlassian Cloud site, `acme` or `acme.atlassian.net`.
pub const SITE_ENV: &str = "ATLASSIAN_SITE";
/// Named config: the Atlassian account email owning the token.
pub const EMAIL_ENV: &str = "ATLASSIAN_EMAIL";
/// Named config (optional): cloud id, required for scoped tokens.
pub const CLOUD_ID_ENV: &str = "ATLASSIAN_CLOUD_ID";
/// Test override: replaces the computed origin.
pub const BASE_URL_ENV: &str = "ATLASSIAN_BASE_URL";
/// Named config (optional): `true` refuses every write tool.
pub const READ_ONLY_ENV: &str = "CONFLUENCE_READ_ONLY";
/// Named config (optional): `true` enables `delete_page`.
pub const ALLOW_DELETE_ENV: &str = "CONFLUENCE_ALLOW_DELETE";
/// Named config (optional): comma-separated space keys to scope to.
pub const SPACES_FILTER_ENV: &str = "CONFLUENCE_SPACES_FILTER";
/// The Desktop secret reference that injects [`TOKEN_ENV`]. Deliberately
/// shared with `atlassian-jira-mcp` (one ref per Atlassian account).
pub const SECRET_REF: &str = "atlassian-api-token";
/// Where a user creates an API token.
pub const TOKEN_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";
/// Origin of the scoped-token gateway.
pub const GATEWAY_ORIGIN: &str = "https://api.atlassian.com";
/// Scopes a scoped token needs for this server's tools (granular v2 scopes
/// plus the classic ones the v1 search/user/label endpoints still check).
pub const SCOPES: &[&str] = &[
    "read:page:confluence",
    "write:page:confluence",
    "delete:page:confluence",
    "read:space:confluence",
    "read:comment:confluence",
    "write:comment:confluence",
    "read:label:confluence",
    "write:label:confluence",
    "read:attachment:confluence",
    "read:user:confluence",
    "read:confluence-user",
    "search:confluence",
    "write:confluence-content",
];

const USER_AGENT: &str = concat!("atlassian-confluence-mcp/", env!("CARGO_PKG_VERSION"));
/// Longest slice of a non-JSON upstream error body carried into a tool error.
const MAX_ERROR_BODY_CHARS: usize = 1024;
/// A 429 whose `Retry-After` is at most this many seconds is retried once,
/// in-process; longer waits are handed back to the caller.
const MAX_INLINE_RETRY_SECONDS: u64 = 5;
/// Largest request body the write tools send (bytes). Confluence rejects
/// bodies over 5 MB with 413; staying under 4 MiB leaves room for the JSON
/// envelope.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Percent-encoding set for query values: everything but unreserved chars.
const QUERY_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// Reads an environment variable, trimmed; `None` when unset or blank.
pub fn env_setting(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Boolean named config: `true`/`1`/`yes`/`on` (case-insensitive).
pub fn env_flag(name: &str) -> bool {
    env_setting(name).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
    })
}

/// Whether `CONFLUENCE_READ_ONLY` refuses the write tools.
pub fn read_only() -> bool {
    env_flag(READ_ONLY_ENV)
}

/// Whether `CONFLUENCE_ALLOW_DELETE` enables `delete_page`.
pub fn allow_delete() -> bool {
    env_flag(ALLOW_DELETE_ENV)
}

/// The `CONFLUENCE_SPACES_FILTER` keys (deduplicated, validated).
pub fn spaces_filter() -> Result<Vec<String>, Error> {
    let Some(raw) = env_setting(SPACES_FILTER_ENV) else {
        return Ok(Vec::new());
    };
    let mut keys: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let key = part.trim().to_owned();
        if key.is_empty() {
            continue;
        }
        if !is_space_key(&key) {
            return Err(Error::Config(format!(
                "{SPACES_FILTER_ENV} contains an invalid space key {key:?}; \
                 use comma-separated keys such as ENG,DOCS"
            )));
        }
        if !keys
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&key))
        {
            keys.push(key);
        }
    }
    Ok(keys)
}

// ---------------------------------------------------------------------------
// Validators (ids go into URL paths — never interpolate unchecked input)
// ---------------------------------------------------------------------------

/// A numeric Confluence id (page, space, comment, attachment, version).
pub fn is_numeric_id(s: &str) -> bool {
    (1..=20).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_digit())
}

/// Confluence space key: `ENG`, `DOCS2`, personal `~5b10ac8d82e05b22cc7d4ef5`
/// or `~712020:uuid`. Letters, digits and `_ ~ : . -`.
pub fn is_space_key(s: &str) -> bool {
    (1..=255).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'~' | b':' | b'.' | b'-'))
        && !s.starts_with('-')
}

/// Accepts a numeric page id, or a Confluence page URL and extracts the id
/// from `/pages/<id>` or `pageId=<id>`.
pub fn parse_page_id(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("page_id is required: a numeric page id or a Confluence page URL".to_owned());
    }
    if is_numeric_id(value) {
        return Ok(value.to_owned());
    }
    // `.../pages/123456/Title`, `.../pages/123456`, `.../pages/edit-v2/123456`.
    if let Some(index) = value.find("/pages/") {
        let rest = &value[index + "/pages/".len()..];
        let rest = rest.strip_prefix("edit-v2/").unwrap_or(rest);
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if is_numeric_id(&digits) {
            return Ok(digits);
        }
    }
    // Legacy `viewpage.action?pageId=123456`.
    if let Some(index) = value.find("pageId=") {
        let digits: String = value[index + "pageId=".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if is_numeric_id(&digits) {
            return Ok(digits);
        }
    }
    let shown: String = value.chars().take(80).collect();
    Err(format!(
        "page_id {shown:?} is neither a numeric page id nor a page URL containing /pages/<id>; \
         find the id with search or list_pages (v2 ids are numeric, titles are not ids)"
    ))
}

/// Strips a scheme, path and trailing slashes from a site value, appends
/// `.atlassian.net` to a bare tenant name, and checks it is a hostname.
fn normalize_site(raw: &str) -> Result<String, Error> {
    let mut site = raw.trim();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = site.strip_prefix(scheme) {
            site = rest;
        }
    }
    let mut site = site
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !site.contains('.') && !site.is_empty() {
        site.push_str(".atlassian.net");
    }
    let valid = (1..=253).contains(&site.len())
        && site
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        && !site.starts_with('.')
        && !site.ends_with('.');
    if !valid {
        return Err(Error::Config(format!(
            "{SITE_ENV}={raw:?} is not a hostname; set it to your Atlassian Cloud site such as \
             acme or acme.atlassian.net (no scheme, no path)"
        )));
    }
    Ok(site)
}

/// Whether a cloud id looks like one (a UUID in practice).
pub fn is_cloud_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Everything the client needs, resolved from the environment once per tool
/// call (instances are stateless; reading a few env vars is cheap).
#[derive(Clone)]
pub struct Config {
    /// Site host, e.g. `acme.atlassian.net` (used for browse URLs and hints).
    pub site: String,
    /// Basic-auth user.
    pub email: String,
    /// Basic-auth password — never logged, never echoed.
    token: String,
    /// Set for scoped tokens: routes through the gateway.
    pub cloud_id: Option<String>,
    /// Origin plus optional `/ex/confluence/<cloudId>`; no trailing slash.
    pub base_url: String,
    /// `CONFLUENCE_SPACES_FILTER` keys, possibly empty.
    pub spaces_filter: Vec<String>,
}

/// Manual `Debug` so the API token can never reach a log line or an error
/// message, whatever formats the config.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("site", &self.site)
            .field("email", &self.email)
            .field("token", &"<redacted>")
            .field("cloud_id", &self.cloud_id)
            .field("base_url", &self.base_url)
            .field("spaces_filter", &self.spaces_filter)
            .finish()
    }
}

impl Config {
    /// Resolves the configuration or reports every missing variable at once.
    pub fn from_env() -> Result<Self, Error> {
        let site = env_setting(SITE_ENV);
        let email = env_setting(EMAIL_ENV);
        let token = env_setting(TOKEN_ENV);
        let missing: Vec<&'static str> = [
            (SITE_ENV, site.is_some()),
            (EMAIL_ENV, email.is_some()),
            (TOKEN_ENV, token.is_some()),
        ]
        .iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| *name)
        .collect();
        let (site, email, token) = match (site, email, token) {
            (Some(site), Some(email), Some(token)) => (site, email, token),
            _ => return Err(Error::NotConfigured { missing }),
        };
        let site = normalize_site(&site)?;
        let cloud_id = match env_setting(CLOUD_ID_ENV) {
            Some(id) if is_cloud_id(&id) => Some(id),
            Some(id) => {
                return Err(Error::Config(format!(
                    "{CLOUD_ID_ENV}={id:?} is not a cloud id; copy the `cloudId` field from \
                     https://{site}/_edge/tenant_info, or leave it empty for a classic token"
                )));
            }
            None => None,
        };
        let origin = match env_setting(BASE_URL_ENV) {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None if cloud_id.is_some() => GATEWAY_ORIGIN.to_owned(),
            None => format!("https://{site}"),
        };
        if !(origin.starts_with("https://") || origin.starts_with("http://")) {
            return Err(Error::Config(format!(
                "{BASE_URL_ENV}={origin:?} must start with https:// (or http:// for a local fixture)"
            )));
        }
        let base_url = match &cloud_id {
            Some(id) => format!("{origin}/ex/confluence/{id}"),
            None => origin,
        };
        Ok(Self {
            site,
            email,
            token,
            cloud_id,
            base_url,
            spaces_filter: spaces_filter()?,
        })
    }

    /// `site` or `gateway` — which of the two Atlassian routes is in use.
    pub fn route(&self) -> &'static str {
        if self.cloud_id.is_some() {
            "gateway"
        } else {
            "site"
        }
    }

    /// Absolute browser URL for a `_links.webui`-style path (or an absolute
    /// URL, returned unchanged).
    pub fn web_url(&self, link: &str) -> Option<String> {
        let link = link.trim();
        if link.is_empty() {
            return None;
        }
        if link.starts_with("http://") || link.starts_with("https://") {
            return Some(link.to_owned());
        }
        let path = if link.starts_with('/') {
            link.to_owned()
        } else {
            format!("/{link}")
        };
        Some(format!("https://{}/wiki{path}", self.site))
    }

    /// Whether a space key passes `CONFLUENCE_SPACES_FILTER` (always true
    /// when the filter is unset).
    pub fn space_allowed(&self, key: &str) -> bool {
        self.spaces_filter.is_empty()
            || self
                .spaces_filter
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(key))
    }
}

/// The remediation text every credential error carries: the env var, the
/// secret ref, where to get a token, and the scoped-token trap.
pub fn remediation(site: Option<&str>) -> String {
    let site = site.unwrap_or("<site>.atlassian.net");
    format!(
        "Create an API token at {TOKEN_URL} (set an expiry; all tokens expire, max 1 year) and \
         register it as the `{SECRET_REF}` secret (env {TOKEN_ENV}) — paste it in Cosmonic Desktop \
         → Secrets, or run cosmonic_set_secret name={SECRET_REF} uri=keychain://cosmonic/{SECRET_REF} \
         env={TOKEN_ENV} value=<token>. The same ref serves atlassian-jira-mcp. {SITE_ENV} and \
         {EMAIL_ENV} are named config (localResources.environment.config in deploy/workload.yaml). \
         If the token was created 'with scopes', it only works through api.atlassian.com: also set \
         {CLOUD_ID_ENV} (the `cloudId` at https://{site}/_edge/tenant_info) and keep \
         https://api.atlassian.com in allowedHosts. Then call check_auth."
    )
}

/// The `credentials` block of the `GET /` discovery document (presence only,
/// never values).
pub fn credentials() -> Value {
    let status = |name: &str| {
        if env_setting(name).is_some() {
            "configured"
        } else {
            "missing"
        }
    };
    json!([{
        "ref": SECRET_REF,
        "env": TOKEN_ENV,
        "kind": "basic-auth-api-token",
        "status": status(TOKEN_ENV),
        "description": "Atlassian API token, sent as the Basic-auth password together with \
                        ATLASSIAN_EMAIL. One ref serves both the Jira and Confluence servers.",
        "obtainUrl": TOKEN_URL,
        "scopes": SCOPES,
        "validate": "check_auth",
        "namedConfig": [
            {"env": SITE_ENV, "status": status(SITE_ENV), "required": true},
            {"env": EMAIL_ENV, "status": status(EMAIL_ENV), "required": true},
            {"env": CLOUD_ID_ENV, "status": status(CLOUD_ID_ENV), "required": false},
            {"env": READ_ONLY_ENV, "status": status(READ_ONLY_ENV), "required": false},
            {"env": ALLOW_DELETE_ENV, "status": status(ALLOW_DELETE_ENV), "required": false},
            {"env": SPACES_FILTER_ENV, "status": status(SPACES_FILTER_ENV), "required": false},
        ],
    }])
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A non-2xx answer from Confluence, decoded from either of its error shapes
/// — v2 `{"errors":[{"status","code","title","detail"}]}` or v1
/// `{"statusCode","message","reason"}` — when present.
#[derive(Debug, Clone)]
pub struct UpstreamError {
    pub status: u16,
    /// Confluence's own messages (v2 `title`/`detail`, v1 `message`).
    pub messages: Vec<String>,
    /// v2 error codes (`NOT_FOUND`, `INVALID_REQUEST_PARAMETER`, …).
    pub codes: Vec<String>,
    /// Body excerpt when it was not a known error shape.
    pub raw: String,
    /// `Retry-After` in seconds when Confluence sent one (429 / 503).
    pub retry_after: Option<u64>,
    /// `RateLimit-Reason` / `X-RateLimit-Reason` when present.
    pub rate_limit_reason: Option<String>,
}

impl UpstreamError {
    /// Confluence's own words, joined.
    pub fn upstream_text(&self) -> String {
        let mut parts: Vec<String> = self.messages.clone();
        if parts.is_empty() && !self.raw.is_empty() {
            parts.push(self.raw.clone());
        }
        parts.join("; ")
    }

    fn mentions(&self, needle: &str) -> bool {
        let needle = needle.to_ascii_lowercase();
        self.messages
            .iter()
            .chain(std::iter::once(&self.raw))
            .any(|text| text.to_ascii_lowercase().contains(&needle))
    }
}

/// Everything that can go wrong between a tool and Confluence.
#[derive(Debug, Clone)]
pub enum Error {
    /// Required settings are absent (no upstream call was made).
    NotConfigured { missing: Vec<&'static str> },
    /// A setting is present but unusable.
    Config(String),
    /// The outbound fetch itself failed (policy, DNS, TLS, timeout, size).
    Transport(String),
    /// Confluence answered with a non-2xx status.
    Upstream(UpstreamError),
    /// Confluence (or something in front of it) answered with HTML.
    Html { status: u16, base_url: String },
    /// A 2xx body was not the JSON we expected.
    Decode(String),
}

impl Error {
    /// The HTTP status behind this error, when there is one.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Upstream(err) => Some(err.status),
            Error::Html { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Short machine-readable classification for structured error output.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::NotConfigured { .. } => "not_configured",
            Error::Config(_) => "bad_config",
            Error::Transport(_) => "transport",
            Error::Upstream(err) => match err.status {
                400 => "bad_request",
                401 => "unauthorized",
                403 => "forbidden",
                404 => "not_found",
                409 => "conflict",
                413 => "too_large",
                429 => "rate_limited",
                500..=599 => "upstream_unavailable",
                300..=399 => "redirected",
                _ => "upstream_error",
            },
            Error::Html { .. } => "html_response",
            Error::Decode(_) => "decode",
        }
    }

    /// Whether a caller may reasonably retry (after the stated delay).
    pub fn retryable(&self) -> bool {
        match self {
            Error::Transport(text) => text.contains("timed out"),
            Error::Upstream(err) => matches!(err.status, 409 | 429 | 500..=599),
            _ => false,
        }
    }
}

/// Context a tool passes so an error can be phrased for its operation.
#[derive(Debug, Default, Clone)]
pub struct ErrorContext {
    /// What was being done, e.g. `get page 123`.
    pub operation: String,
    /// What a 404 means for this operation (Confluence does not distinguish
    /// missing from unviewable).
    pub not_found: Option<String>,
    /// What to try after a 400 for this operation.
    pub bad_request: Option<String>,
}

impl ErrorContext {
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            ..Self::default()
        }
    }
    pub fn not_found(mut self, hint: impl Into<String>) -> Self {
        self.not_found = Some(hint.into());
        self
    }
    pub fn bad_request(mut self, hint: impl Into<String>) -> Self {
        self.bad_request = Some(hint.into());
        self
    }
}

/// Phrases an error for the caller: a readable message plus a structured
/// detail object (`kind`, `status`, `messages`, `codes`,
/// `retry_after_seconds`, `retryable`, `hint`).
pub fn describe(err: &Error, ctx: &ErrorContext, site: Option<&str>) -> (String, Value) {
    let op = &ctx.operation;
    let mut detail = json!({
        "kind": err.kind(),
        "operation": op,
        "retryable": err.retryable(),
    });
    let message = match err {
        Error::NotConfigured { missing } => {
            let list = missing.join(", ");
            detail["missing"] = json!(missing);
            format!(
                "Confluence is not configured: {list} {} not set. {}",
                if missing.len() == 1 { "is" } else { "are" },
                remediation(site)
            )
        }
        Error::Config(text) => format!("Confluence configuration problem: {text}"),
        Error::Transport(text) => {
            let hint = if text.contains("timed out") {
                "The upstream deadline elapsed; retry once, then report Confluence as unreachable."
                    .to_owned()
            } else if text.contains("size limit") {
                "The response exceeded the outbound size cap (4 MiB); request a smaller page \
                 (lower limit) or a lighter format (get_page format=text truncates, storage does not)."
                    .to_owned()
            } else {
                format!(
                    "Check that the site host (https://<site>.atlassian.net, or \
                     https://api.atlassian.com for scoped tokens) is listed in the workload's \
                     allowedHosts and that {SITE_ENV} is correct; TLS uses public roots only. Do \
                     not retry a policy denial."
                )
            };
            format!("Could not reach Confluence while trying to {op}: {text}. {hint}")
        }
        Error::Html { status, base_url } => format!(
            "Confluence at {base_url} answered HTTP {status} with an HTML page instead of JSON \
             while trying to {op}. That usually means {SITE_ENV} names a site that does not exist, \
             the request was redirected to a login page, or Atlassian is showing a maintenance \
             page; check the site host (or {BASE_URL_ENV}) and https://status.atlassian.com."
        ),
        Error::Decode(text) => {
            format!("Confluence returned an unreadable answer while trying to {op}: {text}")
        }
        Error::Upstream(up) => {
            detail["status"] = json!(up.status);
            if !up.messages.is_empty() {
                detail["messages"] = json!(up.messages);
            }
            if !up.codes.is_empty() {
                detail["codes"] = json!(up.codes);
            }
            if let Some(seconds) = up.retry_after {
                detail["retry_after_seconds"] = json!(seconds);
            }
            if let Some(reason) = &up.rate_limit_reason {
                detail["rate_limit_reason"] = json!(reason);
            }
            let text = up.upstream_text();
            let said = if text.is_empty() {
                String::new()
            } else {
                format!(" Confluence said: {text}.")
            };
            let hint = match up.status {
                400 if up.mentions("already exists") => {
                    "A page with that title already exists in the space (titles are unique per \
                     space, trashed pages included): find it with list_pages(space_id, title) and \
                     use update_page, or choose another title."
                        .to_owned()
                }
                400 if up.mentions("cql") => {
                    "Invalid CQL: quote values (space = \"ENG\"), use text ~ / title ~ / label = / \
                     type = / lastmodified > / ancestor =, and avoid user.* fields (unsupported on \
                     this endpoint). The `text` parameter builds a safe query for you."
                        .to_owned()
                }
                400 if up.mentions("xhtml") || up.mentions("parse") || up.mentions("close tag") => {
                    "The body is not well-formed storage format (XHTML): send body_format=markdown \
                     (default) and let the server convert, or fix the XML (close every tag, write & \
                     as &amp;, put raw code inside the code macro's CDATA; control characters are \
                     stripped by the server, so a leftover parse error points at tag structure or \
                     an unknown macro/attribute, not at the text)."
                        .to_owned()
                }
                400 if up.mentions("version") => {
                    "The version number was not accepted; call update_page again (it re-reads the \
                     live version) — do not resend the same number."
                        .to_owned()
                }
                400 => ctx.bad_request.clone().unwrap_or_else(|| {
                    "Confluence rejected the request; its message names the parameter or field to \
                     fix."
                        .to_owned()
                }),
                401 => format!(
                    "Authentication failed: the email/token pair is wrong, the token expired \
                     (tokens live at most one year), it was revoked, or a token created 'with \
                     scopes' is being used against the site URL ('scope does not match'). {} Do \
                     not retry with the same credentials.",
                    remediation(site)
                ),
                403 => format!(
                    "Permission denied: the account is authenticated but has no Confluence product \
                     access on this site ('Can use'), lacks the space permission for this \
                     operation (view / add page / add comment / delete), the space is archived, or \
                     the scoped token lacks {}. Ask a Confluence admin or use a space the account \
                     can act in; do not retry.",
                    SCOPES.join(" / ")
                ),
                404 => ctx.not_found.clone().unwrap_or_else(|| {
                    "Confluence found nothing at that id, or the account is not allowed to see it \
                     (v2 does not distinguish missing, trashed, draft and unviewable). Verify the \
                     numeric id via search or list_pages and check the space with get_space."
                        .to_owned()
                }),
                409 => "Another edit changed the page between read and write (or Confluence's \
                        version propagation lagged); call update_page again — it re-reads the \
                        live version — and reconcile if you passed expected_version."
                    .to_owned(),
                413 => "The request body is over Confluence's 5 MB limit; split the content \
                        across pages or attach it as a file."
                    .to_owned(),
                429 => {
                    let wait = up
                        .retry_after
                        .map(|s| format!("at least {s} seconds"))
                        .unwrap_or_else(|| "a few seconds (exponential backoff)".to_owned());
                    format!(
                        "Rate limited{}. The server already retried once when Retry-After was \
                         short; wait {wait} with jitter (double up to 30 s) before retrying, and \
                         use smaller pages.",
                        up.rate_limit_reason
                            .as_deref()
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default()
                    )
                }
                300..=399 => format!(
                    "Confluence redirected the request, which usually means {SITE_ENV} is wrong \
                     or the site requires a login page; check the site host."
                ),
                500..=599 => "Confluence reported a server-side problem; retry once after a short \
                              delay and check https://status.atlassian.com if it persists."
                    .to_owned(),
                _ => "Unexpected upstream status.".to_owned(),
            };
            detail["hint"] = json!(hint);
            format!(
                "Confluence returned HTTP {} while trying to {op}.{said} {hint}",
                up.status
            )
        }
    };
    (message, detail)
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// A successful upstream exchange: the status and the decoded JSON body
/// (`Value::Null` for an empty 204).
pub struct Reply {
    pub status: u16,
    pub value: Value,
}

/// An authenticated Confluence client for one tool invocation.
pub struct Client {
    pub config: Config,
    authorization: String,
}

impl Client {
    /// Builds the client from the environment (see [`Config::from_env`]).
    pub fn from_env() -> Result<Self, Error> {
        let config = Config::from_env()?;
        let pair = format!("{}:{}", config.email, config.token);
        let authorization = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        );
        Ok(Self {
            config,
            authorization,
        })
    }

    /// Full URL for a v2 path (`/pages/123`) plus encoded query.
    pub fn v2_url(&self, path: &str, query: &[(&str, String)]) -> String {
        self.url("/wiki/api/v2", path, query)
    }

    /// Full URL for a v1 path (`/search`) plus encoded query.
    pub fn v1_url(&self, path: &str, query: &[(&str, String)]) -> String {
        self.url("/wiki/rest/api", path, query)
    }

    fn url(&self, prefix: &str, path: &str, query: &[(&str, String)]) -> String {
        let mut url = format!("{}{prefix}{path}", self.config.base_url);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&encode_query(query));
        }
        url
    }

    pub async fn get(&self, url: String) -> Result<Value, Error> {
        self.send(http::Method::GET, url, None)
            .await
            .map(|r| r.value)
    }

    pub async fn post(&self, url: String, body: &Value) -> Result<Value, Error> {
        self.send(http::Method::POST, url, Some(body))
            .await
            .map(|r| r.value)
    }

    pub async fn put(&self, url: String, body: &Value) -> Result<Value, Error> {
        self.send(http::Method::PUT, url, Some(body))
            .await
            .map(|r| r.value)
    }

    pub async fn delete(&self, url: String) -> Result<Reply, Error> {
        self.send(http::Method::DELETE, url, None).await
    }

    /// Performs one exchange. 2xx with a body → parsed JSON; 2xx without a
    /// body (204) → `Value::Null`; anything else → [`Error`]. A 429 with a
    /// short `Retry-After` is retried once after waiting.
    pub async fn send(
        &self,
        method: http::Method,
        url: String,
        body: Option<&Value>,
    ) -> Result<Reply, Error> {
        let payload: Bytes = match body {
            Some(value) => Bytes::from(value.to_string()),
            None => Bytes::new(),
        };
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            let mut builder = http::Request::builder()
                .method(method.clone())
                .uri(&url)
                .header("Authorization", &self.authorization)
                .header("Accept", "application/json")
                .header("User-Agent", USER_AGENT)
                // Required by Confluence on state-changing requests routed
                // through its XSRF filter; harmless elsewhere.
                .header("X-Atlassian-Token", "no-check");
            if body.is_some() {
                builder = builder.header("Content-Type", "application/json");
            }
            if body.is_some() || method != http::Method::GET {
                // An explicit Content-Length on every request that is not a
                // plain GET — including a bodiless DELETE. wasi:http derives
                // the wire framing from this header alone; without it the
                // (empty) body is streamed chunked, which some servers (and
                // the e2e fixture) do not decode, poisoning the keep-alive
                // connection for the next request.
                builder = builder.header(http::header::CONTENT_LENGTH, payload.len());
            }
            let request = builder
                .body(payload.clone())
                .map_err(|err| Error::Transport(format!("invalid request: {err}")))?;
            tracing::debug!(%method, url = %redact(&url), attempt, "confluence request");
            let response = crate::bridge::outbound::fetch(request)
                .await
                .map_err(|err| Error::Transport(err.to_string()))?;
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            let retry_after = response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let rate_limit_reason = ["ratelimit-reason", "x-ratelimit-reason"]
                .iter()
                .find_map(|name| response.headers().get(*name))
                .and_then(|v| v.to_str().ok())
                .map(|v| v.chars().take(200).collect::<String>());
            let bytes = response.into_body();

            if status == 429
                && attempt == 1
                && retry_after.is_some_and(|seconds| seconds <= MAX_INLINE_RETRY_SECONDS)
            {
                let seconds = retry_after.unwrap_or(1).max(1);
                tracing::warn!(seconds, "rate limited; retrying once after Retry-After");
                tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                continue;
            }

            if (200..300).contains(&status) {
                if bytes.is_empty() {
                    return Ok(Reply {
                        status,
                        value: Value::Null,
                    });
                }
                return match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => Ok(Reply { status, value }),
                    Err(_) if content_type.contains("html") => Err(Error::Html {
                        status,
                        base_url: self.config.base_url.clone(),
                    }),
                    Err(err) => Err(Error::Decode(format!(
                        "expected JSON, got {} bytes of {}: {err}",
                        bytes.len(),
                        if content_type.is_empty() {
                            "unknown content"
                        } else {
                            content_type.as_str()
                        }
                    ))),
                };
            }

            let text = String::from_utf8_lossy(&bytes);
            let looks_html = content_type.contains("html")
                || text.trim_start().starts_with("<!DOCTYPE")
                || text.trim_start().starts_with("<html");
            let parsed = serde_json::from_str::<Value>(&text).ok();
            if parsed.is_none() && looks_html && !matches!(status, 401 | 403 | 429) {
                return Err(Error::Html {
                    status,
                    base_url: self.config.base_url.clone(),
                });
            }
            let (messages, codes) = parsed
                .as_ref()
                .map(extract_error_messages)
                .unwrap_or_default();
            let raw = if messages.is_empty() {
                excerpt(&text, MAX_ERROR_BODY_CHARS)
            } else {
                String::new()
            };
            return Err(Error::Upstream(UpstreamError {
                status,
                messages,
                codes,
                raw,
                retry_after,
                rate_limit_reason,
            }));
        }
    }
}

/// Pulls Confluence's human-readable messages and codes out of an error
/// body in any of its shapes.
fn extract_error_messages(value: &Value) -> (Vec<String>, Vec<String>) {
    let mut messages = Vec::new();
    let mut codes = Vec::new();
    let push_message = |messages: &mut Vec<String>, text: &str| {
        let text = text.trim();
        if !text.is_empty() && messages.len() < 16 && !messages.iter().any(|m| m == text) {
            messages.push(excerpt(text, MAX_ERROR_BODY_CHARS));
        }
    };
    if let Some(errors) = value.get("errors").and_then(|e| e.as_array()) {
        for error in errors.iter().take(16) {
            if let Some(code) = error.get("code").and_then(|c| c.as_str()) {
                if codes.len() < 16 {
                    codes.push(code.chars().take(64).collect());
                }
            }
            let title = error.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let detail = error.get("detail").and_then(|d| d.as_str()).unwrap_or("");
            match (title.is_empty(), detail.is_empty()) {
                (false, false) => push_message(&mut messages, &format!("{title}: {detail}")),
                (false, true) => push_message(&mut messages, title),
                (true, false) => push_message(&mut messages, detail),
                (true, true) => {
                    if let Some(message) = error.get("message").and_then(|m| m.as_str()) {
                        push_message(&mut messages, message);
                    }
                }
            }
        }
    }
    for key in ["message", "reason", "error", "error_description"] {
        if let Some(text) = value.get(key).and_then(|m| m.as_str()) {
            push_message(&mut messages, text);
        }
    }
    if let Some(list) = value.get("errorMessages").and_then(|e| e.as_array()) {
        for item in list.iter().take(16) {
            if let Some(text) = item.as_str() {
                push_message(&mut messages, text);
            }
        }
    }
    (messages, codes)
}

/// A whitespace-collapsed, tag-stripped, character-bounded excerpt of text.
fn excerpt(text: &str, max_chars: usize) -> String {
    let stripped = if text.contains('<') {
        body::storage_to_text(text)
    } else {
        text.to_owned()
    };
    let collapsed: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    body::truncate_chars(&collapsed, max_chars).0
}

/// Drops the query string from a URL for logging (cursors and CQL can be
/// long; nothing secret is in the URL, but keep logs tidy).
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass).
pub fn encode(value: &str) -> String {
    utf8_percent_encode(value, QUERY_ENCODE).to_string()
}

/// Builds `k=v&k2=v2` with every value percent-encoded.
pub fn encode_query(query: &[(&str, String)]) -> String {
    query
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-decodes a query value (`+` is left alone: Confluence cursors are
/// base64url and never contain it as a space).
fn percent_decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}

/// Extracts the opaque `cursor` from a response's `_links.next`, which is a
/// relative URL such as `/wiki/api/v2/pages?cursor=eyJ...&limit=25` (v2) or
/// `/rest/api/search?cursor=...&cql=...` (v1).
pub fn next_cursor(value: &Value) -> Option<String> {
    let next = value.get("_links")?.get("next")?.as_str()?;
    let query = next.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, raw) = pair.split_once('=')?;
        (key == "cursor" && !raw.is_empty()).then(|| percent_decode(raw))
    })
}

/// Clamps a caller-supplied page size to `1..=max`, applying `default` when
/// absent. Negative values are refused before any upstream call.
pub fn clamp_limit(value: Option<i64>, default: u32, max: u32) -> Result<u32, String> {
    match value {
        None => Ok(default),
        Some(n) if n < 0 => Err(format!(
            "limit must be a non-negative integer (1..{max}; default {default}), got {n}"
        )),
        Some(n) => Ok(u32::try_from(n).unwrap_or(u32::MAX).clamp(1, max)),
    }
}

/// Escapes a value for use inside a double-quoted CQL string.
pub fn cql_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Removes the `@@@hl@@@` / `@@@endhl@@@` highlight markers from a search
/// excerpt and collapses whitespace.
pub fn clean_excerpt(excerpt: &str) -> String {
    let cleaned = excerpt.replace("@@@hl@@@", "").replace("@@@endhl@@@", "");
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Result shapers
// ---------------------------------------------------------------------------

fn str_field(value: &Value, key: &str) -> Value {
    match value.get(key) {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Number(n)) => Value::String(n.to_string()),
        Some(Value::Bool(b)) => Value::Bool(*b),
        _ => Value::Null,
    }
}

/// `{number, createdAt, authorId, message, minorEdit}` from a v2 `version`.
pub fn simplify_version(version: Option<&Value>) -> Value {
    let Some(version) = version else {
        return Value::Null;
    };
    json!({
        "number": version.get("number").cloned().unwrap_or(Value::Null),
        "createdAt": str_field(version, "createdAt"),
        "authorId": str_field(version, "authorId"),
        "message": str_field(version, "message"),
        "minorEdit": str_field(version, "minorEdit"),
    })
}

/// Compact page metadata (no body) from a v2 page object.
pub fn simplify_page(page: &Value, config: &Config) -> Value {
    let links = page.get("_links");
    let webui = links
        .and_then(|l| l.get("webui"))
        .and_then(|w| w.as_str())
        .and_then(|w| config.web_url(w));
    let editui = links
        .and_then(|l| l.get("editui"))
        .and_then(|w| w.as_str())
        .and_then(|w| config.web_url(w));
    let mut value = json!({
        "id": str_field(page, "id"),
        "title": str_field(page, "title"),
        "status": str_field(page, "status"),
        "spaceId": str_field(page, "spaceId"),
        "parentId": str_field(page, "parentId"),
        "parentType": str_field(page, "parentType"),
        "authorId": str_field(page, "authorId"),
        "createdAt": str_field(page, "createdAt"),
        "version": simplify_version(page.get("version")),
        "url": webui,
    });
    if let Some(editui) = editui {
        value["edit_url"] = json!(editui);
    }
    if let Some(position) = page.get("childPosition") {
        value["childPosition"] = position.clone();
    }
    if let Some(labels) = page
        .get("labels")
        .and_then(|l| l.get("results"))
        .and_then(|r| r.as_array())
    {
        value["labels"] = json!(labels.iter().map(simplify_label).collect::<Vec<_>>());
    }
    value
}

/// Compact space from a v2 space object.
pub fn simplify_space(space: &Value, config: &Config) -> Value {
    let webui = space
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|w| w.as_str())
        .and_then(|w| config.web_url(w));
    let description = space
        .get("description")
        .and_then(|d| d.get("plain").or_else(|| d.get("view")))
        .and_then(|p| p.get("value"))
        .and_then(|v| v.as_str())
        .map(|text| body::truncate_chars(text.trim(), 2000).0);
    json!({
        "id": str_field(space, "id"),
        "key": str_field(space, "key"),
        "name": str_field(space, "name"),
        "type": str_field(space, "type"),
        "status": str_field(space, "status"),
        "homepageId": str_field(space, "homepageId"),
        "authorId": str_field(space, "authorId"),
        "createdAt": str_field(space, "createdAt"),
        "description": description,
        "url": webui,
    })
}

/// Compact comment (footer or inline) with its body rendered to text.
pub fn simplify_comment(comment: &Value, config: &Config, max_chars: usize) -> Value {
    let webui = comment
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|w| w.as_str())
        .and_then(|w| config.web_url(w));
    let storage = comment
        .get("body")
        .and_then(|b| b.get("storage").or_else(|| b.get("view")))
        .and_then(|s| s.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let (text, truncated) = body::truncate_chars(&body::storage_to_text(storage), max_chars);
    let mut value = json!({
        "id": str_field(comment, "id"),
        "status": str_field(comment, "status"),
        "title": str_field(comment, "title"),
        "pageId": str_field(comment, "pageId"),
        "blogPostId": str_field(comment, "blogPostId"),
        "parentCommentId": str_field(comment, "parentCommentId"),
        "version": simplify_version(comment.get("version")),
        "body": text,
        "url": webui,
    });
    if truncated {
        value["body_truncated"] = json!(true);
    }
    if let Some(resolution) = comment.get("resolutionStatus") {
        value["resolutionStatus"] = resolution.clone();
    }
    if let Some(selection) = comment
        .get("properties")
        .and_then(|p| p.get("inlineOriginalSelection"))
        .and_then(|s| s.as_str())
    {
        value["inlineOriginalSelection"] = json!(body::truncate_chars(selection, 500).0);
    }
    value
}

/// `{id, name, prefix}` from a v1 or v2 label object.
pub fn simplify_label(label: &Value) -> Value {
    json!({
        "id": str_field(label, "id"),
        "name": str_field(label, "name"),
        "prefix": str_field(label, "prefix"),
    })
}

/// Compact attachment with absolute download and web URLs.
pub fn simplify_attachment(attachment: &Value, config: &Config) -> Value {
    let links = attachment.get("_links");
    let webui = links
        .and_then(|l| l.get("webui"))
        .and_then(|w| w.as_str())
        .or_else(|| attachment.get("webuiLink").and_then(|w| w.as_str()))
        .and_then(|w| config.web_url(w));
    let download = links
        .and_then(|l| l.get("download"))
        .and_then(|w| w.as_str())
        .or_else(|| attachment.get("downloadLink").and_then(|w| w.as_str()))
        .and_then(|w| config.web_url(w));
    json!({
        "id": str_field(attachment, "id"),
        "title": str_field(attachment, "title"),
        "status": str_field(attachment, "status"),
        "pageId": str_field(attachment, "pageId"),
        "mediaType": str_field(attachment, "mediaType"),
        "mediaTypeDescription": str_field(attachment, "mediaTypeDescription"),
        "fileSize": attachment.get("fileSize").cloned().unwrap_or(Value::Null),
        "comment": str_field(attachment, "comment"),
        "createdAt": str_field(attachment, "createdAt"),
        "version": simplify_version(attachment.get("version")),
        "url": webui,
        "download_url": download,
    })
}

/// Compact v1 search hit: content identity, space key, web URL, cleaned
/// excerpt.
pub fn simplify_search_result(result: &Value, config: &Config) -> Value {
    let content = result.get("content");
    let content_links = content.and_then(|c| c.get("_links"));
    let webui = content_links
        .and_then(|l| l.get("webui"))
        .and_then(|w| w.as_str())
        .or_else(|| result.get("url").and_then(|u| u.as_str()))
        .and_then(|w| config.web_url(w));
    // The space key hides in one of three places depending on the entity.
    let space_key = content
        .and_then(|c| c.get("space"))
        .and_then(|s| s.get("key"))
        .and_then(|k| k.as_str())
        .map(str::to_owned)
        .or_else(|| {
            content
                .and_then(|c| c.get("_expandable"))
                .and_then(|e| e.get("space"))
                .and_then(|s| s.as_str())
                .and_then(|s| s.rsplit('/').next())
                .map(str::to_owned)
        })
        .or_else(|| {
            result
                .get("resultGlobalContainer")
                .and_then(|g| g.get("displayUrl"))
                .and_then(|u| u.as_str())
                .and_then(|u| u.strip_prefix("/spaces/"))
                .and_then(|u| u.split('/').next())
                .map(str::to_owned)
        });
    let excerpt = result
        .get("excerpt")
        .and_then(|e| e.as_str())
        .map(clean_excerpt)
        .map(|e| body::truncate_chars(&e, 1000).0);
    let (id, kind, status, title) = match content {
        Some(content) => (
            str_field(content, "id"),
            str_field(content, "type"),
            str_field(content, "status"),
            str_field(content, "title"),
        ),
        None => (
            Value::Null,
            str_field(result, "entityType"),
            Value::Null,
            str_field(result, "title"),
        ),
    };
    json!({
        "id": id,
        "type": kind,
        "status": status,
        "title": title,
        "spaceKey": space_key,
        "url": webui,
        "lastModified": str_field(result, "lastModified"),
        "excerpt": excerpt,
        "entityType": str_field(result, "entityType"),
    })
}

/// Compact identity from `GET /wiki/rest/api/user/current`.
pub fn simplify_user(user: &Value) -> Value {
    json!({
        "accountId": str_field(user, "accountId"),
        "accountType": str_field(user, "accountType"),
        "email": str_field(user, "email"),
        "displayName": str_field(user, "displayName"),
        "publicName": str_field(user, "publicName"),
        "timeZone": str_field(user, "timeZone"),
        "type": str_field(user, "type"),
        "isExternalCollaborator": str_field(user, "isExternalCollaborator"),
    })
}
