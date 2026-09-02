//! Notion REST client: configuration, request building, error mapping, id
//! normalization, and the property helpers the tools share.
//!
//! Every call is a plain HTTPS request to one host (`api.notion.com` by
//! default, `NOTION_BASE_URL` to override) carrying `Authorization: Bearer
//! <NOTION_TOKEN>` and a `Notion-Version` header. The token is read from the
//! environment on every call (Cosmonic injects it from the `notion-mcp-token`
//! secret reference) and is never logged or echoed in an error.
//!
//! Two API versions are supported. `2026-03-11` (the default) uses `in_trash`
//! and the `position` object; `2025-09-03` uses `archived` and `after`. The
//! Markdown endpoints exist only from `2026-03-11`, so the tools that use them
//! always send that version regardless of `NOTION_VERSION`.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};

/// Upstream origin used when `NOTION_BASE_URL` is unset.
pub const DEFAULT_BASE_URL: &str = "https://api.notion.com";
/// Default `Notion-Version`; the current latest.
pub const DEFAULT_VERSION: &str = "2026-03-11";
/// The previous version this server still understands.
pub const LEGACY_VERSION: &str = "2025-09-03";
/// The version the Markdown endpoints require; forced for those calls.
pub const MARKDOWN_VERSION: &str = "2026-03-11";
/// Name of the Desktop secret reference that injects the token.
pub const SECRET_REF: &str = "notion-mcp-token";
/// Environment variable carrying the integration token.
pub const TOKEN_ENV: &str = "NOTION_TOKEN";
/// Where a workspace owner creates the integration and copies the token.
pub const INTEGRATIONS_URL: &str = "https://www.notion.so/profile/integrations";
/// Largest `page_size` Notion accepts on any paginated endpoint.
pub const MAX_PAGE_SIZE: i64 = 100;
/// Longest single rich-text run (characters) Notion accepts.
pub const RICH_TEXT_LIMIT: usize = 2000;
/// Most block children (or rich-text items) in one request.
pub const MAX_ARRAY_ITEMS: usize = 100;
/// Notion's request payload cap.
pub const MAX_PAYLOAD_BYTES: usize = 500 * 1024;
/// How long a data source schema stays cached on a warm instance.
const SCHEMA_TTL: Duration = Duration::from_secs(60);
/// Most schemas kept in the per-instance cache.
const SCHEMA_CACHE_MAX: usize = 32;
/// Longest id / URL input examined by [`normalize_id`].
const MAX_ID_INPUT: usize = 2048;
/// How much of a non-JSON upstream body is quoted in an error.
const BODY_SNIPPET_CHARS: usize = 300;

const USER_AGENT: &str = concat!(
    "notion-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (cosmonic-desktop)"
);

/// Whether `NOTION_TOKEN` is set and non-empty (never its value).
pub fn token_present() -> bool {
    std::env::var(TOKEN_ENV).is_ok_and(|v| !clean_token(&v).is_empty())
}

/// Whether the deployment refuses write tools (`NOTION_READ_ONLY=true`).
pub fn read_only() -> bool {
    std::env::var("NOTION_READ_ONLY").is_ok_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("yes")
    })
}

/// Strips whitespace and stray quotes around a pasted token.
fn clean_token(raw: &str) -> &str {
    raw.trim().trim_matches(|c| c == '"' || c == '\'').trim()
}

/// The actionable message for an unset token: names the variable, the secret
/// reference, and where the credential comes from.
pub fn missing_token_message() -> String {
    format!(
        "{TOKEN_ENV} is not set. Create an internal integration at {INTEGRATIONS_URL} \
         (New integration -> Internal -> pick the workspace -> enable the Read/Update/Insert \
         content, Read/Insert comments and User information capabilities -> copy the Internal \
         Integration Secret, which starts with ntn_), share your pages and databases with it \
         (page ... menu -> Connections -> Connect to <integration>), and register the token as \
         the `{SECRET_REF}` secret (env {TOKEN_ENV}): cosmonic_set_secret name={SECRET_REF} \
         uri=keychain://cosmonic/{SECRET_REF} env={TOKEN_ENV} value=<token>. Then run \
         check_auth."
    )
}

/// The hint appended to an invalid-credential (401) error.
pub fn invalid_token_hint() -> String {
    format!(
        "The token in {TOKEN_ENV} (secret `{SECRET_REF}`) was rejected: it is wrong, revoked, \
         belongs to another workspace, or was pasted with extra characters. Open \
         {INTEGRATIONS_URL}, pick the integration, copy the Internal Integration Secret \
         (ntn_...) from its Configuration tab again, re-register `{SECRET_REF}` \
         (cosmonic_set_secret name={SECRET_REF} uri=keychain://cosmonic/{SECRET_REF} \
         env={TOKEN_ENV} value=<token>), then run check_auth."
    )
}

/// Resolved configuration for one call.
#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    pub base_url: String,
    pub version: String,
}

