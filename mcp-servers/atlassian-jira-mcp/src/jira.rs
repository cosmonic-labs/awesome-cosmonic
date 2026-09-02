//! Jira Cloud REST API v3 client.
//!
//! Configuration from the environment (named config + the `atlassian-api-token`
//! secret), HTTP Basic authentication, request plumbing over the
//! [`crate::bridge::outbound`] bridge, upstream error mapping, the
//! Atlassian Document Format (ADF) renderer / builder, and the input
//! validators that keep client-supplied keys out of URL paths.
//!
//! Two routes exist for the same API:
//!
//! - classic API tokens: `https://<site>/rest/api/3/...`
//! - "API tokens with scopes": `https://api.atlassian.com/ex/jira/<cloudId>/rest/api/3/...`
//!
//! [`Config::from_env`] picks the route from `ATLASSIAN_CLOUD_ID`.
//! `ATLASSIAN_BASE_URL` overrides the *origin* (the e2e points it at a local
//! fixture); the `/ex/jira/<cloudId>` prefix still applies when a cloud id is
//! set, so the gateway route is testable hermetically.

use std::collections::BTreeMap;

use base64::Engine as _;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};

/// Secret: the Atlassian API token (Basic-auth password).
pub const TOKEN_ENV: &str = "ATLASSIAN_API_TOKEN";
/// Named config: Jira Cloud site host, e.g. `acme.atlassian.net`.
pub const SITE_ENV: &str = "ATLASSIAN_SITE";
/// Named config: the Atlassian account email owning the token.
pub const EMAIL_ENV: &str = "ATLASSIAN_EMAIL";
/// Named config (optional): cloud id, required for scoped tokens.
pub const CLOUD_ID_ENV: &str = "ATLASSIAN_CLOUD_ID";
/// Test override: replaces the computed origin.
pub const BASE_URL_ENV: &str = "ATLASSIAN_BASE_URL";
/// Named config (optional): `true` refuses every write tool.
pub const READ_ONLY_ENV: &str = "JIRA_READ_ONLY";
/// Named config (optional): comma-separated project keys to scope to.
pub const PROJECTS_FILTER_ENV: &str = "JIRA_PROJECTS_FILTER";
/// The Desktop secret reference that injects [`TOKEN_ENV`]. Deliberately
/// shared with the Confluence server (one ref per Atlassian account).
pub const SECRET_REF: &str = "atlassian-api-token";
/// Where a user creates an API token.
pub const TOKEN_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";
/// Origin of the scoped-token gateway.
pub const GATEWAY_ORIGIN: &str = "https://api.atlassian.com";
/// Scopes a scoped token needs for this server's tools.
pub const SCOPES: &[&str] = &["read:jira-work", "write:jira-work", "read:jira-user"];

const USER_AGENT: &str = concat!("atlassian-jira-mcp/", env!("CARGO_PKG_VERSION"));
/// Longest slice of a non-JSON upstream error body carried into a tool error.
const MAX_ERROR_BODY_CHARS: usize = 1024;
/// Rendered ADF is cut here (characters) so one busy description cannot
/// dominate a tool result.
pub const ADF_MAX_CHARS: usize = 20_000;
/// Nesting depth beyond which the ADF renderer stops descending. Real
/// documents nest a handful of levels; serde_json's own 128-level JSON limit
/// (two JSON levels per ADF node) already rejects anything absurd before it
/// gets here, so this is defense in depth.
const ADF_MAX_DEPTH: usize = 32;
/// Marker appended to any text this module truncates.
pub const TRUNCATED: &str = "…[truncated]";

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

/// Whether `JIRA_READ_ONLY` refuses the write tools.
pub fn read_only() -> bool {
    env_flag(READ_ONLY_ENV)
}

/// The `JIRA_PROJECTS_FILTER` keys (uppercased, deduplicated, validated).
pub fn projects_filter() -> Result<Vec<String>, Error> {
    let Some(raw) = env_setting(PROJECTS_FILTER_ENV) else {
        return Ok(Vec::new());
    };
    let mut keys: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let key = part.trim().to_ascii_uppercase();
        if key.is_empty() {
            continue;
        }
        if !is_project_key(&key) {
            return Err(Error::Config(format!(
                "{PROJECTS_FILTER_ENV} contains an invalid project key {key:?}; \
                 use comma-separated keys such as PROJ,OPS"
            )));
        }
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

// ---------------------------------------------------------------------------
// Validators (keys go into URL paths — never interpolate unchecked input)
// ---------------------------------------------------------------------------

/// Jira project key: a letter followed by letters, digits or underscores.
/// Case is preserved (Jira keys are case-sensitive; the default scheme is
/// uppercase).
pub fn is_project_key(s: &str) -> bool {
    let bytes = s.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// A numeric Jira id (issue, transition, issue type, project).
pub fn is_numeric_id(s: &str) -> bool {
    (1..=20).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_digit())
}

/// Issue key (`PROJ-123`) or numeric issue id.
pub fn is_issue_key(s: &str) -> bool {
    if is_numeric_id(s) {
        return true;
    }
    match s.rsplit_once('-') {
        Some((project, number)) => is_project_key(project) && is_numeric_id(number),
        None => false,
    }
}

/// Atlassian account id (`5b10ac8d82e05b22cc7d4ef5`, `557058:<uuid>`).
pub fn is_account_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'-' | b'_'))
}

