//! Client for the official Playwright MCP server (microsoft/playwright-mcp,
//! Apache-2.0) in its streamable-HTTP mode.
//!
//! A browser cannot run inside a WebAssembly component, so this server is a
//! *proxy*: `@playwright/mcp` runs natively on the developer's machine
//! (`npx @playwright/mcp@latest --port 8931 --host 127.0.0.1 --headless
//! --allowed-hosts host.wasmcloud.internal:8931,localhost:8931`) and this
//! module speaks its wire protocol over `wasi:http`:
//!
//! - one `initialize` + `notifications/initialized` per warm component
//!   instance, after which the upstream's `Mcp-Session-Id` (one browser
//!   session) is cached in a static and reused for every call;
//! - JSON-RPC over POST with `Accept: application/json, text/event-stream`;
//!   replies come back SSE-framed (`event: message` / `data: {...}`) and are
//!   parsed here;
//! - `404 Session not found` / `400 Server not initialized` (upstream
//!   restarted, session evicted) trigger exactly one transparent
//!   re-initialize and retry — page state is gone, the call still succeeds;
//! - `DELETE` with the session id closes the upstream session
//!   (`playwright_reset_session`).
//!
//! The module also holds the upstream knowledge the proxy applies on top of
//! the passthrough: which tools are hidden unless `PLAYWRIGHT_ALLOW_UNSAFE`
//! is on, which parameters are stripped (`filename` — files would land on
//! the developer's disk, invisible to the sandbox), the argument clamps, ANSI
//! stripping, and the snapshot-inlining merge. [`crate::server`] holds the
//! tool surface and result rendering.
//!
//! Nothing is copied from the upstream project; this is an independent
//! implementation of its documented HTTP behaviour (verified against
//! `@playwright/mcp` 0.0.80 on 2026-09-02).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{json, Map, Value};

/// Full URL of the upstream streamable-HTTP endpoint (tests point it at a
/// local fixture).
pub const BASE_URL_ENV: &str = "PLAYWRIGHT_BASE_URL";
/// The `npx @playwright/mcp --port 8931` endpoint on the developer's machine,
/// reached through the Desktop loopback sentinel.
pub const DEFAULT_BASE_URL: &str = "http://host.wasmcloud.internal:8931/mcp";
/// The port the default URL and the manifest's loopback grant agree on.
pub const DEFAULT_PORT: &str = "8931";
/// `true` lists and forwards the arbitrary-code / host-path tools.
pub const ALLOW_UNSAFE_ENV: &str = "PLAYWRIGHT_ALLOW_UNSAFE";
/// `false` disables the snapshot-inlining round-trip after actions.
pub const AUTO_SNAPSHOT_ENV: &str = "PLAYWRIGHT_AUTO_SNAPSHOT";
/// Seconds the upstream `tools/list` is cached per warm instance.
pub const TOOLS_TTL_ENV: &str = "PLAYWRIGHT_TOOLS_TTL_SECS";
/// Optional bearer token for a hosted upstream behind an authenticating
/// proxy. The local `npx` server has no authentication.
pub const TOKEN_ENV: &str = "PLAYWRIGHT_BEARER_TOKEN";
/// Desktop secret reference that injects [`TOKEN_ENV`].
pub const SECRET_REF: &str = "playwright-mcp-token";
/// Where the upstream (and its flags) is documented.
pub const UPSTREAM_PROJECT_URL: &str = "https://github.com/microsoft/playwright-mcp";
/// The MCP revision the upstream negotiates (it does not speak 2026-07-28).
pub const UPSTREAM_PROTOCOL_VERSION: &str = "2025-11-25";
/// The Desktop loopback sentinel hostname.
pub const LOOPBACK_HOST: &str = "host.wasmcloud.internal";

pub const DEFAULT_TOOLS_TTL_SECS: u64 = 300;
pub const MAX_TOOLS_TTL_SECS: u64 = 86_400;

/// Tools that run arbitrary JavaScript in the page / Playwright process or
/// read paths on the developer's machine. Hidden and refused unless
/// `PLAYWRIGHT_ALLOW_UNSAFE=true`.
pub const UNSAFE_TOOLS: &[&str] = &[
    "browser_run_code_unsafe",
    "browser_evaluate",
    "browser_file_upload",
    "browser_drop",
];

/// Parameters removed from every schema and every call: a `filename` diverts
/// the result to a file on the developer's disk, which the sandbox cannot
/// read, so the proxy always asks for inline results.
pub const STRIPPED_PARAMS: &[&str] = &["filename"];

/// Upper bounds applied in-guest before forwarding (see
/// [`sanitize_arguments`]).
pub const MAX_WAIT_SECS: f64 = 60.0;
pub const MAX_SNAPSHOT_DEPTH: f64 = 64.0;
pub const MAX_VIEWPORT_PX: f64 = 8192.0;