/// Reads and validates the environment. The returned `Err` is a complete,
/// user-facing tool error message.
pub fn config() -> Result<Config, String> {
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .map(|v| clean_token(&v).to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(missing_token_message)?;
    let base_url = std::env::var("NOTION_BASE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
        return Err(format!(
            "NOTION_BASE_URL must be an http(s) origin without a trailing path (got a value \
             that does not start with http:// or https://); the default is {DEFAULT_BASE_URL}."
        ));
    }
    let version = std::env::var("NOTION_VERSION")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_VERSION.to_owned());
    if version != DEFAULT_VERSION && version != LEGACY_VERSION {
        return Err(format!(
            "NOTION_VERSION={version} is not supported by this server: only {DEFAULT_VERSION} \
             (default) and {LEGACY_VERSION} are understood, because older versions predate \
             data sources. Set NOTION_VERSION in the workload's environment config to one of \
             those (or unset it)."
        ));
    }
    Ok(Config {
        token,
        base_url,
        version,
    })
}

/// A failed upstream exchange, already parsed into Notion's error shape when
/// the body allowed it.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// HTTP status, or 0 for a transport failure (policy, DNS, TLS, timeout).
    pub status: u16,
    /// Notion's `code` (`object_not_found`, `rate_limited`, ...), or a local
    /// label such as `transport`.
    pub code: String,
    pub message: String,
    /// Parsed `Retry-After` seconds, when the upstream sent one.
    pub retry_after: Option<u64>,
    /// `additional_data` from the error body (rate-limit reason, ...).
    pub additional_data: Option<Value>,
}

impl ApiError {
    fn transport(message: String) -> Self {
        Self {
            status: 0,
            code: "transport".to_owned(),
            message,
            retry_after: None,
            additional_data: None,
        }
    }

    /// Whether a retry (after a pause) is reasonable.
    pub fn is_transient(&self) -> bool {
        matches!(self.status, 409 | 429 | 500 | 502 | 503 | 504 | 529)
    }

    /// The full caller-facing text: what happened, what it means, what to do.
    ///
    /// `capability` names the integration capability the calling tool needs
    /// (for 403s); `not_found` is the tool-specific hint for a 404.
    pub fn explain(&self, capability: &str, not_found: &str) -> String {
        if self.status == 0 {
            // Local pre-checks (nothing was sent) carry their own label and
            // must not be explained as a network/policy failure.
            if self.code != "transport" {
                return format!("Request not sent ({}): {}.", self.code, self.message);
            }
            return format!(
                "Notion request failed before a response arrived: {}. If this says the host is \
                 not allowed or the connection was refused, the upstream host (api.notion.com, \
                 or the NOTION_BASE_URL host) must be listed in the workload's allowedHosts in \
                 deploy/workload.yaml and .wash/config.yaml; a timeout means the upstream was \
                 slow (raise MCP_OUTBOUND_TIMEOUT_MS or narrow the request).",
                self.message
            );
        }
        let head = format!(
            "Notion API error {} {}: {}",
            self.status,
            self.code,
            self.message.trim_end_matches('.')
        );
        let hint = match self.status {
            401 => invalid_token_hint(),
            403 => format!(
                "The integration is missing a capability or hit a workspace plan limit. This \
                 tool needs the `{capability}` capability: open {INTEGRATIONS_URL}, pick the \
                 integration, tick it on the Capabilities tab, save, and retry. Do not retry \
                 unchanged."
            ),
            404 => format!(
                "A 404 from Notion almost always means the object is not shared with the \
                 integration rather than that it does not exist: open it in Notion -> ... menu \
                 -> Connections -> Connect to <integration> (children inherit). {not_found} \
                 Do not retry unchanged."
            ),
            400 if self.code == "validation_error" => {
                let msg = self.message.to_ascii_lowercase();
                if msg.contains("old_str") {
                    "The old_str did not match exactly once: make it unique (it is \
                     case-sensitive and must match the page's Markdown rendering) or set \
                     replace_all=true."
                        .to_owned()
                } else if msg.contains("property") || msg.contains("properties") {
                    "The request body, not the token, is wrong: call get_data_source to read \
                     the exact property names and types, then mirror them."
                        .to_owned()
                } else {
                    "The request body was rejected as sent; fix the payload rather than \
                     retrying it unchanged."
                        .to_owned()
                }
            }
            400 if matches!(
                self.code.as_str(),
                "missing_version" | "invalid_request_url" | "invalid_request"
            ) =>
            {
                format!(
                    "The Notion-Version header or endpoint was refused. This server sends \
                     NOTION_VERSION (default {DEFAULT_VERSION}; {LEGACY_VERSION} also accepted) \
                     and forces {MARKDOWN_VERSION} for the Markdown tools; check the workload's \
                     NOTION_VERSION config."
                )
            }
            409 => "Concurrent-edit collision or temporary storage contention: retry the same \
                    call once after about 1 s; if it persists, re-read the page first."
                .to_owned(),
            429 => {
                let reason = self
                    .additional_data
                    .as_ref()
                    .and_then(|d| d.get("rate_limit_reason"))
                    .and_then(Value::as_str)
                    .map(|r| format!(" (reason: {r})"))
                    .unwrap_or_default();
                match self.retry_after {
                    Some(secs) => format!(
                        "Rate limited{reason}: retry_after_seconds={secs}. Wait that long before \
                         calling again, then paginate with page_size<=100 instead of looping \
                         quickly (about 3 requests/second per integration on average)."
                    ),
                    None => format!(
                        "Rate limited{reason}: no Retry-After was sent; wait a few seconds \
                         before calling again and slow down (about 3 requests/second per \
                         integration on average)."
                    ),
                }
            }
            500 | 502 | 503 | 504 | 529 => {
                let extra = self
                    .retry_after
                    .map(|s| format!(" retry_after_seconds={s}."))
                    .unwrap_or_default();
                format!(
                    "Transient Notion-side failure.{extra} Retry with exponential backoff; for \
                     very large Markdown writes split the content or raise MCP_OUTBOUND_TIMEOUT_MS."
                )
            }
            _ => String::new(),
        };
        if hint.is_empty() {
            format!("{head}.")
        } else {
            format!("{head}. {hint}")
        }
    }
}