/// Atlassian cloud id (a UUID in practice).
pub fn is_cloud_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// Strips a scheme, path and trailing slashes from a site value and checks
/// it is a plausible hostname.
fn normalize_site(raw: &str) -> Result<String, Error> {
    let mut site = raw.trim();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = site.strip_prefix(scheme) {
            site = rest;
        }
    }
    let site = site
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let valid = (1..=253).contains(&site.len())
        && site
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        && !site.starts_with('.')
        && !site.ends_with('.');
    if !valid {
        return Err(Error::Config(format!(
            "{SITE_ENV}={raw:?} is not a hostname; set it to your Jira Cloud site such as \
             acme.atlassian.net (no scheme, no path)"
        )));
    }
    Ok(site)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Everything the client needs, resolved from the environment once per tool
/// call (instances are stateless; reading a few env vars is cheap).
#[derive(Debug, Clone)]
pub struct Config {
    /// Site host, e.g. `acme.atlassian.net` (used for browse URLs and hints).
    pub site: String,
    /// Basic-auth user.
    pub email: String,
    /// Basic-auth password — never logged, never echoed.
    token: String,
    /// Set for scoped tokens: routes through the gateway.
    pub cloud_id: Option<String>,
    /// Origin plus optional `/ex/jira/<cloudId>`; no trailing slash.
    pub base_url: String,
    /// `JIRA_PROJECTS_FILTER` keys, possibly empty.
    pub projects_filter: Vec<String>,
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
            Some(id) => format!("{origin}/ex/jira/{id}"),
            None => origin,
        };
        Ok(Self {
            site,
            email,
            token,
            cloud_id,
            base_url,
            projects_filter: projects_filter()?,
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

    /// Browser URL of an issue.
    pub fn browse_url(&self, issue_key: &str) -> String {
        format!("https://{}/browse/{issue_key}", self.site)
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
         env={TOKEN_ENV} value=<token>. {SITE_ENV} and {EMAIL_ENV} are named config \
         (localResources.environment.config in deploy/workload.yaml). If the token was created \
         'with scopes' ({}), it only works through api.atlassian.com: also set {CLOUD_ID_ENV} \
         (the `cloudId` at https://{site}/_edge/tenant_info) and keep https://api.atlassian.com in \
         allowedHosts. Then call check_auth.",
        SCOPES.join(", ")
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
        ],
    }])
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A non-2xx answer from Jira, decoded from its `ErrorCollection` shape
/// (`{errorMessages: [], errors: {field: message}}`) when present.
#[derive(Debug, Clone)]
pub struct UpstreamError {
    pub status: u16,
    pub error_messages: Vec<String>,
    pub errors: BTreeMap<String, String>,
    /// Body excerpt when it was not an ErrorCollection (gateway errors,
    /// proxies, empty bodies).
    pub raw: String,
    /// `Retry-After` in seconds when Jira sent one (429 / 503).
    pub retry_after: Option<u64>,
    /// `RateLimit-Reason` / `X-RateLimit-Reason` when present.
    pub rate_limit_reason: Option<String>,
}

impl UpstreamError {
    /// Jira's own words, joined — precise for JQL and field errors.
    pub fn upstream_text(&self) -> String {
        let mut parts: Vec<String> = self.error_messages.clone();
        parts.extend(
            self.errors
                .iter()
                .map(|(field, message)| format!("{field}: {message}")),
        );
        if parts.is_empty() && !self.raw.is_empty() {
            parts.push(self.raw.clone());
        }
        parts.join("; ")
    }
}