/// Longest text block returned from one upstream content item (bytes, cut on
/// a char boundary). The outbound reply itself is bounded by
/// `MCP_OUTBOUND_MAX_BYTES`; this keeps a single snapshot from flooding the
/// client context.
pub const MAX_TEXT_BYTES: usize = 1_000_000;
/// Most content blocks kept from one upstream result.
pub const MAX_CONTENT_BLOCKS: usize = 64;
/// Most tools accepted from the upstream list.
const MAX_UPSTREAM_TOOLS: usize = 256;
/// Longest slice of an upstream body echoed into an error message.
const SNIPPET_CHARS: usize = 300;
/// Bound on the SSE reply parser (events, not bytes — bytes are bounded by
/// the bridge).
const MAX_SSE_EVENTS: usize = 256;

/// Runtime configuration, read from the environment on every call.
#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub allow_unsafe: bool,
    pub auto_snapshot: bool,
    pub tools_ttl_secs: u64,
    pub token: Option<String>,
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn env_flag(name: &str, default: bool) -> bool {
    match non_empty_env(name) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => default,
    }
}

pub fn config() -> Config {
    let base_url = non_empty_env(BASE_URL_ENV)
        .map(|v| v.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let tools_ttl_secs = non_empty_env(TOOLS_TTL_ENV)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TOOLS_TTL_SECS)
        .min(MAX_TOOLS_TTL_SECS);
    Config {
        base_url,
        allow_unsafe: env_flag(ALLOW_UNSAFE_ENV, false),
        auto_snapshot: env_flag(AUTO_SNAPSHOT_ENV, true),
        tools_ttl_secs,
        token: non_empty_env(TOKEN_ENV),
    }
}

impl Config {
    /// `host[:port]` of the base URL, for messages.
    pub fn authority(&self) -> String {
        let rest = self
            .base_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.base_url);
        rest.split(['/', '?', '#']).next().unwrap_or("").to_owned()
    }

    /// Whether the base URL goes through the Desktop loopback sentinel.
    pub fn via_loopback(&self) -> bool {
        let authority = self.authority();
        let host = authority
            .rsplit_once(':')
            .map_or(authority.as_str(), |(h, _)| h);
        host.eq_ignore_ascii_case(LOOPBACK_HOST)
    }

    /// Port of the base URL (explicit, or the scheme default).
    pub fn port(&self) -> String {
        let authority = self.authority();
        match authority.rsplit_once(':') {
            Some((_, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                port.to_owned()
            }
            _ if self.base_url.starts_with("https://") => "443".to_owned(),
            _ => "80".to_owned(),
        }
    }

    /// The `npx` command line that makes the upstream reachable from this
    /// sandbox: IPv4 bind (Desktop resolves the sentinel to 127.0.0.1;
    /// `localhost` alone binds `::1` on many machines) plus the Host
    /// allow-list the sandbox cannot influence.
    pub fn launch_command(&self) -> String {
        let port = self.port();
        format!(
            "npx @playwright/mcp@latest --port {port} --host 127.0.0.1 --headless \
             --allowed-hosts {LOOPBACK_HOST}:{port},localhost:{port}"
        )
    }
}

/// The `credentials` block of the `GET /` discovery document (presence only,
/// never the value). The token is optional: only a hosted upstream behind an
/// authenticating proxy needs it.
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
        "required": false,
        "status": status,
        "description": "Optional. Sent as `Authorization: Bearer <token>` on every upstream request; only needed when PLAYWRIGHT_BASE_URL points at a hosted Playwright MCP endpoint behind an authenticating reverse proxy. The local `npx @playwright/mcp` server has no authentication and needs no token.",
        "obtainUrl": UPSTREAM_PROJECT_URL,
        "scopes": [],
        "validate": "playwright_status",
    }])
}

/// The one-paragraph setup instruction for the missing/invalid-token errors.
pub fn token_hint(cfg: &Config) -> String {
    format!(
        "The local `npx @playwright/mcp` server needs no token; only a hosted Playwright MCP \
         endpoint behind an authenticating proxy does. Take the bearer token that proxy issues \
         and register it as the `{SECRET_REF}` secret (env {TOKEN_ENV}): paste it in Cosmonic \
         Desktop -> Secrets, or run cosmonic_set_secret name={SECRET_REF} \
         uri=keychain://cosmonic/{SECRET_REF} env={TOKEN_ENV} value=<token>, then list \
         secretFrom [{{name: {SECRET_REF}}}] in deploy/workload.yaml and call playwright_status. \
         (PLAYWRIGHT_BASE_URL is {base}.)",
        base = cfg.base_url
    )
}