/// The client: one per tool call, cheap to build.
pub struct Client {
    cfg: Config,
}

impl Client {
    /// Builds a client from the environment, or returns the tool error text.
    pub fn from_env() -> Result<Self, String> {
        Ok(Self { cfg: config()? })
    }

    /// The configured `Notion-Version`.
    pub fn version(&self) -> &str {
        &self.cfg.version
    }

    /// Whether the configured version is the older field-name dialect.
    pub fn legacy(&self) -> bool {
        self.cfg.version == LEGACY_VERSION
    }

    /// The error a write tool returns on a read-only deployment, if any.
    pub fn refuse_write(&self, tool: &str) -> Option<String> {
        read_only().then(|| {
            format!(
                "notion-mcp is deployed read-only (NOTION_READ_ONLY=true), so `{tool}` was not \
                 sent to Notion. Use a deployment with NOTION_READ_ONLY=false for writes."
            )
        })
    }

    /// `GET path?query` under the configured version (or `version` when given).
    pub async fn get(
        &self,
        path: &str,
        query: &[(&str, String)],
        version: Option<&str>,
    ) -> Result<Value, ApiError> {
        self.call("GET", path, query, None, version).await
    }

    /// `POST path` with a JSON body.
    pub async fn post(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: Value,
        version: Option<&str>,
    ) -> Result<Value, ApiError> {
        self.call("POST", path, query, Some(body), version).await
    }

    /// `PATCH path` with a JSON body.
    pub async fn patch(
        &self,
        path: &str,
        body: Value,
        version: Option<&str>,
    ) -> Result<Value, ApiError> {
        self.call("PATCH", path, &[], Some(body), version).await
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        version: Option<&str>,
    ) -> Result<Value, ApiError> {
        let mut url = format!("{}{}", self.cfg.base_url, path);
        for (i, (key, value)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(&encode(value));
        }
        let payload = match &body {
            Some(value) => serde_json::to_vec(value).map_err(|e| {
                ApiError::transport(format!("could not serialize the request body: {e}"))
            })?,
            None => Vec::new(),
        };
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(ApiError {
                status: 0,
                code: "payload_too_large".to_owned(),
                message: format!(
                    "the request body is {} bytes; Notion accepts at most {} bytes per request — \
                     split the content into several calls",
                    payload.len(),
                    MAX_PAYLOAD_BYTES
                ),
                retry_after: None,
                additional_data: None,
            });
        }
        let mut builder = http::Request::builder()
            .method(method)
            .uri(&url)
            .header("Authorization", format!("Bearer {}", self.cfg.token))
            .header("Notion-Version", version.unwrap_or(&self.cfg.version))
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT);
        if body.is_some() {
            // An explicit Content-Length makes the host send a fixed-length
            // body instead of chunked transfer encoding, which not every
            // upstream (nor the e2e fixture) accepts on requests.
            builder = builder
                .header("Content-Type", "application/json")
                .header("Content-Length", payload.len().to_string());
        }
        let request = builder.body(Bytes::from(payload)).map_err(|e| {
            ApiError::transport(format!(
                "could not build the request (is the token or NOTION_BASE_URL malformed?): {e}"
            ))
        })?;
        tracing::debug!(method, path, "notion request");
        let response = crate::bridge::outbound::fetch(request)
            .await
            .map_err(|e| ApiError::transport(e.to_string()))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let bytes = response.into_body();
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        if (200..300).contains(&status) {
            if status == 202 {
                return Err(ApiError {
                    status,
                    code: "async_task".to_owned(),
                    message: "Notion answered 202 with an async task; this server never asks for \
                              asynchronous processing and cannot poll it — retry, and if it \
                              persists split the content"
                        .to_owned(),
                    retry_after,
                    additional_data: parsed,
                });
            }
            return parsed.ok_or_else(|| {
                let (snippet, _) =
                    truncate_chars(&String::from_utf8_lossy(&bytes), BODY_SNIPPET_CHARS);
                ApiError::transport(format!(
                    "upstream answered {status} with a non-JSON body: {snippet:?}"
                ))
            });
        }
        let code = parsed
            .as_ref()
            .and_then(|v| v.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("http_{status}"));
        let message = parsed
            .as_ref()
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                truncate_chars(&String::from_utf8_lossy(&bytes), BODY_SNIPPET_CHARS).0
            });
        Err(ApiError {
            status,
            code,
            message,
            retry_after,
            additional_data: parsed.and_then(|v| v.get("additional_data").cloned()),
        })
    }

    /// The schema (data source object) for a data source, cached for
    /// [`SCHEMA_TTL`] on this instance.
    pub async fn data_source_schema(&self, id: &str) -> Result<Value, ApiError> {
        if let Some(cached) = schema_cache_get(id) {
            return Ok(cached);
        }
        let value = self
            .get(&format!("/v1/data_sources/{id}"), &[], None)
            .await?;
        schema_cache_put(id, &value);
        Ok(value)
    }
}