/// Everything that can go wrong between a tool and Jira.
#[derive(Debug, Clone)]
pub enum Error {
    /// Required settings are absent (no upstream call was made).
    NotConfigured { missing: Vec<&'static str> },
    /// A setting is present but unusable.
    Config(String),
    /// The outbound fetch itself failed (policy, DNS, TLS, timeout, size).
    Transport(String),
    /// Jira answered with a non-2xx status.
    Upstream(UpstreamError),
    /// Jira (or something in front of it) answered with HTML.
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
                410 => "gone",
                413 => "too_large",
                422 => "unprocessable",
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
    /// What was being done, e.g. `get issue PROJ-1`.
    pub operation: String,
    /// What a 404 means for this operation (Jira does not distinguish
    /// missing from unbrowsable).
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
/// detail object (`kind`, `status`, `messages`, `errors`,
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
                "Jira is not configured: {list} {} not set. {}",
                if missing.len() == 1 { "is" } else { "are" },
                remediation(site)
            )
        }
        Error::Config(text) => format!("Jira configuration problem: {text}"),
        Error::Transport(text) => {
            let hint = if text.contains("timed out") {
                "The upstream deadline elapsed; retry once, then report Jira as unreachable."
            } else if text.contains("size limit") {
                "The response exceeded the outbound size cap; request fewer fields or a smaller page."
            } else {
                "Check that the site host (https://<site>.atlassian.net, or https://api.atlassian.com \
                 for scoped tokens) is listed in the workload's allowedHosts and that ATLASSIAN_SITE \
                 is correct; TLS uses public roots only. Do not retry a policy denial."
            };
            format!("Could not reach Jira while trying to {op}: {text}. {hint}")
        }
        Error::Html { status, base_url } => format!(
            "Jira at {base_url} answered HTTP {status} with an HTML page instead of JSON while \
             trying to {op}. That usually means {SITE_ENV} names a site that does not exist or the \
             request was redirected to a login page; check the site host (or {BASE_URL_ENV})."
        ),
        Error::Decode(text) => {
            format!("Jira returned an unreadable answer while trying to {op}: {text}")
        }
        Error::Upstream(up) => {
            detail["status"] = json!(up.status);
            if !up.error_messages.is_empty() {
                detail["messages"] = json!(up.error_messages);
            }
            if !up.errors.is_empty() {
                detail["errors"] = json!(up.errors);
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
                format!(" Jira said: {text}.")
            };
            let hint = match up.status {
                400 => ctx.bad_request.clone().unwrap_or_else(|| {
                    "Jira rejected the request; its message names the field or JQL clause to fix."
                        .to_owned()
                }),
                401 => format!(
                    "Authentication failed: the email/token pair is wrong, the token expired \
                     (tokens live at most one year), it was revoked, or a token created 'with \
                     scopes' is being used against the site URL. {} Do not retry with the same \
                     credentials.",
                    remediation(site)
                ),
                403 => format!(
                    "Permission denied: the account is authenticated but lacks the Jira permission \
                     for this operation (Browse Projects, Create Issues, Edit Issues, Assign Issues, \
                     Add Comments, Transition Issues) or the scoped token lacks {}. Ask a Jira admin \
                     or use a project the account can act in; do not retry.",
                    SCOPES.join(" / ")
                ),
                404 => ctx.not_found.clone().unwrap_or_else(|| {
                    "Jira found nothing at that address, or the account is not allowed to see it \
                     (Jira does not distinguish the two)."
                        .to_owned()
                }),
                409 => "Another update changed the issue concurrently; re-read it and retry once."
                    .to_owned(),
                410 => format!(
                    "That endpoint is gone. This server only uses the current /search/jql API, so \
                     {BASE_URL_ENV} probably points somewhere stale."
                ),
                413 => "A per-issue limit (comments, worklogs, attachments, remote links) was \
                        reached; do not retry."
                    .to_owned(),
                422 => "A workflow validator, post-function or field configuration blocks this \
                        change; a Jira admin has to adjust it. Do not retry."
                    .to_owned(),
                429 => {
                    let wait = up
                        .retry_after
                        .map(|s| format!("at least {s} seconds"))
                        .unwrap_or_else(|| "a few seconds (exponential backoff)".to_owned());
                    format!(
                        "Rate limited{}. Wait {wait} before retrying (cap 4 tries), request fewer \
                         fields, and use smaller pages; the server does not retry for you.",
                        up.rate_limit_reason
                            .as_deref()
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default()
                    )
                }
                300..=399 => format!(
                    "Jira redirected the request, which usually means {SITE_ENV} is wrong or the \
                     site requires a login page; check the site host."
                ),
                500..=599 => "Jira reported a server-side problem; retry once after a short delay."
                    .to_owned(),
                _ => "Unexpected upstream status.".to_owned(),
            };
            detail["hint"] = json!(hint);
            format!(
                "Jira returned HTTP {} while trying to {op}.{said} {hint}",
                up.status
            )
        }
    };
    (message, detail)
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// An authenticated Jira client for one tool invocation.
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

    /// Full URL for an API path (`/issue/PROJ-1`) plus encoded query.
    pub fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        let mut url = format!("{}/rest/api/3{path}", self.config.base_url);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&encode_query(query));
        }
        url
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, Error> {
        self.send(http::Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, Error> {
        self.send(http::Method::POST, path, &[], Some(body)).await
    }

    pub async fn put(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: &Value,
    ) -> Result<Value, Error> {
        self.send(http::Method::PUT, path, query, Some(body)).await
    }

    /// Performs one exchange. 2xx with a body → parsed JSON; 2xx without a
    /// body (204) → `Value::Null`; anything else → [`Error`].
    pub async fn send(
        &self,
        method: http::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        let url = self.url(path, query);
        let mut builder = http::Request::builder()
            .method(method)
            .uri(&url)
            .header(http::header::AUTHORIZATION, &self.authorization)
            .header(http::header::ACCEPT, "application/json")
            .header(http::header::USER_AGENT, USER_AGENT);
        let bytes = match body {
            Some(value) => {
                let encoded =
                    serde_json::to_vec(value).map_err(|e| Error::Decode(e.to_string()))?;
                // An explicit Content-Length: the wasi:http client would
                // otherwise stream the body chunked, which some servers (and
                // simple fixtures) do not accept on a request.
                builder = builder
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(http::header::CONTENT_LENGTH, encoded.len());
                Bytes::from(encoded)
            }
            None => Bytes::new(),
        };
        let request = builder
            .body(bytes)
            .map_err(|e| Error::Transport(format!("could not build request for {url}: {e}")))?;
        tracing::debug!(url = %url, "jira request");
        let response = crate::bridge::outbound::fetch(request)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;

        let status = response.status().as_u16();
        let headers = response.headers();
        let content_type = header_str(headers, "content-type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let retry_after = header_str(headers, "retry-after").and_then(|v| v.parse::<u64>().ok());
        let rate_limit_reason = header_str(headers, "ratelimit-reason")
            .or_else(|| header_str(headers, "x-ratelimit-reason"));
        let body = response.body();
        let is_html = content_type.contains("text/html");

        if (200..300).contains(&status) {
            if body.iter().all(u8::is_ascii_whitespace) {
                return Ok(Value::Null);
            }
            if is_html {
                return Err(Error::Html {
                    status,
                    base_url: self.config.base_url.clone(),
                });
            }
            return serde_json::from_slice(body).map_err(|e| {
                Error::Decode(format!("HTTP {status} carried a non-JSON body ({e})"))
            });
        }
        if is_html {
            return Err(Error::Html {
                status,
                base_url: self.config.base_url.clone(),
            });
        }
        let parsed: Option<Value> = serde_json::from_slice(body).ok();
        let (error_messages, errors) = parse_error_collection(parsed.as_ref());
        let raw = if error_messages.is_empty() && errors.is_empty() {
            bound_chars(
                String::from_utf8_lossy(body).trim().to_owned(),
                MAX_ERROR_BODY_CHARS,
            )
        } else {
            String::new()
        };
        Err(Error::Upstream(UpstreamError {
            status,
            error_messages,
            errors,
            raw,
            retry_after,
            rate_limit_reason,
        }))
    }
}

fn header_str(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Decodes Jira's `ErrorCollection` (and the gateway's `{code,message}` /
/// `{error}` shapes) into messages and per-field errors.
fn parse_error_collection(value: Option<&Value>) -> (Vec<String>, BTreeMap<String, String>) {
    let mut messages = Vec::new();
    let mut errors = BTreeMap::new();
    let Some(value) = value else {
        return (messages, errors);
    };
    if let Some(list) = value.get("errorMessages").and_then(Value::as_array) {
        for item in list.iter().take(20) {
            if let Some(text) = item.as_str() {
                messages.push(bound_chars(text.to_owned(), 512));
            }
        }
    }
    if let Some(map) = value.get("errors").and_then(Value::as_object) {
        for (field, message) in map.iter().take(50) {
            let text = match message {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            errors.insert(bound_chars(field.clone(), 128), bound_chars(text, 512));
        }
    }
    for key in ["message", "error", "error_description"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            if messages.is_empty() {
                messages.push(bound_chars(text.to_owned(), 512));
            }
        }
    }
    (messages, errors)
}

/// `k=v&k2=v2` with both sides percent-encoded (the `http` crate does not).
pub fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                utf8_percent_encode(key, QUERY_ENCODE),
                utf8_percent_encode(value, QUERY_ENCODE)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// Cuts `s` to at most `max_chars` characters (never mid-codepoint) and
/// marks the cut.
pub fn bound_chars(mut s: String, max_chars: usize) -> String {
    if let Some((index, _)) = s.char_indices().nth(max_chars) {
        s.truncate(index);
        s.push_str(TRUNCATED);
    }
    s
}

/// Whether a string carries control characters other than tab/newline.
pub fn has_control_chars(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
}

// ---------------------------------------------------------------------------
// JQL helpers
// ---------------------------------------------------------------------------

/// Wraps `jql` as `(<jql>) AND project in (<keys>)`, keeping any trailing
/// `ORDER BY` clause outside the parentheses. An ORDER BY-only query becomes
/// `project in (<keys>) ORDER BY …`, which is bounded.
pub fn apply_projects_filter(jql: &str, keys: &[String]) -> String {
    if keys.is_empty() {
        return jql.to_owned();
    }
    let list = keys.join(", ");
    let (where_part, order_part) = split_order_by(jql);
    let where_part = where_part.trim();
    let mut out = if where_part.is_empty() {
        format!("project in ({list})")
    } else {
        format!("({where_part}) AND project in ({list})")
    };
    if let Some(order) = order_part {
        out.push(' ');
        out.push_str(order.trim());
    }
    out
}

/// Splits a JQL string at the first `ORDER BY` keyword that sits outside
/// single/double quotes (case-insensitive, whole words).
fn split_order_by(jql: &str) -> (&str, Option<&str>) {
    let bytes = jql.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while let Some(&c) = bytes.get(i) {
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                } else if c.eq_ignore_ascii_case(&b'o')
                    && boundary_before(bytes, i)
                    && order_by_at(bytes, i)
                {
                    // `i` indexes an ASCII byte, hence a char boundary.
                    return (&jql[..i], Some(&jql[i..]));
                }
            }
        }
        i += 1;
    }
    (jql, None)
}