/// What to do when the sandbox cannot reach the upstream at all.
pub fn transport_hint(cfg: &Config, detail: &str) -> String {
    let port = cfg.port();
    let lower = detail.to_ascii_lowercase();
    let grant = if cfg.via_loopback() {
        format!(
            " The sandbox reaches your machine only through the three Cosmonic Desktop loopback \
             grants: deploy/workload.yaml allowedHosts [\"{LOOPBACK_HOST}:{port}\"], \
             allowedHostLoopbackPorts [\"{port}\"], and Settings -> Security -> 'allow host \
             loopback' (off by default)."
        )
    } else {
        format!(
            " The host of {} must be listed in the workload's allowedHosts.",
            cfg.base_url
        )
    };
    let cause = if lower.contains("denied") || lower.contains("policy") {
        "The host refused the outbound request (policy)."
    } else if cfg.via_loopback()
        && (lower.contains("dns") || lower.contains("address not available"))
    {
        "The loopback sentinel name did not resolve — that is what Desktop reports while the \
         Settings -> Security host-loopback door is off or the port grant is missing."
    } else if lower.contains("timed out") {
        "The upstream accepted the connection but did not answer before the outbound deadline \
         (MCP_OUTBOUND_TIMEOUT_MS)."
    } else {
        "Nothing answered there."
    };
    format!(
        "Could not reach the Playwright MCP server at {base}: {detail}. {cause}{grant} Start the \
         upstream on this machine with `{launch}` (comma-separated --allowed-hosts; \
         --host 127.0.0.1 because Desktop dials the sentinel over IPv4), then call \
         playwright_status. If you changed --port, PLAYWRIGHT_BASE_URL, allowedHosts, \
         allowedHostLoopbackPorts and --allowed-hosts must all change together.",
        base = cfg.base_url,
        launch = cfg.launch_command()
    )
}

/// What to do when the upstream refuses the Host header.
pub fn host_rejected_hint(cfg: &Config, message: &str) -> String {
    format!(
        "The Playwright MCP server refused the request by Host header (HTTP 403: {message}). \
         The sandbox cannot change the Host it sends ({authority}), so restart the upstream \
         with `{launch}` — the --allowed-hosts list is comma-separated (a space-separated list \
         keeps only the last value); PLAYWRIGHT_MCP_ALLOWED_HOSTS is the env equivalent and \
         '*' disables the check. Retrying without changing the upstream will fail the same way.",
        authority = cfg.authority(),
        launch = cfg.launch_command()
    )
}

// --- errors ------------------------------------------------------------------

/// Everything that can go wrong between this proxy and the upstream.
#[derive(Debug, Clone)]
pub enum Error {
    /// The deployment's configuration is unusable (bad base URL, token with
    /// characters that cannot travel in a header).
    Config(String),
    /// The host could not complete the exchange (DNS, policy, refused,
    /// timeout, reply too large).
    Transport { detail: String },
    /// HTTP 403 "Access is only allowed at …": the upstream's Host
    /// allow-list does not include the name the sandbox dials.
    HostRejected { message: String },
    /// HTTP 401/403 from an authenticating proxy in front of the upstream.
    Auth { status: u16, message: String },
    /// HTTP 429.
    RateLimited {
        message: String,
        retry_after: Option<String>,
    },
    /// Any other non-2xx answer.
    Http { status: u16, message: String },
    /// A JSON-RPC `error` object in the reply.
    Rpc { code: i64, message: String },
    /// A 2xx whose body is not a JSON-RPC reply to our request.
    Malformed { detail: String },
    /// The cached session is gone upstream (404 "Session not found" or 400
    /// "Server not initialized"). Handled internally by re-initializing
    /// once; surfaces only if that also fails.
    SessionLost { detail: String },
}