type SchemaCache = Mutex<Vec<(String, Instant, Value)>>;

fn schema_cache() -> &'static SchemaCache {
    static CACHE: OnceLock<SchemaCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn schema_cache_get(id: &str) -> Option<Value> {
    let cache = schema_cache().lock().ok()?;
    cache
        .iter()
        .find(|(key, at, _)| key == id && at.elapsed() < SCHEMA_TTL)
        .map(|(_, _, value)| value.clone())
}

fn schema_cache_put(id: &str, value: &Value) {
    if let Ok(mut cache) = schema_cache().lock() {
        cache.retain(|(key, at, _)| key != id && at.elapsed() < SCHEMA_TTL);
        if cache.len() >= SCHEMA_CACHE_MAX {
            cache.clear();
        }
        cache.push((id.to_owned(), Instant::now(), value.clone()));
    }
}

/// Percent-encodes a query-string value.
pub fn encode(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// Clamps a caller-supplied page size into Notion's `1..=100`.
pub fn clamp_page_size(requested: Option<i64>, default: i64) -> i64 {
    requested.unwrap_or(default).clamp(1, MAX_PAGE_SIZE)
}

/// Cuts `s` to at most `max_chars` characters (never mid-character).
/// Returns the text and whether anything was cut.
pub fn truncate_chars(s: &str, max_chars: usize) -> (String, bool) {
    match s.char_indices().nth(max_chars) {
        Some((byte_index, _)) => (s[..byte_index].to_owned(), true),
        None => (s.to_owned(), false),
    }
}

/// Like [`truncate_chars`] but prefers to cut at the last line break before
/// the limit, so Markdown is not split mid-line.
pub fn truncate_lines(s: &str, max_chars: usize) -> (String, bool) {
    let (cut, truncated) = truncate_chars(s, max_chars);
    if !truncated {
        return (cut, false);
    }
    match cut.rfind('\n') {
        Some(index) if index > 0 => (cut[..index].to_owned(), true),
        _ => (cut, true),
    }
}

/// Normalizes a page/block/database/data-source/user id given as a 32-hex
/// string, a hyphenated UUID, or a `notion.so` URL into the hyphenated form
/// Notion's paths accept.
pub fn normalize_id(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "an id is required (32 hex characters, a hyphenated UUID, or a notion.so URL)"
                .to_owned(),
        );
    }
    if trimmed.chars().count() > MAX_ID_INPUT {
        return Err(format!(
            "the id/URL is longer than {MAX_ID_INPUT} characters and cannot be a Notion id"
        ));
    }
    let candidate = if trimmed.contains("://")
        || trimmed.starts_with("notion.so")
        || trimmed.starts_with("www.notion.so")
    {
        // Drop the query (?v=<view id> on database URLs is NOT the id) and
        // fragment, then look at the last path segment.
        let path = trimmed
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .trim_end_matches('/');
        path.rsplit('/').next().unwrap_or_default()
    } else {
        trimmed
    };
    let hex: String = candidate
        .chars()
        .rev()
        .filter(|c| *c != '-')
        .take(32)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let bare_len = candidate.chars().filter(|c| *c != '-').count();
    let is_url = candidate != trimmed;
    if hex.chars().count() != 32
        || !hex.chars().all(|c| c.is_ascii_hexdigit())
        || (!is_url && bare_len != 32)
    {
        let (shown, _) = truncate_chars(trimmed, 80);
        return Err(format!(
            "{shown:?} is not a Notion id: pass 32 hex characters, a hyphenated UUID, or the \
             page/database URL from Notion (the id is the trailing 32 hex characters of the \
             URL path; the ?v= parameter on database URLs is a view id, not the data source id)"
        ));
    }
    let hex = hex.to_ascii_lowercase();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// Concatenates the plain text of a Notion rich-text array.