fn boundary_before(bytes: &[u8], i: usize) -> bool {
    i == 0
        || bytes
            .get(i - 1)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b')')
}

fn order_by_at(bytes: &[u8], i: usize) -> bool {
    let word = |at: usize, text: &[u8]| -> bool {
        bytes
            .get(at..at + text.len())
            .is_some_and(|slice| slice.eq_ignore_ascii_case(text))
    };
    if !word(i, b"order") {
        return false;
    }
    let mut j = i + 5;
    let mut spaces = 0;
    while bytes.get(j).is_some_and(u8::is_ascii_whitespace) {
        j += 1;
        spaces += 1;
    }
    if spaces == 0 || !word(j, b"by") {
        return false;
    }
    match bytes.get(j + 2) {
        None => true,
        Some(c) => c.is_ascii_whitespace(),
    }
}

// ---------------------------------------------------------------------------
// Atlassian Document Format
// ---------------------------------------------------------------------------

/// Wraps plain text into an ADF document: paragraphs split on blank lines,
/// single newlines become hard breaks.
pub fn text_to_adf(text: &str) -> Value {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut paragraphs = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    let flush = |block: &mut Vec<&str>, paragraphs: &mut Vec<Value>| {
        let mut content = Vec::new();
        for (index, line) in block.iter().enumerate() {
            if index > 0 {
                content.push(json!({"type": "hardBreak"}));
            }
            if !line.is_empty() {
                content.push(json!({"type": "text", "text": line}));
            }
        }
        if !content.is_empty() {
            paragraphs.push(json!({"type": "paragraph", "content": content}));
        }
        block.clear();
    };
    for line in normalized.split('\n') {
        if line.trim().is_empty() {
            flush(&mut block, &mut paragraphs);
        } else {
            block.push(line);
        }
    }
    flush(&mut block, &mut paragraphs);
    if paragraphs.is_empty() {
        paragraphs.push(json!({"type": "paragraph", "content": []}));
    }
    json!({"type": "doc", "version": 1, "content": paragraphs})
}

