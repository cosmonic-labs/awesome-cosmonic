//! Obsidian Local REST API client.
//!
//! Everything that talks to the "Local REST API with MCP" community plugin
//! (coddingtonbear/obsidian-local-rest-api, MIT) lives here: configuration
//! from the environment, the bearer-key request shape, per-segment path
//! encoding, the plugin's `{errorCode, message}` error envelope mapped to the
//! error catalogue, the version-gated PATCH format, and the small date/time
//! helpers the periodic-note and recent-changes tools need.
//! [`crate::server`] holds the tool definitions and result rendering.
//!
//! Tool naming and semantics follow MarkusPfundstein/mcp-obsidian (MIT); the
//! read-only / command gates follow cyanheads/obsidian-mcp-server
//! (Apache-2.0). Nothing is copied verbatim.

use std::sync::{Mutex, OnceLock};

use bytes::Bytes;
use serde_json::{json, Value};

/// Env var carrying the plugin's API key.
pub const API_KEY_ENV: &str = "OBSIDIAN_API_KEY";
/// Desktop secret reference that injects [`API_KEY_ENV`].
pub const SECRET_REF: &str = "obsidian-mcp-api-key";
/// Upstream base URL override (tests point it at a local fixture).
pub const BASE_URL_ENV: &str = "OBSIDIAN_BASE_URL";
/// The plugin's opt-in plain-HTTP listener, reached through the Desktop
/// loopback sentinel.
pub const DEFAULT_BASE_URL: &str = "http://host.wasmcloud.internal:27123";
/// `true` disables every write tool without calling Obsidian.
pub const READ_ONLY_ENV: &str = "OBSIDIAN_READ_ONLY";
/// `true` enables `list_commands` / `execute_command`.
pub const ENABLE_COMMANDS_ENV: &str = "OBSIDIAN_ENABLE_COMMANDS";
/// Cap on characters of note content returned by one tool call.
pub const MAX_CONTENT_CHARS_ENV: &str = "OBSIDIAN_MAX_CONTENT_CHARS";
pub const DEFAULT_MAX_CONTENT_CHARS: usize = 100_000;
pub const MIN_MAX_CONTENT_CHARS: usize = 1_000;
pub const MAX_MAX_CONTENT_CHARS: usize = 2_000_000;
/// The value a freshly registered-but-unfilled secret ref carries on Cosmonic
/// Desktop; it is treated as "no key" so `GET /` and the tools do not report
/// a working configuration before a real key exists.
pub const PLACEHOLDER_KEY: &str = "REPLACE_ME";
/// Where the plugin (and its API key) comes from.
pub const PLUGIN_URL: &str = "https://github.com/coddingtonbear/obsidian-local-rest-api";
/// Deep link that opens the plugin's page inside a running Obsidian.
pub const PLUGIN_DEEP_LINK: &str = "obsidian://show-plugin?id=obsidian-local-rest-api";
/// The companion plugin that serves `/periodic/` on plugin 5.x.
pub const PERIODIC_PLUGIN_URL: &str =
    "https://github.com/coddingtonbear/obsidian-local-rest-api-periodic-notes";
/// The companion plugin's `errorCode`s on `/periodic/` (its `src/types.ts`;
/// HTTP status = code / 100). Three different conditions come back as 404,
/// so the code — never the status alone — decides what a periodic answer
/// means. Without the companion, the core plugin's generic 404 carries 40400.
pub const PERIODIC_CODE_NOT_ENABLED: u64 = 40060;
pub const PERIODIC_CODE_UNKNOWN_PERIOD: u64 = 40460;
pub const PERIODIC_CODE_NO_NOTE: u64 = 40461;
pub const PERIODIC_CODE_OPERATION_FAILED: u64 = 50060;

/// Media types the plugin negotiates on. Always sent bare, never with a
/// `; charset=` parameter: the plugin's `isContentType` guard on targeted
/// writes (`POST .../heading/..`, every `PATCH`) is an exact string match
/// against markdown-patch's `ContentType` set, and a parameterised value is
/// answered with 400 errorCode 40012.
pub const MT_MARKDOWN: &str = "text/markdown";
pub const MT_NOTE_JSON: &str = "application/vnd.olrapi.note+json";
pub const MT_DOC_MAP: &str = "application/vnd.olrapi.document-map+json";
pub const MT_JSONLOGIC: &str = "application/vnd.olrapi.jsonlogic+json";
pub const MT_PATCH: &str = "application/vnd.olrapi.patch-instruction+json";