pub fn rich_text_plain(value: Option<&Value>) -> String {
    let mut out = String::new();
    if let Some(items) = value.and_then(Value::as_array) {
        for item in items {
            if let Some(text) = item.get("plain_text").and_then(Value::as_str) {
                out.push_str(text);
            } else if let Some(text) = item
                .get("text")
                .and_then(|t| t.get("content"))
                .and_then(Value::as_str)
            {
                out.push_str(text);
            }
        }
    }
    out
}

/// Splits text into rich-text items of at most [`RICH_TEXT_LIMIT`] characters.
pub fn rich_text_chunks(text: &str) -> Vec<Value> {
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(RICH_TEXT_LIMIT)
        .map(|chunk| {
            let content: String = chunk.iter().collect();
            json!({ "type": "text", "text": { "content": content } })
        })
        .collect()
}

/// The title of a page (its `title`-typed property), database, or data source.
pub fn object_title(object: &Value) -> String {
    if let Some(props) = object.get("properties").and_then(Value::as_object) {
        for prop in props.values() {
            if prop.get("type").and_then(Value::as_str) == Some("title") {
                return rich_text_plain(prop.get("title"));
            }
        }
    }
    rich_text_plain(object.get("title"))
}

/// `{type, id}` for a parent object (`page_id`, `data_source_id`, ...).
pub fn parent_summary(parent: Option<&Value>) -> Value {
    let Some(parent) = parent else {
        return Value::Null;
    };
    let ty = parent.get("type").and_then(Value::as_str).unwrap_or("");
    let id = parent.get(ty).cloned().unwrap_or(Value::Null);
    json!({ "type": ty, "id": id })
}

/// `in_trash` (2026-03-11) or `archived` (older), whichever the object has.
pub fn in_trash(object: &Value) -> Value {
    object
        .get("in_trash")
        .or_else(|| object.get("archived"))
        .cloned()
        .unwrap_or(Value::Bool(false))
}

/// `{id, name}` for a user object.
fn user_summary(user: Option<&Value>) -> Value {
    match user {
        Some(user) => json!({
            "id": user.get("id").cloned().unwrap_or(Value::Null),
            "name": user.get("name").cloned().unwrap_or(Value::Null),
        }),
        None => Value::Null,
    }
}

/// Flattens every property of a page into plain values.
pub fn flatten_properties(properties: Option<&Value>) -> Value {
    let mut out = Map::new();
    if let Some(props) = properties.and_then(Value::as_object) {
        for (name, prop) in props {
            out.insert(name.clone(), flatten_property(prop));
        }
    }
    Value::Object(out)
}