/// Whether a value is an ADF document object.
pub fn is_adf(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("doc")
}

/// Renders an ADF document (or any ADF node) to readable text, bounded by
/// [`ADF_MAX_CHARS`] and [`ADF_MAX_DEPTH`]. Unknown nodes degrade to their
/// text descendants; media become `[media]` placeholders.
pub fn adf_to_text(node: &Value) -> String {
    let mut out = String::new();
    render_node(node, &mut out, 0, 0);
    let text = out.trim_end().to_owned();
    bound_chars(text, ADF_MAX_CHARS)
}

/// A rich-text field as text: ADF objects rendered, plain strings (v2-style
/// bodies) passed through, anything else `None`.
pub fn rich_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(bound_chars(s.clone(), ADF_MAX_CHARS)),
        Value::Object(_) => Some(adf_to_text(value)),
        _ => None,
    }
}

fn attr_str<'a>(node: &'a Value, name: &str) -> Option<&'a str> {
    node.get("attrs")
        .and_then(|a| a.get(name))
        .and_then(Value::as_str)
}

fn attr_i64(node: &Value, name: &str) -> Option<i64> {
    node.get("attrs").and_then(|a| a.get(name)).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    })
}

fn children(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn render_children(node: &Value, out: &mut String, depth: usize, indent: usize) {
    for child in children(node) {
        render_node(child, out, depth + 1, indent);
    }
}

/// Renders a node's children into a fresh buffer (for list items, quotes,
/// table cells that need post-processing).
fn render_block(node: &Value, depth: usize, indent: usize) -> String {
    let mut tmp = String::new();
    render_children(node, &mut tmp, depth, indent);
    tmp.trim_end().to_owned()
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn render_node(node: &Value, out: &mut String, depth: usize, indent: usize) {
    // Byte-length guard: chars are bounded afterwards, this only stops runaway work.
    if depth > ADF_MAX_DEPTH || out.len() > ADF_MAX_CHARS * 4 {
        return;
    }
    let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "doc" => render_children(node, out, depth, indent),
        "text" => {
            let text = node.get("text").and_then(Value::as_str).unwrap_or("");
            let mut href = None;
            let mut code = false;
            if let Some(marks) = node.get("marks").and_then(Value::as_array) {
                for mark in marks {
                    match mark.get("type").and_then(Value::as_str) {
                        Some("link") => href = attr_str(mark, "href"),
                        Some("code") => code = true,
                        _ => {}
                    }
                }
            }
            if code {
                out.push('`');
                out.push_str(text);
                out.push('`');
            } else {
                out.push_str(text);
            }
            if let Some(href) = href {
                if href != text {
                    out.push_str(" (");
                    out.push_str(href);
                    out.push(')');
                }
            }
        }
        "paragraph" => {
            render_children(node, out, depth, indent);
            ensure_newline(out);
        }
        "heading" => {
            let level = attr_i64(node, "level").unwrap_or(1).clamp(1, 6) as usize;
            ensure_newline(out);
            out.push_str(&"#".repeat(level));
            out.push(' ');
            render_children(node, out, depth, indent);
            ensure_newline(out);
        }
        "bulletList" | "orderedList" | "taskList" | "decisionList" => {
            let start = attr_i64(node, "order").unwrap_or(1).max(0);
            for (index, item) in children(node).iter().enumerate() {
                let marker = match kind {
                    "orderedList" => format!("{}. ", start.saturating_add(index as i64)),
                    "taskList" => {
                        if attr_str(item, "state") == Some("DONE") {
                            "[x] ".to_owned()
                        } else {
                            "[ ] ".to_owned()
                        }
                    }
                    "decisionList" => "→ ".to_owned(),
                    _ => "- ".to_owned(),
                };
                let body = render_block(item, depth + 1, indent + 2);
                out.push_str(&" ".repeat(indent));
                out.push_str(&marker);
                let pad = " ".repeat(indent + 2);
                for (line_index, line) in body.lines().enumerate() {
                    if line_index > 0 {
                        out.push('\n');
                        if !line.is_empty() {
                            out.push_str(&pad);
                        }
                    }
                    out.push_str(line);
                }
                out.push('\n');
            }
        }
        "listItem" | "taskItem" | "decisionItem" => {
            render_children(node, out, depth, indent);
            ensure_newline(out);
        }
        "codeBlock" => {
            ensure_newline(out);
            out.push_str("```");
            if let Some(language) = attr_str(node, "language") {
                out.push_str(language);
            }
            out.push('\n');
            for child in children(node) {
                if let Some(text) = child.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
            ensure_newline(out);
            out.push_str("```\n");
        }
        "blockquote" | "panel" => {
            ensure_newline(out);
            if kind == "panel" {
                out.push_str(&format!(
                    "[{}]\n",
                    attr_str(node, "panelType").unwrap_or("panel")
                ));
            }
            let body = render_block(node, depth, indent);
            for line in body.lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
        }
        "expand" | "nestedExpand" => {
            ensure_newline(out);
            out.push_str("▸ ");
            out.push_str(attr_str(node, "title").unwrap_or("(expand)"));
            out.push('\n');
            render_children(node, out, depth, indent);
        }
        "rule" => {
            ensure_newline(out);
            out.push_str("---\n");
        }
        "hardBreak" => out.push('\n'),
        "mention" => {
            let text = attr_str(node, "text")
                .map(str::to_owned)
                .or_else(|| attr_str(node, "id").map(|id| format!("@{id}")))
                .unwrap_or_else(|| "@unknown".to_owned());
            out.push_str(&text);
        }
        "emoji" => {
            out.push_str(
                attr_str(node, "text")
                    .or_else(|| attr_str(node, "shortName"))
                    .unwrap_or(""),
            );
        }
        "inlineCard" | "blockCard" | "embedCard" => {
            out.push_str(attr_str(node, "url").unwrap_or("[card]"));
            if kind != "inlineCard" {
                out.push('\n');
            }
        }
        "date" => {
            let text = attr_i64(node, "timestamp")
                .map(format_epoch_ms)
                .unwrap_or_else(|| "[date]".to_owned());
            out.push_str(&text);
        }
        "status" => {
            out.push('[');
            out.push_str(attr_str(node, "text").unwrap_or("status"));
            out.push(']');
        }
        "placeholder" => out.push_str(attr_str(node, "text").unwrap_or("")),
        "media" => {
            let label = attr_str(node, "alt")
                .or_else(|| attr_str(node, "id"))
                .unwrap_or("");
            if label.is_empty() {
                out.push_str("[media]");
            } else {
                out.push_str(&format!("[media: {label}]"));
            }
        }
        "mediaSingle" | "mediaGroup" => {
            ensure_newline(out);
            render_children(node, out, depth, indent);
            ensure_newline(out);
        }
        "table" => {
            ensure_newline(out);
            for (row_index, row) in children(node).iter().enumerate() {
                let cells: Vec<String> = children(row)
                    .iter()
                    .map(|cell| render_block(cell, depth + 2, 0).replace('\n', " "))
                    .collect();
                if cells.is_empty() {
                    continue;
                }
                out.push_str("| ");
                out.push_str(&cells.join(" | "));
                out.push_str(" |\n");
                let is_header = children(row)
                    .iter()
                    .all(|cell| cell.get("type").and_then(Value::as_str) == Some("tableHeader"));
                if row_index == 0 && is_header {
                    out.push('|');
                    for _ in &cells {
                        out.push_str(" --- |");
                    }
                    out.push('\n');
                }
            }
        }
        "tableRow" => {
            let cells: Vec<String> = children(node)
                .iter()
                .map(|cell| render_block(cell, depth + 1, 0).replace('\n', " "))
                .collect();
            out.push_str("| ");
            out.push_str(&cells.join(" | "));
            out.push_str(" |\n");
        }
        "extension" | "inlineExtension" | "bodiedExtension" => {
            if children(node).is_empty() {
                out.push_str(&format!(
                    "[{}]",
                    attr_str(node, "extensionKey").unwrap_or("extension")
                ));
            } else {
                render_children(node, out, depth, indent);
            }
        }
        // tableCell, tableHeader, layoutSection, layoutColumn, mediaInline,
        // and anything future: fall back to the text descendants.
        _ => render_children(node, out, depth, indent),
    }
}

/// `YYYY-MM-DD` for an epoch-millisecond timestamp (UTC).
fn format_epoch_ms(ms: i64) -> String {
    if !(-1_000_000_000_000_000..=1_000_000_000_000_000).contains(&ms) {
        return ms.to_string();
    }
    let days = ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 → y/m/d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1).clamp(1, 31) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }).clamp(1, 12) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ---------------------------------------------------------------------------