impl Error {
    /// Whether waiting and retrying could succeed without a human acting.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Transport { detail } => detail.to_ascii_lowercase().contains("timed out"),
            Error::RateLimited { .. } => true,
            Error::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// Short machine-readable kind for structured results.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Config(_) => "config",
            Error::Transport { .. } => "unreachable",
            Error::HostRejected { .. } => "host_rejected",
            Error::Auth { .. } => "auth",
            Error::RateLimited { .. } => "rate_limited",
            Error::Http { .. } => "http",
            Error::Rpc { .. } => "rpc",
            Error::Malformed { .. } => "malformed",
            Error::SessionLost { .. } => "session_lost",
        }
    }

    /// The caller-facing message: what happened and what to do about it.
    pub fn message(&self, cfg: &Config) -> String {
        match self {
            Error::Config(detail) => format!("playwright-mcp configuration error: {detail}"),
            Error::Transport { detail } => transport_hint(cfg, detail),
            Error::HostRejected { message } => host_rejected_hint(cfg, message),
            Error::Auth { status, message } => match &cfg.token {
                None => format!(
                    "{TOKEN_ENV} is not set and the Playwright MCP endpoint at {base} demands \
                     authentication (HTTP {status}: {message}). {hint}",
                    base = cfg.base_url,
                    hint = token_hint(cfg)
                ),
                Some(_) => format!(
                    "The Playwright MCP endpoint at {base} rejected the bearer token (HTTP \
                     {status}: {message}). Replace the `{SECRET_REF}` secret with a valid token; \
                     retrying with the same one will fail again. {hint}",
                    base = cfg.base_url,
                    hint = token_hint(cfg)
                ),
            },
            Error::RateLimited {
                message,
                retry_after,
            } => {
                let wait = retry_after
                    .as_deref()
                    .map(|v| format!(" Retry-After: {v}."))
                    .unwrap_or_default();
                format!(
                    "The Playwright MCP endpoint is rate limiting this proxy (HTTP 429: \
                     {message}).{wait} Wait before retrying; do not loop."
                )
            }
            Error::Http { status, message } => {
                let extra = match *status {
                    406 => {
                        " (the proxy's Accept header was refused — this is a proxy bug; \
                             the upstream needs `Accept: application/json, text/event-stream`)"
                    }
                    415 => " (the proxy's Content-Type was refused — this is a proxy bug)",
                    500..=599 => " — an upstream failure; retry once after a few seconds",
                    _ => "",
                };
                format!(
                    "The Playwright MCP server at {base} answered HTTP {status}: {message}{extra}",
                    base = cfg.base_url
                )
            }
            Error::Rpc { code, message } => {
                format!("The Playwright MCP server returned JSON-RPC error {code}: {message}")
            }
            Error::Malformed { detail } => format!(
                "The reply from {base} was not a Playwright MCP JSON-RPC response: {detail}. \
                 Is PLAYWRIGHT_BASE_URL the streamable-HTTP endpoint (it normally ends in /mcp)?",
                base = cfg.base_url
            ),
            Error::SessionLost { detail } => format!(
                "The upstream browser session was lost twice in a row ({detail}). The upstream \
                 may be restarting; call playwright_reset_session, then playwright_status."
            ),
        }
    }
}

// --- per-instance state ------------------------------------------------------

/// The upstream session cached on this instance.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
}

struct ToolCache {
    tools: Vec<Value>,
    fetched_at: Instant,
}

struct State {
    base_url: String,
    session: Option<Session>,
    tools: Option<ToolCache>,
    initialize_count: u64,
    next_id: u64,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            base_url: String::new(),
            session: None,
            tools: None,
            initialize_count: 0,
            next_id: 1,
        })
    })
}

/// Runs `f` with the state locked. The bridge serializes exchanges, so the
/// lock is uncontended; it is never held across an await. A poisoned lock
/// (impossible without panics, which abort here anyway) is recovered rather
/// than propagated.
fn with_state<T>(base_url: &str, f: impl FnOnce(&mut State) -> T) -> T {
    let mut guard = match state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.base_url != base_url {
        // Configuration changed under us (cannot happen on Desktop, where
        // env is fixed per workload; harmless otherwise): start clean.
        guard.base_url = base_url.to_owned();
        guard.session = None;
        guard.tools = None;
    }
    f(&mut guard)
}

/// A read-only view of the cache for `playwright_status` and `GET /`.
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub session: Option<Session>,
    pub tools_cached: Option<usize>,
    pub tools_age_secs: Option<u64>,
    pub initialize_count: u64,
}

pub fn snapshot(cfg: &Config) -> StateSnapshot {
    with_state(&cfg.base_url, |s| StateSnapshot {
        session: s.session.clone(),
        tools_cached: s.tools.as_ref().map(|c| c.tools.len()),
        tools_age_secs: s.tools.as_ref().map(|c| c.fetched_at.elapsed().as_secs()),
        initialize_count: s.initialize_count,
    })
}

/// Names of the upstream tools this proxy would list right now, from the
/// cache only (no I/O — used by the `GET /` discovery document, which runs
/// outside the request lock).
pub fn cached_tool_names(cfg: &Config) -> Vec<String> {
    let raw = with_state(&cfg.base_url, |s| {
        s.tools
            .as_ref()
            .map(|c| c.tools.clone())
            .unwrap_or_default()
    });
    visible_tools(&raw, cfg.allow_unsafe)
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

// --- the client --------------------------------------------------------------

/// A raw upstream reply.
pub struct Reply {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The most useful one-line description of a failure body: the JSON-RPC
    /// `error.message` if there is one, else a bounded excerpt of the text.
    fn message(&self) -> String {
        let text = self.text();
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            if let Some(msg) = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
            {
                return snippet(msg);
            }
            if let Some(msg) = value.get("message").and_then(Value::as_str) {
                return snippet(msg);
            }
        }
        let cleaned = strip_ansi(text.trim());
        if cleaned.is_empty() {
            format!("HTTP {}", self.status)
        } else {
            snippet(&cleaned)
        }
    }
}