/// One Notion property-value object to a plain value.
pub fn flatten_property(prop: &Value) -> Value {
    let ty = prop.get("type").and_then(Value::as_str).unwrap_or("");
    let inner = prop.get(ty);
    match ty {
        "title" | "rich_text" => Value::String(rich_text_plain(inner)),
        "select" | "status" => inner
            .and_then(|v| v.get("name"))
            .cloned()
            .unwrap_or(Value::Null),
        "multi_select" => Value::Array(
            inner
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i.get("name").cloned())
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "date" => inner.cloned().unwrap_or(Value::Null),
        "people" => Value::Array(
            inner
                .and_then(Value::as_array)
                .map(|items| items.iter().map(|u| user_summary(Some(u))).collect())
                .unwrap_or_default(),
        ),
        "relation" => {
            let ids: Vec<Value> = inner
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(|i| i.get("id").cloned()).collect())
                .unwrap_or_default();
            if prop.get("has_more").and_then(Value::as_bool) == Some(true) {
                json!({ "ids": ids, "has_more": true })
            } else {
                Value::Array(ids)
            }
        }
        "number" | "checkbox" | "url" | "email" | "phone_number" | "created_time"
        | "last_edited_time" => inner.cloned().unwrap_or(Value::Null),
        "formula" => flatten_typed(inner),
        "rollup" => match inner.and_then(|r| r.get("type")).and_then(Value::as_str) {
            Some("array") => Value::Array(
                inner
                    .and_then(|r| r.get("array"))
                    .and_then(Value::as_array)
                    .map(|items| items.iter().map(flatten_property).collect())
                    .unwrap_or_default(),
            ),
            _ => flatten_typed(inner),
        },
        "created_by" | "last_edited_by" => user_summary(inner),
        "files" => Value::Array(
            inner
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|f| {
                            let kind = f.get("type").and_then(Value::as_str).unwrap_or("");
                            json!({
                                "name": f.get("name").cloned().unwrap_or(Value::Null),
                                "url": f.get(kind).and_then(|k| k.get("url")).cloned().unwrap_or(Value::Null),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "unique_id" => {
            let number = inner.and_then(|u| u.get("number")).cloned();
            match (
                inner.and_then(|u| u.get("prefix")).and_then(Value::as_str),
                number,
            ) {
                (Some(prefix), Some(n)) => Value::String(format!("{prefix}-{n}")),
                (None, Some(n)) => n,
                _ => Value::Null,
            }
        }
        "verification" => inner
            .and_then(|v| v.get("state"))
            .cloned()
            .unwrap_or(Value::Null),
        _ => inner.cloned().unwrap_or(Value::Null),
    }
}

/// `{type: "number", number: 3}`-style objects to their value.
fn flatten_typed(value: Option<&Value>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
    value.get(ty).cloned().unwrap_or(Value::Null)
}

/// Compact page summary shared by get_page / query rows / create results.
pub fn page_summary(page: &Value, raw_properties: bool) -> Value {
    json!({
        "id": page.get("id").cloned().unwrap_or(Value::Null),
        "url": page.get("url").cloned().unwrap_or(Value::Null),
        "title": object_title(page),
        "parent": parent_summary(page.get("parent")),
        "in_trash": in_trash(page),
        "created_time": page.get("created_time").cloned().unwrap_or(Value::Null),
        "last_edited_time": page.get("last_edited_time").cloned().unwrap_or(Value::Null),
        "icon": page.get("icon").cloned().unwrap_or(Value::Null),
        "properties": if raw_properties {
            page.get("properties").cloned().unwrap_or(Value::Null)
        } else {
            flatten_properties(page.get("properties"))
        },
    })
}

/// Summary of a data source's schema: one entry per property.
pub fn schema_summary(source: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(props) = source.get("properties").and_then(Value::as_object) {
        for (name, prop) in props {
            let ty = prop.get("type").and_then(Value::as_str).unwrap_or("");
            let mut entry = Map::new();
            entry.insert("name".into(), Value::String(name.clone()));
            entry.insert("id".into(), prop.get("id").cloned().unwrap_or(Value::Null));
            entry.insert("type".into(), Value::String(ty.to_owned()));
            match ty {
                "select" | "multi_select" | "status" => {
                    let options: Vec<Value> = prop
                        .get(ty)
                        .and_then(|t| t.get("options"))
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|o| o.get("name").cloned())
                                .collect()
                        })
                        .unwrap_or_default();
                    entry.insert("options".into(), Value::Array(options));
                    if ty == "status" {
                        if let Some(groups) = prop.get(ty).and_then(|t| t.get("groups")) {
                            entry.insert("groups".into(), groups.clone());
                        }
                    }
                }
                "relation" => {
                    if let Some(target) = prop.get(ty).and_then(|t| t.get("data_source_id")) {
                        entry.insert("data_source_id".into(), target.clone());
                    }
                    if let Some(target) = prop.get(ty).and_then(|t| t.get("database_id")) {
                        entry.insert("database_id".into(), target.clone());
                    }
                }
                "formula" => {
                    if let Some(expr) = prop.get(ty).and_then(|t| t.get("expression")) {
                        entry.insert("expression".into(), expr.clone());
                    }
                }
                "rollup" | "number" | "unique_id" => {
                    if let Some(detail) = prop.get(ty) {
                        entry.insert("config".into(), detail.clone());
                    }
                }
                _ => {}
            }
            out.push(Value::Object(entry));
        }
    }
    out
}

/// Name of the `title`-typed property in a data source schema.
pub fn title_property_name(source: &Value) -> Option<String> {
    source
        .get("properties")
        .and_then(Value::as_object)?
        .iter()
        .find(|(_, prop)| prop.get("type").and_then(Value::as_str) == Some("title"))
        .map(|(name, _)| name.clone())
}

/// Resolves a caller-supplied property name against the schema: exact match
/// first, then a unique case-insensitive match.
fn resolve_property_name<'a>(
    schema: &'a Map<String, Value>,
    requested: &str,
) -> Result<&'a str, String> {
    if let Some((name, _)) = schema.get_key_value(requested) {
        return Ok(name.as_str());
    }
    let matches: Vec<&str> = schema
        .keys()
        .filter(|k| k.eq_ignore_ascii_case(requested))
        .map(String::as_str)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => {
            let mut names: Vec<&str> = schema.keys().map(String::as_str).collect();
            names.sort_unstable();
            Err(format!(
                "`{requested}` is not a property of this data source; available properties: {}",
                names.join(", ")
            ))
        }
        many => Err(format!(
            "`{requested}` is ambiguous (case-insensitively matches {}); use the exact name",
            many.join(", ")
        )),
    }
}