/// In-guest bounds (the plugin documents none of these; they keep one call
/// from dragging a multi-megabyte payload through the sandbox).
pub const MAX_PATH_CHARS: usize = 1024;
pub const MAX_PATH_SEGMENTS: usize = 64;
pub const MAX_TARGET_ELEMENTS: usize = 16;
pub const MAX_TARGET_CHARS: usize = 512;
pub const MAX_CONTENT_BYTES: usize = 1024 * 1024;
pub const MAX_QUERY_CHARS: usize = 1000;
pub const MAX_JSONLOGIC_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_FILES: usize = 20;
/// Longest `filepaths` list `batch_get_file_contents` accepts at all (before
/// de-duplication); anything longer is refused in-guest so a client cannot
/// make one call burn CPU on a multi-megabyte list.
pub const MAX_BATCH_INPUT: usize = 200;
pub const MAX_COMMAND_ID_CHARS: usize = 200;
pub const MAX_IF_MATCH_CHARS: usize = 200;
/// Longest slice of an upstream body echoed into an error message.
const SNIPPET_CHARS: usize = 300;

/// Runtime configuration, read from the environment on every call (cheap,
/// and it keeps the instance free of state that could go stale).
#[derive(Debug, Clone)]
pub struct Config {
    /// `None` when the env var is unset, empty, or still the placeholder.
    pub api_key: Option<String>,
    /// The env var holds [`PLACEHOLDER_KEY`] rather than a real key.
    pub key_is_placeholder: bool,
    pub base_url: String,
    pub read_only: bool,
    pub commands_enabled: bool,
    pub max_content_chars: usize,
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn env_flag(name: &str) -> bool {
    non_empty_env(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub fn config() -> Config {
    let base_url = non_empty_env(BASE_URL_ENV)
        .map(|v| v.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let max_content_chars = non_empty_env(MAX_CONTENT_CHARS_ENV)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONTENT_CHARS)
        .clamp(MIN_MAX_CONTENT_CHARS, MAX_MAX_CONTENT_CHARS);
    let raw_key = non_empty_env(API_KEY_ENV);
    let key_is_placeholder = raw_key.as_deref() == Some(PLACEHOLDER_KEY);
    Config {
        api_key: raw_key.filter(|_| !key_is_placeholder),
        key_is_placeholder,
        base_url,
        read_only: env_flag(READ_ONLY_ENV),
        commands_enabled: env_flag(ENABLE_COMMANDS_ENV),
        max_content_chars,
    }
}

/// Human description of the credential, shared by the discovery block and
/// (verbatim) the `desktop.cosmonic.com/credentials` annotation in
/// `deploy/workload.yaml` — the e2e suite fails when the two drift.
pub const CREDENTIAL_DESCRIPTION: &str = "API key shown in Obsidian Settings -> Community plugins -> Local REST API with MCP. The plugin's 'Enable Non-encrypted (HTTP) Server' switch (port 27123) must be on: the default HTTPS listener uses a self-signed certificate the sandbox cannot trust.";
/// What the key lets the tools do (same sharing rule as the description).
pub const CREDENTIAL_SCOPES: [&str; 3] = [
    "vault read",
    "vault write (unless OBSIDIAN_READ_ONLY=true)",
    "commands (only when OBSIDIAN_ENABLE_COMMANDS=true)",
];

/// The `credentials` block of the `GET /` discovery document (presence only,
/// never values). `status` is `configured` only for a real key: the
/// placeholder a freshly created ref carries counts as `missing` (with
/// `placeholder: true` so the reader knows the ref exists but is unfilled).
pub fn credentials() -> Value {
    let cfg = config();
    let status = if cfg.api_key.is_some() {
        "configured"
    } else {
        "missing"
    };
    json!([{
        "ref": SECRET_REF,
        "env": API_KEY_ENV,
        "kind": "bearer-token",
        "status": status,
        "placeholder": cfg.key_is_placeholder,
        "description": CREDENTIAL_DESCRIPTION,
        "obtainUrl": PLUGIN_URL,
        "deepLink": PLUGIN_DEEP_LINK,
        "scopes": CREDENTIAL_SCOPES,
        "validate": "check_auth",
    }])
}

/// The one-paragraph setup instruction shared by the missing-key and
/// invalid-key errors.
pub fn setup_hint() -> String {
    format!(
        "In Obsidian open Settings -> Community plugins, install and enable \"Local REST API \
         with MCP\" ({PLUGIN_URL}; deep link {PLUGIN_DEEP_LINK}), open its settings tab, copy \
         the API key, and switch on \"Enable Non-encrypted (HTTP) Server\" (port 27123 — the \
         default HTTPS listener on 27124 uses a self-signed certificate this sandbox cannot \
         trust). Register the key as the `{SECRET_REF}` secret (env {API_KEY_ENV}): paste it in \
         Cosmonic Desktop -> Secrets, or run cosmonic_set_secret name={SECRET_REF} \
         uri=keychain://cosmonic/{SECRET_REF} env={API_KEY_ENV} value=<key>. Then call \
         check_auth."
    )
}

/// What to do when the sandbox cannot reach the plugin at all.
pub fn transport_hint(detail: &str) -> String {
    let lower = detail.to_ascii_lowercase();
    let grant_missing = lower.contains("denied")
        || (lower.contains("dns") && lower.contains("address not available"))
        || lower.contains("dnserror");
    if grant_missing {
        "A Cosmonic Desktop grant is missing (on Desktop the host.wasmcloud.internal name \
         only resolves while the loopback grants are in place; 'DnsError: address not \
         available' and 'HttpRequestDenied' both mean this): deploy/workload.yaml must list \
         allowedHosts [\"host.wasmcloud.internal:27123\"] and allowedHostLoopbackPorts \
         [\"27123\"], and Desktop Settings -> Security -> 'allow host loopback' must be on. \
         Retrying without changing policy returns the same result."
            .to_owned()
    } else if lower.contains("timed out") {
        "Obsidian did not answer within the outbound deadline (it may be indexing a large \
         vault, or the search walked every note). Retry once; then narrow the request \
         (shorter query, fewer files per batch, a JsonLogic glob on path)."
            .to_owned()
    } else {
        format!(
            "Obsidian is not reachable at that address: the app is closed, the plugin is \
             disabled, or 'Enable Non-encrypted (HTTP) Server' is off (the plugin only \
             listens on HTTPS 27124 by default). Open Obsidian with the vault, enable the HTTP \
             listener in Settings -> Local REST API with MCP, and confirm with \
             `curl http://127.0.0.1:27123/`. {BASE_URL_ENV} currently selects the upstream."
        )
    }
}

/// A failed exchange (or a refusal before any exchange), already classified
/// so tools can render one actionable message and agents can tell retryable
/// from permanent.
#[derive(Debug)]
pub enum Error {
    /// `OBSIDIAN_API_KEY` unset, empty, or still the registration
    /// placeholder — nothing was sent.
    MissingKey { placeholder: bool },
    /// The deployment's configuration is unusable (bad base URL, key with
    /// characters that cannot travel in a header).
    Config(String),
    /// A parameter was refused in-guest; nothing was sent.
    Invalid(String),
    /// A server-side policy gate (read-only, commands disabled, confirm).
    Gated(String),
    /// HTTP 401: the plugin rejected the key.
    Unauthorized { path: String, message: String },
    /// Any other non-2xx answer, with the plugin's error envelope when present.
    Api {
        path: String,
        status: u16,
        error_code: Option<u64>,
        message: String,
        retry_after: Option<String>,
    },
    /// The host could not complete the exchange (DNS, policy, refused, timeout).
    Transport { path: String, detail: String },
    /// A 2xx whose body is not what the endpoint documents.
    Malformed { path: String, detail: String },
}

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }

    /// Whether waiting and retrying could succeed without a human acting.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Transport { detail, .. } => detail.to_ascii_lowercase().contains("timed out"),
            Error::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    /// The caller-facing message: what happened and what to do about it.
    pub fn message(&self) -> String {
        match self {
            Error::MissingKey { placeholder } => format!(
                "{API_KEY_ENV} is not set{}. Copy the API key from Obsidian Settings -> \
                 Community plugins -> Local REST API with MCP and register it as the \
                 {SECRET_REF} secret. {}",
                if *placeholder {
                    format!(
                        " (the `{SECRET_REF}` secret still holds the placeholder value \
                         {PLACEHOLDER_KEY}; overwrite it with the real key)"
                    )
                } else {
                    String::new()
                },
                setup_hint()
            ),
            Error::Config(detail) => format!("obsidian-mcp is misconfigured: {detail}"),
            Error::Invalid(detail) => detail.clone(),
            Error::Gated(detail) => detail.clone(),
            Error::Unauthorized { path, message } => format!(
                "Obsidian rejected the API key (HTTP 401 on {path}: {message}). \
                 {API_KEY_ENV} is missing, stale (the plugin can regenerate it) or belongs to \
                 another vault window; get_server_info reports authenticated=false for the \
                 same reason. Update the `{SECRET_REF}` secret and redeploy; do not retry. {}",
                setup_hint()
            ),
            Error::Api {
                path,
                status,
                error_code,
                message,
                retry_after,
            } => api_message(path, *status, *error_code, message, retry_after.as_deref()),
            Error::Transport { path, detail } => format!(
                "could not reach Obsidian for {path}: {detail}. {}",
                transport_hint(detail)
            ),
            Error::Malformed { path, detail } => format!(
                "Obsidian answered {path} with something this server does not understand: \
                 {detail}. Check that {BASE_URL_ENV} points at the Local REST API plugin \
                 (GET / must return service 'Obsidian Local REST API') and retry once."
            ),
        }
    }
}

fn api_message(
    path: &str,
    status: u16,
    code: Option<u64>,
    message: &str,
    retry_after: Option<&str>,
) -> String {
    let code_text = code.map(|c| format!(" errorCode {c}")).unwrap_or_default();
    let head = format!("Obsidian returned HTTP {status}{code_text} for {path}: {message}");
    let hint = match (code, status) {
        (Some(40101), _) | (_, 401) => format!(
            "The API key was rejected. Update the `{SECRET_REF}` secret (env {API_KEY_ENV}). {}",
            setup_hint()
        ),
        (Some(40021), _) => "The path started with '/' or escaped the vault via '..'. Use a \
                             vault-relative path such as 'Projects/Plan.md'."
            .to_owned(),
        (Some(40005), _) => "The note's YAML frontmatter is not valid YAML, so the plugin \
                             cannot parse or patch it. Read the note, fix the frontmatter with \
                             put_content, then retry."
            .to_owned(),
        (Some(40080 | 40081), _) => "The patch instruction was rejected: heading targets must \
                                     be an array from the top-level heading down, exactly one \
                                     of content/value may be supplied (value only for \
                                     frontmatter fields and table rows), and the operation x \
                                     scope x targetType combination must exist. See \
                                     skill://obsidian-mcp/references/PATCH.md."
            .to_owned(),
        (Some(40083 | 40084), _) => "Legacy header-based patch targeting hit a 5.x plugin \
                                     (the cached plugin version was stale). Call \
                                     get_server_info to refresh it; patch_content then \
                                     switches to the JSON instruction format."
            .to_owned(),
        (Some(40082), _) => "Markdown-Patch-Version was not accepted — this is a bug in \
                             obsidian-mcp, not something you can fix; report it."
            .to_owned(),
        (Some(40070 | 40012), _) => "The JsonLogic body or its Content-Type was rejected. Send \
                                     a JSON object using the documented operators (var, ==, \
                                     !=, in, glob, regexp, and, or, if, >=, <=)."
            .to_owned(),
        (Some(40090), _) => "The search query was empty. Provide a non-empty query.".to_owned(),
        (Some(40510), _) | (_, 405) => "The path is a folder: file operations (read body, \
                                        put, append, patch, delete) only apply to notes. Use \
                                        list_files_in_dir; folders cannot be deleted through \
                                        the API."
            .to_owned(),
        (Some(PERIODIC_CODE_NOT_ENABLED), _) => "That period exists but is switched off. \
                                                 Enable it in Obsidian (the core Daily Notes \
                                                 plugin for daily; the community Periodic \
                                                 Notes plugin for weekly, monthly, quarterly \
                                                 and yearly) or use another period; do not \
                                                 retry unchanged."
            .to_owned(),
        (Some(PERIODIC_CODE_UNKNOWN_PERIOD), _) => "Obsidian has no configuration for that \
                                                    period: daily comes from the core Daily \
                                                    Notes plugin, the other periods from the \
                                                    community Periodic Notes plugin. Enable \
                                                    the one that provides it, or use another \
                                                    period; do not retry unchanged."
            .to_owned(),
        (Some(PERIODIC_CODE_NO_NOTE), _) => "The period is enabled but its note has not been \
                                             created yet. Create it with put_content or \
                                             append_content at the path the Daily Notes / \
                                             Periodic Notes settings produce (their folder + \
                                             date format), or with open_file (Obsidian \
                                             creates it and applies the template), then read \
                                             it again. get_recent_periodic_notes counts such \
                                             periods as skipped."
            .to_owned(),
        (Some(PERIODIC_CODE_OPERATION_FAILED), _) => "The companion plugin failed while \
                                                      resolving the periodic note (usually \
                                                      the Daily Notes / Periodic Notes plugin \
                                                      threw). Retry once after a few seconds; \
                                                      if it persists, check Obsidian's \
                                                      developer console."
            .to_owned(),
        (Some(40400) | None, 404) if path.starts_with("/periodic/") => format!(
            "The /periodic/ route is not served at all, so the companion plugin 'Local REST \
             API - Periodic Notes' ({PERIODIC_PLUGIN_URL}) is not installed or enabled (the \
             core plugin dropped /periodic/ in 5.0.2; an answer from the companion itself \
             carries errorCode 40460 or 40461). Install it, or read the note by path with \
             get_file_contents; do not retry unchanged."
        ),
        (Some(40400), _) | (_, 404) => "Not found: the note, folder (the plugin also answers \
                                        404 for a folder that exists but holds no files), \
                                        heading/block/frontmatter target, active file, or \
                                        route does not exist. For a patch target, read \
                                        get_file_contents format=document_map and copy the \
                                        exact heading text, or set create_target_if_missing."
            .to_owned(),
        (Some(40920), _) | (_, 409) => "The content is already present in the target (or the \
                                        destination exists). Treat it as done; do not retry."
            .to_owned(),
        (_, 412) => "if_match is stale: the note changed since the document map was read. \
                     Re-read get_file_contents format=document_map and re-issue the patch \
                     with the new version token."
            .to_owned(),
        (Some(42200), _) | (_, 422) => "Conflicting target specification — a bug in \
                                        obsidian-mcp; report it."
            .to_owned(),
        (_, 429) => format!(
            "Rate limited{}. Wait before retrying, never in a tight loop, and read fewer \
             files per call.",
            retry_after
                .map(|s| format!(" (Retry-After: {s})"))
                .unwrap_or_default()
        ),
        (_, 403) => "Forbidden: the plugin refused this operation. Check the plugin \
                     settings (binary file access, allowed paths) and do not retry blindly."
            .to_owned(),
        (Some(50010 | 50020), _) | (_, 500..=599) => "Obsidian's search API or vault adapter \
                                                     threw (transient — often during a vault \
                                                     reload). Retry once after a few seconds; \
                                                     if it persists, check Obsidian's \
                                                     developer console."
            .to_owned(),
        (_, 400) => "The plugin rejected the request as malformed; check the parameters \
                     against skill://obsidian-mcp/references/TOOLS.md."
            .to_owned(),
        _ => String::new(),
    };
    if hint.is_empty() {
        head
    } else {
        format!("{head}. {hint}")
    }
}

/// A buffered upstream response.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    pub fn content_type(&self) -> String {
        self.header("content-type").unwrap_or_default()
    }

    /// The body as text, if it is valid UTF-8.
    pub fn text(&self) -> Option<String> {
        std::str::from_utf8(&self.body).ok().map(str::to_owned)
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The plugin's 5-digit `errorCode`, when the body carries the envelope.
    pub fn error_code(&self) -> Option<u64> {
        self.json()
            .and_then(|v| v.get("errorCode").and_then(Value::as_u64))
    }

    pub fn is_redirect(&self) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308)
    }
}