fn snippet(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(SNIPPET_CHARS).collect();
    if flat.chars().count() > SNIPPET_CHARS {
        out.push('…');
    }
    out
}

pub struct Client {
    cfg: Config,
}

impl Client {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn next_id(&self) -> u64 {
        with_state(&self.cfg.base_url, |s| {
            let id = s.next_id;
            s.next_id = s.next_id.wrapping_add(1).max(1);
            id
        })
    }

    fn cached_session(&self) -> Option<Session> {
        with_state(&self.cfg.base_url, |s| s.session.clone())
    }

    fn forget_session(&self) {
        with_state(&self.cfg.base_url, |s| s.session = None);
    }

    fn builder(&self, method: http::Method) -> Result<http::request::Builder, Error> {
        if !(self.cfg.base_url.starts_with("http://") || self.cfg.base_url.starts_with("https://"))
        {
            return Err(Error::Config(format!(
                "{BASE_URL_ENV} must be an http(s) URL, got {:?}",
                self.cfg.base_url
            )));
        }
        let mut builder = http::Request::builder()
            .method(method)
            .uri(&self.cfg.base_url)
            .header("Accept", "application/json, text/event-stream")
            .header(
                "User-Agent",
                concat!("playwright-mcp-proxy/", env!("CARGO_PKG_VERSION")),
            );
        if let Some(token) = &self.cfg.token {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        Ok(builder)
    }

    /// POST one JSON-RPC message. `session`/`protocol` are the cached
    /// `Mcp-Session-Id` and the negotiated `MCP-Protocol-Version` (absent
    /// only for `initialize`).
    async fn post(
        &self,
        message: &Value,
        session: Option<&str>,
        protocol: Option<&str>,
    ) -> Result<Reply, Error> {
        let mut builder = self
            .builder(http::Method::POST)?
            .header("Content-Type", "application/json");
        if let Some(id) = session {
            builder = builder.header("Mcp-Session-Id", id);
        }
        if let Some(version) = protocol {
            builder = builder.header("MCP-Protocol-Version", version);
        }
        let body = serde_json::to_vec(message).map_err(|err| Error::Malformed {
            detail: format!("could not encode the request: {err}"),
        })?;
        let request = builder.body(Bytes::from(body)).map_err(|err| {
            Error::Config(format!(
                "could not build the upstream request (check {BASE_URL_ENV} and {TOKEN_ENV}): {err}"
            ))
        })?;
        self.send(request).await
    }

    async fn send(&self, request: http::Request<Bytes>) -> Result<Reply, Error> {
        let response = crate::bridge::outbound::fetch(request)
            .await
            .map_err(|err| Error::Transport {
                detail: err.to_string(),
            })?;
        let status = response.status().as_u16();
        let (parts, body) = response.into_parts();
        Ok(Reply {
            status,
            headers: parts.headers,
            body,
        })
    }

    /// Maps a non-2xx reply to the error catalogue.
    fn classify(reply: Reply) -> Result<Reply, Error> {
        let status = reply.status;
        if (200..300).contains(&status) {
            return Ok(reply);
        }
        let message = reply.message();
        Err(match status {
            401 => Error::Auth { status, message },
            403 if message.contains("Access is only allowed") => Error::HostRejected { message },
            403 => Error::Auth { status, message },
            404 if message.contains("Session not found") => Error::SessionLost {
                detail: format!("HTTP 404: {message}"),
            },
            400 if message.contains("not initialized") => Error::SessionLost {
                detail: format!("HTTP 400: {message}"),
            },
            429 => Error::RateLimited {
                message,
                retry_after: reply.header("retry-after"),
            },
            _ => Error::Http { status, message },
        })
    }

    /// Extracts the JSON-RPC `result` addressed to request `id` from a 2xx
    /// reply (SSE-framed or plain JSON).
    fn result_of(reply: &Reply, id: u64) -> Result<Value, Error> {
        let content_type = reply.header("content-type").unwrap_or_default();
        let messages = parse_messages(&content_type, &reply.text());
        if messages.is_empty() {
            return Err(Error::Malformed {
                detail: format!("HTTP {} with no JSON-RPC message in the body", reply.status),
            });
        }
        let matches_id = |m: &Value| match m.get("id") {
            Some(Value::Number(n)) => n.as_u64() == Some(id),
            Some(Value::String(s)) => s == &id.to_string(),
            _ => false,
        };
        if let Some(message) = messages.iter().find(|m| matches_id(m)) {
            if let Some(error) = message.get("error") {
                return Err(Error::Rpc {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(snippet)
                        .unwrap_or_else(|| "(no message)".to_owned()),
                });
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| Error::Malformed {
                    detail: "reply carries neither result nor error".to_owned(),
                });
        }
        // An error with a null id (the SDK's protocol-level rejections).
        if let Some(error) = messages.iter().find_map(|m| m.get("error")) {
            return Err(Error::Rpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(snippet)
                    .unwrap_or_else(|| "(no message)".to_owned()),
            });
        }
        Err(Error::Malformed {
            detail: format!("no reply with id {id} among {} message(s)", messages.len()),
        })
    }