/// Coerces plain values into Notion property-value objects using the data
/// source schema. `title` (if given) sets the title property.
pub fn coerce_properties(
    source: &Value,
    title: Option<&str>,
    plain: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>, String> {
    let empty = Map::new();
    let schema = source
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut out = Map::new();
    if let Some(title) = title {
        let name = title_property_name(source)
            .ok_or_else(|| "this data source has no title property".to_owned())?;
        out.insert(name, json!({ "title": rich_text_chunks(title) }));
    }
    if let Some(plain) = plain {
        for (requested, value) in plain {
            let name = resolve_property_name(schema, requested)?;
            let ty = schema
                .get(name)
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            out.insert(name.to_owned(), coerce_property(name, ty, value)?);
        }
    }
    Ok(out)
}

fn expect_string<'a>(name: &str, ty: &str, value: &'a Value) -> Result<&'a str, String> {
    value.as_str().ok_or_else(|| {
        format!(
            "property `{name}` ({ty}) expects a string, got {}",
            kind_of(value)
        )
    })
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// One plain value to a Notion property-value object for a property of `ty`.
pub fn coerce_property(name: &str, ty: &str, value: &Value) -> Result<Value, String> {
    // A value that is already a Notion property object passes through.
    if value.get(ty).is_some() {
        return Ok(value.clone());
    }
    let unsupported = || {
        format!(
            "property `{name}` has type `{ty}`, which this tool cannot set from a plain value \
             (files, rollup, formula, verification, button, place and the created/edited \
             metadata are read-only or need raw objects); use create_page with raw `properties`"
        )
    };
    let string_list = |value: &Value| -> Result<Vec<String>, String> {
        match value {
            Value::String(s) => Ok(vec![s.clone()]),
            Value::Array(items) => items
                .iter()
                .map(|i| {
                    i.as_str().map(str::to_owned).ok_or_else(|| {
                        format!(
                            "property `{name}` ({ty}) expects strings, got {}",
                            kind_of(i)
                        )
                    })
                })
                .collect(),
            other => Err(format!(
                "property `{name}` ({ty}) expects a string or an array of strings, got {}",
                kind_of(other)
            )),
        }
    };
    let result = match ty {
        "title" | "rich_text" => {
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                other => return Err(expect_string(name, ty, other).unwrap_err()),
            };
            json!({ ty: rich_text_chunks(&text) })
        }
        "number" => match value {
            Value::Null => json!({ "number": null }),
            Value::Number(n) => json!({ "number": n }),
            Value::String(s) => {
                let parsed: f64 = s.trim().parse().map_err(|_| {
                    format!("property `{name}` (number) expects a number, got the string {s:?}")
                })?;
                if !parsed.is_finite() {
                    return Err(format!("property `{name}` (number) must be finite"));
                }
                json!({ "number": parsed })
            }
            other => {
                return Err(format!(
                    "property `{name}` (number) expects a number, got {}",
                    kind_of(other)
                ))
            }
        },
        "checkbox" => match value {
            Value::Bool(b) => json!({ "checkbox": b }),
            Value::String(s)
                if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") =>
            {
                json!({ "checkbox": s.eq_ignore_ascii_case("true") })
            }
            other => {
                return Err(format!(
                    "property `{name}` (checkbox) expects true or false, got {}",
                    kind_of(other)
                ))
            }
        },
        "select" | "status" => match value {
            Value::Null => json!({ ty: null }),
            other => json!({ ty: { "name": expect_string(name, ty, other)? } }),
        },
        "multi_select" => {
            let names = match value {
                Value::Null => Vec::new(),
                other => string_list(other)?,
            };
            if names.len() > MAX_ARRAY_ITEMS {
                return Err(format!(
                    "property `{name}` (multi_select) accepts at most {MAX_ARRAY_ITEMS} options"
                ));
            }
            json!({ "multi_select": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() })
        }
        "date" => match value {
            Value::Null => json!({ "date": null }),
            Value::String(s) => json!({ "date": { "start": s } }),
            Value::Object(o) if o.contains_key("start") => json!({ "date": o }),
            other => {
                return Err(format!(
                    "property `{name}` (date) expects an ISO-8601 string or {{start, end?, \
                     time_zone?}}, got {}",
                    kind_of(other)
                ))
            }
        },
        "url" | "email" | "phone_number" => match value {
            Value::Null => json!({ ty: null }),
            other => json!({ ty: expect_string(name, ty, other)? }),
        },
        "relation" => {
            let ids = match value {
                Value::Null => Vec::new(),
                other => string_list(other)?,
            };
            if ids.len() > MAX_ARRAY_ITEMS {
                return Err(format!(
                    "property `{name}` (relation) accepts at most {MAX_ARRAY_ITEMS} related pages"
                ));
            }
            let mut related = Vec::with_capacity(ids.len());
            for id in ids {
                let id =
                    normalize_id(&id).map_err(|e| format!("property `{name}` (relation): {e}"))?;
                related.push(json!({ "id": id }));
            }
            json!({ "relation": related })
        }
        "people" => {
            let ids = match value {
                Value::Null => Vec::new(),
                other => string_list(other)?,
            };
            if ids.len() > MAX_ARRAY_ITEMS {
                return Err(format!(
                    "property `{name}` (people) accepts at most {MAX_ARRAY_ITEMS} users"
                ));
            }
            let mut people = Vec::with_capacity(ids.len());
            for id in ids {
                let id =
                    normalize_id(&id).map_err(|e| format!("property `{name}` (people): {e}"))?;
                people.push(json!({ "object": "user", "id": id }));
            }
            json!({ "people": people })
        }
        "" => {
            return Err(format!(
                "property `{name}` has no type in the schema; call get_data_source and retry"
            ))
        }
        _ => return Err(unsupported()),
    };
    Ok(result)
}