/// The HTTP client: base URL plus (optionally) the bearer key. `GET /` is
/// auth-exempt upstream, so `get_server_info` works without a key; every
/// other tool goes through [`Client::authenticated`].
#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
    api_key: Option<String>,
}

impl Client {
    /// A client that sends the key when one is configured.
    pub fn new(cfg: &Config) -> Result<Self, Error> {
        let base = cfg.base_url.as_str();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(Error::Config(format!(
                "{BASE_URL_ENV} must start with http:// or https:// (got {base:?})"
            )));
        }
        if base.len() > 512 || base.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::Config(format!("{BASE_URL_ENV} is not a valid URL")));
        }
        if let Some(key) = &cfg.api_key {
            if http::HeaderValue::from_str(key).is_err() {
                return Err(Error::Config(format!(
                    "{API_KEY_ENV} contains characters that cannot travel in an HTTP header"
                )));
            }
        }
        Ok(Self {
            base_url: base.to_owned(),
            api_key: cfg.api_key.clone(),
        })
    }

    /// A client that refuses to exist without a key.
    pub fn authenticated(cfg: &Config) -> Result<Self, Error> {
        let client = Self::new(cfg)?;
        if client.api_key.is_none() {
            return Err(Error::MissingKey {
                placeholder: cfg.key_is_placeholder,
            });
        }
        Ok(client)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn has_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// One exchange. `path` is an already-encoded path (+ query) starting
    /// with `/`. A 401 becomes [`Error::Unauthorized`]; every other status is
    /// returned to the caller, who decides with [`Client::ok`].
    pub async fn send(
        &self,
        method: http::Method,
        path: &str,
        headers: &[(&str, String)],
        body: Bytes,
    ) -> Result<Reply, Error> {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = http::Request::builder().method(method).uri(&url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        builder = builder.header(
            "User-Agent",
            concat!("obsidian-mcp/", env!("CARGO_PKG_VERSION")),
        );
        for (name, value) in headers {
            builder = builder.header(*name, value.as_str());
        }
        let request = builder.body(body).map_err(|err| {
            Error::Invalid(format!("could not build the request for {path}: {err}"))
        })?;
        let response = crate::bridge::outbound::fetch(request)
            .await
            .map_err(|err| Error::Transport {
                path: path.to_owned(),
                detail: err.to_string(),
            })?;
        let status = response.status().as_u16();
        let (parts, body) = response.into_parts();
        let reply = Reply {
            status,
            headers: parts.headers,
            body,
        };
        if status == 401 {
            let message = envelope_message(&reply);
            return Err(Error::Unauthorized {
                path: path.to_owned(),
                message,
            });
        }
        Ok(reply)
    }

    /// Turns a non-2xx reply into [`Error::Api`].
    pub fn ok(reply: Reply, path: &str) -> Result<Reply, Error> {
        if reply.is_success() {
            return Ok(reply);
        }
        let error_code = reply.error_code();
        Err(Error::Api {
            path: path.to_owned(),
            status: reply.status,
            error_code,
            message: envelope_message(&reply),
            retry_after: reply.header("retry-after"),
        })
    }

    /// `GET /` — auth-exempt; carries `authenticated` and the plugin version.
    /// Remembers the version for [`cached_plugin_major`].
    pub async fn server_info(&self) -> Result<Value, Error> {
        let reply = self.send(http::Method::GET, "/", &[], Bytes::new()).await?;
        let reply = Self::ok(reply, "/")?;
        let info = reply.json().ok_or_else(|| Error::Malformed {
            path: "/".to_owned(),
            detail: format!(
                "expected the plugin's JSON status document, got {} bytes of {}",
                reply.body.len(),
                reply.content_type()
            ),
        })?;
        if info.get("service").and_then(Value::as_str) != Some("Obsidian Local REST API") {
            return Err(Error::Malformed {
                path: "/".to_owned(),
                detail: format!(
                    "service is {:?}, not 'Obsidian Local REST API'",
                    info.get("service")
                ),
            });
        }
        if let Some(version) = info
            .get("versions")
            .and_then(|v| v.get("self"))
            .and_then(Value::as_str)
        {
            remember_plugin_version(version);
        }
        Ok(info)
    }

    /// The plugin's major version, from the cache or one `GET /`.
    pub async fn plugin_major(&self) -> Result<u32, Error> {
        if let Some(major) = cached_plugin_major() {
            return Ok(major);
        }
        self.server_info().await?;
        Ok(cached_plugin_major().unwrap_or(5))
    }
}

/// The plugin's `{"errorCode": n, "message": "…"}` envelope, or a bounded
/// snippet of whatever the body was.
fn envelope_message(reply: &Reply) -> String {
    if let Some(msg) = reply
        .json()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
    {
        // Bounded like the non-JSON path: the bridge caps a body at 4 MiB,
        // far too much to echo into a tool error.
        let (snippet, dropped) = clamp_chars(msg.trim(), SNIPPET_CHARS);
        return if dropped > 0 {
            format!("{snippet}...[truncated {dropped} chars]")
        } else {
            snippet
        };
    }
    let text = reply.text().unwrap_or_default();
    let (snippet, _) = clamp_chars(text.trim(), SNIPPET_CHARS);
    if snippet.is_empty() {
        format!("(empty {} body)", reply.content_type())
    } else {
        snippet
    }
}

// --- plugin version cache ---------------------------------------------------

fn version_cache() -> &'static Mutex<Option<u32>> {
    static CACHE: OnceLock<Mutex<Option<u32>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Major version of the plugin seen by the last `get_server_info` /
/// `check_auth` on this instance (statics survive across requests on a warm
/// instance). `None` until one succeeded.
pub fn cached_plugin_major() -> Option<u32> {
    version_cache().lock().ok().and_then(|guard| *guard)
}

pub fn remember_plugin_version(version: &str) {
    let major = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok());
    if let (Some(major), Ok(mut guard)) = (major, version_cache().lock()) {
        *guard = Some(major);
    }
}