    /// `initialize` + `notifications/initialized`; caches the session.
    async fn initialize(&self) -> Result<Session, Error> {
        let id = self.next_id();
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": UPSTREAM_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": env!("CARGO_PKG_NAME"),
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        });
        let reply = Self::classify(self.post(&message, None, None).await?)?;
        let session_id = reply
            .header("mcp-session-id")
            .ok_or_else(|| Error::Malformed {
                detail: "initialize succeeded but no Mcp-Session-Id header came back".to_owned(),
            })?;
        if session_id.is_empty() || session_id.len() > 512 || !session_id.is_ascii() {
            return Err(Error::Malformed {
                detail: "Mcp-Session-Id header is not a usable token".to_owned(),
            });
        }
        let result = Self::result_of(&reply, id)?;
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(UPSTREAM_PROTOCOL_VERSION)
            .to_owned();
        let server_info = result.get("serverInfo").cloned().unwrap_or(Value::Null);
        let session = Session {
            id: session_id,
            protocol_version,
            server_name: server_info
                .get("name")
                .and_then(Value::as_str)
                .map(snippet)
                .unwrap_or_default(),
            server_version: server_info
                .get("version")
                .and_then(Value::as_str)
                .map(snippet)
                .unwrap_or_default(),
        };

        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        match self
            .post(
                &initialized,
                Some(&session.id),
                Some(&session.protocol_version),
            )
            .await
        {
            Ok(reply) if (200..300).contains(&reply.status) => {}
            Ok(reply) => tracing::warn!(
                status = reply.status,
                "upstream did not accept notifications/initialized"
            ),
            Err(err) => tracing::warn!(error = ?err, "notifications/initialized failed"),
        }

        with_state(&self.cfg.base_url, |s| {
            s.session = Some(session.clone());
            s.initialize_count = s.initialize_count.saturating_add(1);
        });
        tracing::info!(
            server = %session.server_name,
            version = %session.server_version,
            protocol = %session.protocol_version,
            "initialized upstream Playwright MCP session"
        );
        Ok(session)
    }

    async fn session(&self) -> Result<Session, Error> {
        match self.cached_session() {
            Some(session) => Ok(session),
            None => self.initialize().await,
        }
    }

    /// One JSON-RPC request in the cached session. If the upstream reports
    /// the session gone, re-initializes once and retries (browser state is
    /// then fresh).
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value, Error> {
        let mut retried = false;
        loop {
            let session = self.session().await?;
            let id = self.next_id();
            let message = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params.clone(),
            });
            let reply = self
                .post(&message, Some(&session.id), Some(&session.protocol_version))
                .await?;
            match Self::classify(reply) {
                Ok(reply) => return Self::result_of(&reply, id),
                Err(Error::SessionLost { detail }) if !retried => {
                    tracing::warn!(%detail, "upstream session lost; re-initializing once");
                    self.forget_session();
                    retried = true;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `ping` in the cached session (initializing if needed).
    pub async fn ping(&self) -> Result<Session, Error> {
        self.rpc("ping", json!({})).await?;
        self.session().await
    }

    /// The upstream tool list, raw, cached for `tools_ttl_secs`.
    pub async fn tools(&self) -> Result<Vec<Value>, Error> {
        let ttl = Duration::from_secs(self.cfg.tools_ttl_secs);
        let cached = with_state(&self.cfg.base_url, |s| {
            s.tools
                .as_ref()
                .filter(|c| c.fetched_at.elapsed() < ttl)
                .map(|c| c.tools.clone())
        });
        match cached {
            Some(tools) => Ok(tools),
            None => self.refresh_tools().await,
        }
    }

    /// Re-fetches the upstream tool list unconditionally.
    pub async fn refresh_tools(&self) -> Result<Vec<Value>, Error> {
        let result = self.rpc("tools/list", json!({})).await?;
        let tools: Vec<Value> = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Malformed {
                detail: "tools/list result has no `tools` array".to_owned(),
            })?
            .iter()
            .filter(|t| t.get("name").and_then(Value::as_str).is_some())
            .take(MAX_UPSTREAM_TOOLS)
            .cloned()
            .collect();
        with_state(&self.cfg.base_url, |s| {
            s.tools = Some(ToolCache {
                tools: tools.clone(),
                fetched_at: Instant::now(),
            })
        });
        Ok(tools)
    }

    /// `tools/call`; returns the upstream result object.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, Error> {
        let result = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        if result.is_object() {
            Ok(result)
        } else {
            Err(Error::Malformed {
                detail: "tools/call result is not an object".to_owned(),
            })
        }
    }

    /// Closes the cached upstream session (DELETE) and forgets it along with
    /// the tool cache. Returns the DELETE status if a session existed.
    pub async fn reset_session(&self) -> Result<Option<u16>, Error> {
        let session = with_state(&self.cfg.base_url, |s| {
            s.tools = None;
            s.session.take()
        });
        let Some(session) = session else {
            return Ok(None);
        };
        let request = self
            .builder(http::Method::DELETE)?
            .header("Mcp-Session-Id", &session.id)
            .header("MCP-Protocol-Version", &session.protocol_version)
            .body(Bytes::new())
            .map_err(|err| Error::Config(format!("could not build the DELETE request: {err}")))?;
        let reply = self.send(request).await?;
        Ok(Some(reply.status))
    }
}