/// Compact summary of one block for get_block_children.
pub fn block_summary(block: &Value) -> Value {
    let ty = block.get("type").and_then(Value::as_str).unwrap_or("");
    let inner = block.get(ty);
    let mut entry = Map::new();
    entry.insert("id".into(), block.get("id").cloned().unwrap_or(Value::Null));
    entry.insert("type".into(), Value::String(ty.to_owned()));
    entry.insert(
        "has_children".into(),
        block
            .get("has_children")
            .cloned()
            .unwrap_or(Value::Bool(false)),
    );
    if let Some(rich) = inner.and_then(|i| i.get("rich_text")) {
        entry.insert("text".into(), Value::String(rich_text_plain(Some(rich))));
    }
    match ty {
        "to_do" => {
            entry.insert(
                "checked".into(),
                inner
                    .and_then(|i| i.get("checked"))
                    .cloned()
                    .unwrap_or(Value::Bool(false)),
            );
        }
        "code" => {
            entry.insert(
                "language".into(),
                inner
                    .and_then(|i| i.get("language"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        "child_page" | "child_database" => {
            entry.insert(
                "title".into(),
                inner
                    .and_then(|i| i.get("title"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        "bookmark" | "embed" | "link_preview" => {
            entry.insert(
                "url".into(),
                inner
                    .and_then(|i| i.get("url"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        "image" | "file" | "pdf" | "video" | "audio" => {
            let kind = inner
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            entry.insert(
                "url".into(),
                inner
                    .and_then(|i| i.get(kind))
                    .and_then(|k| k.get("url"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        "synced_block" => {
            entry.insert(
                "synced_from".into(),
                inner
                    .and_then(|i| i.get("synced_from"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        "table" => {
            entry.insert(
                "table_width".into(),
                inner
                    .and_then(|i| i.get("table_width"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        _ => {}
    }
    Value::Object(entry)
}

/// Compact summary of one comment.
pub fn comment_summary(comment: &Value) -> Value {
    json!({
        "id": comment.get("id").cloned().unwrap_or(Value::Null),
        "discussion_id": comment.get("discussion_id").cloned().unwrap_or(Value::Null),
        "parent": parent_summary(comment.get("parent")),
        "created_time": comment.get("created_time").cloned().unwrap_or(Value::Null),
        "created_by": user_summary(comment.get("created_by")),
        "display_name": comment
            .get("display_name")
            .and_then(|d| d.get("resolved_name"))
            .cloned()
            .unwrap_or(Value::Null),
        "text": rich_text_plain(comment.get("rich_text")),
        "attachments": comment
            .get("attachments")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
    })
}

/// Compact summary of one user.
pub fn user_entry(user: &Value) -> Value {
    let mut entry = Map::new();
    entry.insert("id".into(), user.get("id").cloned().unwrap_or(Value::Null));
    entry.insert(
        "type".into(),
        user.get("type").cloned().unwrap_or(Value::Null),
    );
    entry.insert(
        "name".into(),
        user.get("name").cloned().unwrap_or(Value::Null),
    );
    if let Some(email) = user.get("person").and_then(|p| p.get("email")) {
        entry.insert("email".into(), email.clone());
    }
    if let Some(workspace) = user.get("bot").and_then(|b| b.get("workspace_name")) {
        entry.insert("workspace_name".into(), workspace.clone());
    }
    Value::Object(entry)
}

/// Compact summary of one search result (page or data source).
pub fn search_entry(item: &Value) -> Value {
    let object = item.get("object").and_then(Value::as_str).unwrap_or("");
    let mut entry = Map::new();
    entry.insert("object".into(), Value::String(object.to_owned()));
    entry.insert("id".into(), item.get("id").cloned().unwrap_or(Value::Null));
    entry.insert("title".into(), Value::String(object_title(item)));
    entry.insert(
        "url".into(),
        item.get("url").cloned().unwrap_or(Value::Null),
    );
    entry.insert("parent".into(), parent_summary(item.get("parent")));
    entry.insert(
        "last_edited_time".into(),
        item.get("last_edited_time").cloned().unwrap_or(Value::Null),
    );
    if object == "data_source" {
        entry.insert(
            "database_id".into(),
            item.get("database_parent")
                .and_then(|p| p.get("database_id"))
                .or_else(|| item.get("parent").and_then(|p| p.get("database_id")))
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(entry)
}

/// The pagination trailer every list result carries.
pub fn pagination(list: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert(
        "has_more".into(),
        list.get("has_more").cloned().unwrap_or(Value::Bool(false)),
    );
    out.insert(
        "next_cursor".into(),
        list.get("next_cursor").cloned().unwrap_or(Value::Null),
    );
    if let Some(status) = list.get("request_status") {
        if status.get("type").and_then(Value::as_str) == Some("incomplete") {
            out.insert("request_status".into(), status.clone());
        }
    }
    out
}