// --- paths and encoding -----------------------------------------------------

/// RFC 3986 percent-encoding of everything but the unreserved set. Used for
/// each path segment separately (so a real `/` separator survives while a
/// `/` inside a heading becomes `%2F`) and for query values.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-decoding (lenient: malformed escapes are kept as-is).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A validated vault-relative path: the display form and the per-segment
/// encoded form used on the wire.
#[derive(Debug, Clone)]
pub struct VaultPath {
    pub display: String,
    pub encoded: String,
}

/// Validates a vault-relative path. `allow_root` lets the empty string (or
/// `/`-free root) through for directory listings.
pub fn vault_path(raw: &str, allow_root: bool) -> Result<VaultPath, Error> {
    let trimmed = raw.strip_suffix('/').unwrap_or(raw);
    if trimmed.is_empty() {
        if allow_root {
            return Ok(VaultPath {
                display: String::new(),
                encoded: String::new(),
            });
        }
        return Err(Error::invalid("filepath must not be empty"));
    }
    if raw.chars().count() > MAX_PATH_CHARS {
        return Err(Error::invalid(format!(
            "path is longer than {MAX_PATH_CHARS} characters"
        )));
    }
    if trimmed.starts_with('/') {
        return Err(Error::invalid(
            "path must be vault-relative: drop the leading '/' (the plugin answers 400 \
             errorCode 40021 PathTraversalNotAllowed for absolute paths)",
        ));
    }
    if trimmed.contains('\0') {
        return Err(Error::invalid("path must not contain NUL characters"));
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    if segments.len() > MAX_PATH_SEGMENTS {
        return Err(Error::invalid(format!(
            "path has more than {MAX_PATH_SEGMENTS} segments"
        )));
    }
    for segment in &segments {
        if segment.is_empty() {
            return Err(Error::invalid(
                "path must not contain empty segments ('//')",
            ));
        }
        if *segment == ".." || *segment == "." {
            return Err(Error::invalid(
                "path must not contain '.' or '..' segments (the plugin refuses traversal \
                 with 400 errorCode 40021); use a vault-relative path",
            ));
        }
    }
    let encoded = segments
        .iter()
        .map(|s| encode_segment(s))
        .collect::<Vec<_>>()
        .join("/");
    Ok(VaultPath {
        display: trimmed.to_owned(),
        encoded,
    })
}

/// A sub-document target appended to a note URL.
#[derive(Debug, Clone)]
pub enum UrlTarget {
    Heading(Vec<String>),
    Block(String),
    Frontmatter(String),
}

impl UrlTarget {
    pub fn suffix(&self) -> String {
        match self {
            UrlTarget::Heading(path) => {
                let mut out = String::from("/heading");
                for element in path {
                    out.push('/');
                    out.push_str(&encode_segment(element));
                }
                out
            }
            UrlTarget::Block(id) => format!("/block/{}", encode_segment(id)),
            UrlTarget::Frontmatter(key) => format!("/frontmatter/{}", encode_segment(key)),
        }
    }

    pub fn describe(&self) -> Value {
        match self {
            UrlTarget::Heading(path) => json!({"type": "heading", "path": path}),
            UrlTarget::Block(id) => json!({"type": "block", "id": id}),
            UrlTarget::Frontmatter(key) => json!({"type": "frontmatter", "key": key}),
        }
    }
}

/// Validates the elements of a heading path / a block id / a frontmatter key.
pub fn validate_target_text(kind: &str, text: &str) -> Result<(), Error> {
    if text.trim().is_empty() {
        return Err(Error::invalid(format!("{kind} must not be empty")));
    }
    if text.chars().count() > MAX_TARGET_CHARS {
        return Err(Error::invalid(format!(
            "{kind} is longer than {MAX_TARGET_CHARS} characters"
        )));
    }
    if text.contains('\0') {
        return Err(Error::invalid(format!("{kind} must not contain NUL")));
    }
    Ok(())
}

pub fn validate_heading_path(path: &[String]) -> Result<(), Error> {
    if path.is_empty() {
        return Err(Error::invalid(
            "heading path must contain at least one element",
        ));
    }
    if path.len() > MAX_TARGET_ELEMENTS {
        return Err(Error::invalid(format!(
            "heading path has more than {MAX_TARGET_ELEMENTS} elements"
        )));
    }
    for element in path {
        validate_target_text("heading element", element)?;
    }
    Ok(())
}

// --- text bounds ------------------------------------------------------------

/// Cuts `s` to at most `max_chars` characters (never mid-character). Returns
/// the kept text and the number of characters dropped.
pub fn clamp_chars(s: &str, max_chars: usize) -> (String, usize) {
    let total = s.chars().count();
    if total <= max_chars {
        return (s.to_owned(), 0);
    }
    let cut = s
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    (s[..cut].to_owned(), total - max_chars)
}

/// Clamps and appends the standard truncation marker.
pub fn clamp_marked(s: &str, max_chars: usize) -> (String, bool) {
    let (mut kept, dropped) = clamp_chars(s, max_chars);
    if dropped > 0 {
        kept.push_str(&format!("\n...[truncated {dropped} chars]"));
    }
    (kept, dropped > 0)
}

// --- dates ------------------------------------------------------------------

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Per-exchange outbound deadline, as the bridge reads it
/// (`MCP_OUTBOUND_TIMEOUT_MS`, default 30 000).
pub fn outbound_timeout_ms() -> u64 {
    std::env::var("MCP_OUTBOUND_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(30_000)
}

/// Wall-clock budget for a tool that loops over several exchanges
/// (`batch_get_file_contents`, `get_recent_periodic_notes`): twice the
/// per-exchange deadline. Checked between exchanges, so one call holds the
/// instance for at most `budget + one exchange` against a slow-but-answering
/// Obsidian, instead of `N x deadline`.
pub struct Budget {
    started_ms: u64,
    limit_ms: u64,
}

impl Budget {
    pub fn start() -> Self {
        Budget {
            started_ms: now_ms(),
            limit_ms: outbound_timeout_ms().saturating_mul(2),
        }
    }

    pub fn limit_ms(&self) -> u64 {
        self.limit_ms
    }

    pub fn elapsed_ms(&self) -> u64 {
        now_ms().saturating_sub(self.started_ms)
    }

    /// True once the budget is used up (no further exchange should start).
    pub fn spent(&self) -> bool {
        self.elapsed_ms() >= self.limit_ms
    }
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) -> days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Strict `YYYY-MM-DD`.
pub fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

/// `YYYY-MM-DDTHH:MM:SSZ` from epoch milliseconds.
pub fn iso8601_from_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Shifts a (year, month) pair by `delta` months (negative = back).
pub fn shift_months(y: i64, m: u32, delta: i64) -> (i64, u32) {
    let index = y * 12 + (m as i64 - 1) + delta;
    (index.div_euclid(12), (index.rem_euclid(12) + 1) as u32)
}