/// Splits a streamable-HTTP reply body into JSON-RPC messages: SSE events'
/// `data:` payloads when the body is `text/event-stream`, else the body as
/// one JSON value (or a batch array).
fn parse_messages(content_type: &str, body: &str) -> Vec<Value> {
    let mut messages = Vec::new();
    if content_type
        .to_ascii_lowercase()
        .starts_with("text/event-stream")
    {
        let mut data: Vec<&str> = Vec::new();
        let flush = |data: &mut Vec<&str>, messages: &mut Vec<Value>| {
            if data.is_empty() {
                return;
            }
            let joined = data.join("\n");
            data.clear();
            if let Ok(value) = serde_json::from_str::<Value>(joined.trim()) {
                messages.push(value);
            }
        };
        for line in body.lines() {
            if messages.len() >= MAX_SSE_EVENTS {
                break;
            }
            if line.is_empty() {
                flush(&mut data, &mut messages);
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // `event:`, `id:`, `retry:` and comment lines carry nothing we need.
        }
        flush(&mut data, &mut messages);
    } else if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
        match value {
            Value::Array(items) => messages.extend(items.into_iter().take(MAX_SSE_EVENTS)),
            other => messages.push(other),
        }
    }
    messages
}

// --- upstream knowledge applied on the passthrough ---------------------------

pub fn is_unsafe_tool(name: &str) -> bool {
    UNSAFE_TOOLS.contains(&name)
}

/// The refusal for a hidden tool.
pub fn gated_message(name: &str) -> String {
    format!(
        "{name} is disabled: set {ALLOW_UNSAFE_ENV}=true in the workload config to expose it. \
         It runs arbitrary JavaScript in the page or the Playwright process, or reads paths on \
         the developer's machine — outside this sandbox. Use browser_click / browser_type / \
         browser_fill_form / browser_snapshot instead."
    )
}

/// One upstream tool definition as this proxy lists it: `filename` removed
/// from the schema (and from `required`), plus a description note where the
/// proxy changes behaviour. Returns `None` for an entry without a name.
pub fn shape_tool(raw: &Value) -> Option<Value> {
    let name = raw.get("name").and_then(Value::as_str)?.to_owned();
    let mut tool = raw.clone();
    let mut stripped = false;
    if let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) {
        if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            for param in STRIPPED_PARAMS {
                stripped |= props.remove(*param).is_some();
            }
        }
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|v| v.as_str().is_none_or(|s| !STRIPPED_PARAMS.contains(&s)));
        }
    }
    let mut note = String::new();
    if stripped {
        note.push_str(
            " Results are always returned inline (no `filename`: files would land on \
                       the developer's disk, invisible to this sandbox).",
        );
    }
    if name == "browser_take_screenshot" {
        note.push_str(
            " Through this proxy `type` defaults to jpeg; prefer viewport (not \
                       fullPage) shots to stay under the reply-size cap.",
        );
    }
    if name == "browser_wait_for" {
        note.push_str(" `time` is clamped to 60 seconds here.");
    }
    if !note.is_empty() {
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        tool["description"] = Value::String(format!("{description}{note}"));
    }
    Some(tool)
}

/// The upstream tools this proxy exposes: gated ones removed unless allowed,
/// every schema shaped.
pub fn visible_tools(raw: &[Value], allow_unsafe: bool) -> Vec<Value> {
    raw.iter()
        .filter(|t| {
            allow_unsafe
                || t.get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|n| !is_unsafe_tool(n))
        })
        .filter_map(shape_tool)
        .collect()
}