// Response shaping
// ---------------------------------------------------------------------------

/// A user object reduced to what an agent needs.
pub fn simplify_user(value: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "accountId",
        "displayName",
        "emailAddress",
        "active",
        "accountType",
        "timeZone",
    ] {
        if let Some(v) = value.get(key) {
            if !v.is_null() {
                out.insert(key.to_owned(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// A named entity (status, priority, issue type, resolution, project,
/// component, version) reduced to id/key/name (+ status category).
fn simplify_named(value: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "id",
        "key",
        "name",
        "value",
        "subtask",
        "released",
        "hierarchyLevel",
    ] {
        if let Some(v) = value.get(key) {
            if !v.is_null() {
                out.insert(key.to_owned(), v.clone());
            }
        }
    }
    if let Some(category) = value
        .get("statusCategory")
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str)
    {
        out.insert("statusCategory".to_owned(), json!(category));
    }
    Value::Object(out)
}

/// One issue field, made readable: ADF → text, users and named entities
/// reduced, comments rendered, everything else passed through.
pub fn simplify_field(name: &str, value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            if is_adf(value) {
                json!(adf_to_text(value))
            } else if name == "comment" {
                simplify_comment_page(value)
            } else if map.contains_key("accountId") {
                simplify_user(value)
            } else if map.contains_key("name") || map.contains_key("value") {
                simplify_named(value)
            } else {
                value.clone()
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(200)
                .map(|item| match item {
                    Value::Object(map) if map.contains_key("accountId") => simplify_user(item),
                    Value::Object(map) if map.contains_key("name") || map.contains_key("value") => {
                        simplify_named(item)
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
        Value::String(s) => json!(bound_chars(s.clone(), ADF_MAX_CHARS)),
        other => other.clone(),
    }
}

/// An issue as returned by search or GET /issue, made readable.
pub fn simplify_issue(issue: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "key"] {
        if let Some(v) = issue.get(key) {
            out.insert(key.to_owned(), v.clone());
        }
    }
    if let Some(fields) = issue.get("fields").and_then(Value::as_object) {
        let mut simplified = Map::new();
        for (name, value) in fields {
            if value.is_null() {
                continue;
            }
            simplified.insert(name.clone(), simplify_field(name, value));
        }
        out.insert("fields".to_owned(), Value::Object(simplified));
    }
    for key in [
        "renderedFields",
        "names",
        "changelog",
        "transitions",
        "editmeta",
    ] {
        if let Some(v) = issue.get(key) {
            out.insert(key.to_owned(), v.clone());
        }
    }
    Value::Object(out)
}

/// One comment with its ADF body rendered.
pub fn simplify_comment(comment: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "created", "updated", "jsdPublic"] {
        if let Some(v) = comment.get(key) {
            out.insert(key.to_owned(), v.clone());
        }
    }
    if let Some(author) = comment.get("author") {
        out.insert("author".to_owned(), simplify_user(author));
    }
    if let Some(body) = comment.get("body").and_then(rich_text) {
        out.insert("body".to_owned(), json!(body));
    }
    if let Some(visibility) = comment.get("visibility") {
        out.insert("visibility".to_owned(), visibility.clone());
    }
    Value::Object(out)
}

/// A comment page (`{comments, total, startAt, maxResults}`) rendered.
pub fn simplify_comment_page(page: &Value) -> Value {
    let comments: Vec<Value> = page
        .get("comments")
        .and_then(Value::as_array)
        .map(|list| list.iter().take(200).map(simplify_comment).collect())
        .unwrap_or_default();
    let mut out = Map::new();
    for key in ["total", "startAt", "maxResults"] {
        if let Some(v) = page.get(key) {
            out.insert(key.to_owned(), v.clone());
        }
    }
    out.insert("comments".to_owned(), Value::Array(comments));
    Value::Object(out)
}

/// A project (from /project/search or /project/{key}) reduced.
pub fn simplify_project(project: &Value, site: &str) -> Value {
    let mut out = Map::new();
    for key in [
        "id",
        "key",
        "name",
        "projectTypeKey",
        "style",
        "simplified",
        "isPrivate",
    ] {
        if let Some(v) = project.get(key) {
            if !v.is_null() {
                out.insert(key.to_owned(), v.clone());
            }
        }
    }
    if let Some(description) = project.get("description").and_then(Value::as_str) {
        if !description.is_empty() {
            out.insert(
                "description".to_owned(),
                json!(bound_chars(description.to_owned(), 2000)),
            );
        }
    }
    if let Some(lead) = project.get("lead") {
        if lead.is_object() {
            out.insert("lead".to_owned(), simplify_user(lead));
        }
    }
    for key in ["issueTypes", "components", "versions"] {
        if let Some(list) = project.get(key).and_then(Value::as_array) {
            out.insert(
                key.to_owned(),
                Value::Array(list.iter().take(200).map(simplify_named).collect()),
            );
        }
    }
    if let Some(key) = project.get("key").and_then(Value::as_str) {
        out.insert(
            "url".to_owned(),
            json!(format!("https://{site}/browse/{key}")),
        );
    }
    Value::Object(out)
}

/// One workflow transition reduced, with the ids of required screen fields
/// when the caller expanded `transitions.fields`.
pub fn simplify_transition(transition: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "id",
        "name",
        "hasScreen",
        "isAvailable",
        "isConditional",
        "isGlobal",
        "isInitial",
    ] {
        if let Some(v) = transition.get(key) {
            if !v.is_null() {
                out.insert(key.to_owned(), v.clone());
            }
        }
    }
    if let Some(to) = transition.get("to") {
        out.insert("to".to_owned(), simplify_named(to));
    }
    if let Some(fields) = transition.get("fields").and_then(Value::as_object) {
        let mut required = Vec::new();
        let mut screen = Vec::new();
        for (id, field) in fields.iter().take(200) {
            let is_required = field
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_required {
                required.push(json!(id));
            }
            screen.push(json!({
                "id": id,
                "name": field.get("name").cloned().unwrap_or(Value::Null),
                "required": is_required,
                "type": field.get("schema").and_then(|s| s.get("type")).cloned().unwrap_or(Value::Null),
                "allowedValues": allowed_values(field),
            }));
        }
        out.insert("requiredFields".to_owned(), Value::Array(required));
        out.insert("screenFields".to_owned(), Value::Array(screen));
    }
    Value::Object(out)
}

/// `allowedValues` of a create/transition field reduced to id + name/value.
pub fn allowed_values(field: &Value) -> Value {
    match field.get("allowedValues").and_then(Value::as_array) {
        Some(list) => Value::Array(list.iter().take(100).map(simplify_named).collect()),
        None => Value::Array(Vec::new()),
    }
}

/// A create-metadata field reduced.
pub fn simplify_create_field(field: &Value) -> Value {
    let id = field
        .get("fieldId")
        .or_else(|| field.get("key"))
        .cloned()
        .unwrap_or(Value::Null);
    let schema = field.get("schema");
    json!({
        "id": id,
        "name": field.get("name").cloned().unwrap_or(Value::Null),
        "required": field.get("required").and_then(Value::as_bool).unwrap_or(false),
        "type": schema.and_then(|s| s.get("type")).cloned().unwrap_or(Value::Null),
        "items": schema.and_then(|s| s.get("items")).cloned().unwrap_or(Value::Null),
        "custom": schema.and_then(|s| s.get("custom")).cloned().unwrap_or(Value::Null),
        "hasDefaultValue": field.get("hasDefaultValue").and_then(Value::as_bool).unwrap_or(false),
        "operations": field.get("operations").cloned().unwrap_or(Value::Array(Vec::new())),
        "allowedValues": allowed_values(field),
        "autoCompleteUrl": field.get("autoCompleteUrl").cloned().unwrap_or(Value::Null),
    })
}