fn set_number(args: &mut Map<String, Value>, key: &str, value: f64) {
    if let Some(n) = serde_json::Number::from_f64(value) {
        args.insert(key.to_owned(), Value::Number(n));
    }
}

fn clamp_number(
    args: &mut Map<String, Value>,
    key: &str,
    min: f64,
    max: f64,
    notes: &mut Vec<String>,
) {
    let Some(value) = args.get(key).and_then(Value::as_f64) else {
        return;
    };
    if !value.is_finite() {
        return;
    }
    let clamped = value.clamp(min, max);
    if clamped != value {
        set_number(args, key, clamped);
        notes.push(format!("`{key}` clamped from {value} to {clamped}"));
    }
}

/// Applies the proxy's argument policy before forwarding: strips
/// [`STRIPPED_PARAMS`], clamps the numeric ranges the upstream would otherwise
/// run away with, and fills the defaults the proxy changes. Returns the notes
/// to show the caller.
pub fn sanitize_arguments(name: &str, args: &mut Map<String, Value>) -> Vec<String> {
    let mut notes = Vec::new();
    for param in STRIPPED_PARAMS {
        if args.remove(*param).is_some() {
            notes.push(format!(
                "`{param}` was dropped: files would land on the developer's disk, invisible \
                 here; the result is returned inline instead"
            ));
        }
    }
    match name {
        "browser_wait_for" => clamp_number(args, "time", 0.0, MAX_WAIT_SECS, &mut notes),
        "browser_snapshot" => clamp_number(args, "depth", 1.0, MAX_SNAPSHOT_DEPTH, &mut notes),
        "browser_resize" => {
            clamp_number(args, "width", 1.0, MAX_VIEWPORT_PX, &mut notes);
            clamp_number(args, "height", 1.0, MAX_VIEWPORT_PX, &mut notes);
        }
        "browser_tabs" => clamp_number(args, "index", 0.0, 100_000.0, &mut notes),
        "browser_take_screenshot" => {
            if !args.contains_key("type") {
                args.insert("type".to_owned(), Value::String("jpeg".to_owned()));
            }
            if args.get("fullPage").and_then(Value::as_bool) == Some(true) {
                notes.push(
                    "fullPage screenshots can exceed the outbound reply cap \
                     (MCP_OUTBOUND_MAX_BYTES); if this call fails with ResponseTooLarge, \
                     take a viewport shot or raise the cap"
                        .to_owned(),
                );
            }
        }
        _ => {}
    }
    notes
}

/// Removes ANSI SGR/CSI escape sequences (Playwright call logs carry them).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // CSI: parameter/intermediate bytes, then a final byte 0x40..=0x7E.
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Largest index `<= limit` on a UTF-8 char boundary of `s`.
pub fn truncation_boundary(s: &str, limit: usize) -> usize {
    let mut index = limit.min(s.len());
    while !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Bounds a text block to [`MAX_TEXT_BYTES`], cutting on a char boundary.
pub fn bounded_text(mut text: String) -> String {
    if text.len() > MAX_TEXT_BYTES {
        let cut = truncation_boundary(&text, MAX_TEXT_BYTES);
        let dropped = text.len() - cut;
        text.truncate(cut);
        text.push_str(&format!(
            "\n…[truncated {dropped} bytes by playwright-mcp; use browser_find or \
             browser_snapshot with target/depth for a smaller view]"
        ));
    }
    text
}

/// Whether an upstream result text carries the file-only snapshot link.
pub fn has_snapshot_link(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with("- [Snapshot]("))
}

/// The fenced ```yaml block of a `browser_snapshot` result, if any.
fn yaml_block(text: &str) -> Option<&str> {
    let start = text.find("```yaml")?;
    let rest = &text[start + "```yaml".len()..];
    let end = rest.find("```")?;
    Some(&text[start..start + "```yaml".len() + end + "```".len()])
}

/// Replaces the `- [Snapshot](file)` line of an action result with the
/// inline YAML from a fresh `browser_snapshot`, keeping every other section
/// (`### Page`, `### Modal state`, `### Events`, …) intact.
pub fn inline_snapshot(action_text: &str, snapshot_text: &str) -> Option<String> {
    let link_line = action_text
        .lines()
        .find(|line| line.trim_start().starts_with("- [Snapshot]("))?;
    let inline = match yaml_block(snapshot_text) {
        Some(block) => block.to_owned(),
        None => {
            // No fence (e.g. "No open tabs"): keep whatever the snapshot said
            // after its own `### Snapshot` header, or the whole text.
            let body = snapshot_text
                .split_once("### Snapshot")
                .map(|(_, rest)| rest.trim())
                .filter(|rest| !rest.is_empty())
                .unwrap_or(snapshot_text.trim());
            body.to_owned()
        }
    };
    Some(action_text.replacen(link_line, &inline, 1))
}
