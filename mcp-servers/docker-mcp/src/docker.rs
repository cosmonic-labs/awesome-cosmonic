//! Docker Engine API client (podman-compatible).
//!
//! Everything that talks to the daemon lives here: configuration from the
//! environment, the versioned request shape, query encoding, the Docker /
//! podman error envelopes mapped to one error catalogue, the multiplexed
//! log-stream demultiplexer, the NDJSON pull-progress parser, the stats
//! arithmetic, and the output trimming/redaction the tools apply.
//! [`crate::server`] holds the tool definitions and result rendering.
//!
//! Endpoint semantics, status codes and the attach/logs frame format follow
//! the Docker Engine API v1.44 OpenAPI document (moby/moby `api/swagger.yaml`,
//! Apache-2.0). The tool surface takes ideas from ckreiling/mcp-server-docker
//! (GPL-3.0 — ideas only, no code) and QuantGeekDev/docker-mcp (MIT).

use std::collections::BTreeMap;

use base64::Engine as _;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

// --- configuration ----------------------------------------------------------

/// Base URL of the Docker Engine / podman REST endpoint.
pub const HOST_ENV: &str = "DOCKER_HOST";
/// The daemon's plain-HTTP TCP listener on the developer's machine, reached
/// through the Desktop loopback sentinel.
pub const DEFAULT_HOST: &str = "http://host.wasmcloud.internal:2375";
/// API version prefix put on every path (`/v1.44/...`).
pub const API_VERSION_ENV: &str = "DOCKER_API_VERSION";
pub const DEFAULT_API_VERSION: &str = "v1.44";
/// Anything but `false`/`0`/`no`/`off` keeps the write tools refused.
pub const READ_ONLY_ENV: &str = "DOCKER_READ_ONLY";
/// Optional registry credential for `pull_image` of private images.
pub const REGISTRY_AUTH_ENV: &str = "DOCKER_REGISTRY_AUTH";
/// Desktop secret reference that injects [`REGISTRY_AUTH_ENV`].
pub const REGISTRY_AUTH_REF: &str = "docker-mcp-registry-auth";
/// Loopback grant the default host needs.
pub const LOOPBACK_HOST: &str = "host.wasmcloud.internal";
pub const LOOPBACK_PORT: &str = "2375";
/// Where a Docker Hub read-only token comes from.
pub const HUB_TOKENS_URL: &str = "https://app.docker.com/settings/personal-access-tokens";
/// The daemon-side setup guide served as a skill file.
pub const SETUP_SKILL: &str = "skill://docker-mcp/references/SETUP.md";

// --- bounds -----------------------------------------------------------------

pub const MAX_ID_CHARS: usize = 128;
pub const MAX_IMAGE_REF_CHARS: usize = 256;
pub const MAX_LIST_LIMIT: i64 = 500;
pub const DEFAULT_LIST_LIMIT: i64 = 100;
pub const MAX_IMAGE_LIMIT: i64 = 1000;
pub const DEFAULT_IMAGE_LIMIT: i64 = 200;
pub const MAX_TAIL: i64 = 5000;
pub const DEFAULT_TAIL: i64 = 200;
pub const MIN_LOG_BYTES: i64 = 1024;
pub const MAX_LOG_BYTES: i64 = 1_048_576;
pub const DEFAULT_LOG_BYTES: i64 = 65_536;
pub const MAX_STOP_TIMEOUT: i64 = 300;
pub const DEFAULT_STOP_TIMEOUT: i64 = 10;
/// Characters of a `raw=true` document one call may return.
pub const RAW_CAP_CHARS: usize = 200 * 1024;
pub const MAX_ENV_ENTRIES: usize = 200;
pub const MAX_ENV_BYTES: usize = 8192;
pub const MAX_LABELS: usize = 100;
pub const MAX_LABEL_CHARS: usize = 512;
pub const MAX_ARGV: usize = 256;
pub const MAX_ARG_CHARS: usize = 4096;
pub const MAX_PORTS: usize = 64;
pub const MAX_FILTER_KEYS: usize = 16;
pub const MAX_FILTER_VALUES: usize = 64;
pub const MAX_FILTER_VALUE_CHARS: usize = 512;
pub const MAX_FILTER_BYTES: usize = 4096;
/// Docker refuses a memory limit under 6 MiB.
pub const MIN_MEMORY_BYTES: u64 = 6 * 1024 * 1024;
pub const MAX_PROGRESS_LINES: usize = 200;
pub const MAX_LAYERS: usize = 500;
pub const MAX_DF_ITEMS: usize = 200;
pub const MAX_ROWS: usize = 1000;
/// Lines of logs `run_container wait=true` appends to its result.
pub const WAIT_LOG_TAIL: i64 = 200;
/// Longest slice of an upstream body echoed into an error message.
const SNIPPET_CHARS: usize = 300;
/// Template outbound deadline default (mirrors `bridge::outbound`).
const DEFAULT_OUTBOUND_TIMEOUT_MS: u64 = 30_000;

/// Runtime configuration, read from the environment on every call.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub api_version: String,
    pub read_only: bool,
    pub registry_auth: Option<String>,
    pub outbound_timeout_ms: u64,
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

pub fn config() -> Config {
    let host = non_empty_env(HOST_ENV)
        .map(|v| v.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| DEFAULT_HOST.to_owned());
    let api_version =
        non_empty_env(API_VERSION_ENV).unwrap_or_else(|| DEFAULT_API_VERSION.to_owned());
    // Fail closed: only an explicit "false"-like value enables writes.
    let read_only = !non_empty_env(READ_ONLY_ENV)
        .map(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off"
            )
        })
        .unwrap_or(false);
    let outbound_timeout_ms = non_empty_env("MCP_OUTBOUND_TIMEOUT_MS")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_OUTBOUND_TIMEOUT_MS);
    Config {
        host,
        api_version,
        read_only,
        registry_auth: non_empty_env(REGISTRY_AUTH_ENV),
        outbound_timeout_ms,
    }
}

/// The `credentials` block of the `GET /` discovery document (presence only,
/// never values). The registry credential is optional: public images pull
/// without it, and the daemon itself has no authentication (the TCP
/// listener plus the Desktop loopback grants are the access control).
pub fn credentials() -> Value {
    let status = if non_empty_env(REGISTRY_AUTH_ENV).is_some() {
        "configured"
    } else {
        "missing"
    };
    json!([{
        "ref": REGISTRY_AUTH_REF,
        "env": REGISTRY_AUTH_ENV,
        "kind": "registry-auth-json",
        "required": false,
        "status": status,
        "validate": "version",
        "description": "Optional. Registry credentials for pull_image of private images: JSON {\"username\":\"..\",\"password\":\"<token>\",\"serveraddress\":\"<registry host>\"} (or {\"identitytoken\":\"..\"}), raw or already base64-encoded (any variant; it is re-sent as padded base64url). Sent only as the X-Registry-Auth header on POST /images/create; the `version` tool reports registry_auth_valid without dialling a registry. The Docker daemon itself needs no credential: DOCKER_HOST plus the Desktop loopback grants are the access control.",
        "obtainUrl": HUB_TOKENS_URL,
        "scopes": ["pull private images (read-only registry token)"],
    }])
}

/// The three-step loopback grant, quoted by every transport failure.
pub fn grant_hint() -> String {
    format!(
        "From the sandbox the daemon is reachable only as {DEFAULT_HOST} (never localhost/127.0.0.1, \
         which is the workload's own network) and only when (1) the daemon listens on TCP \
         127.0.0.1:{LOOPBACK_PORT} — `dockerd -H fd:// -H tcp://127.0.0.1:{LOOPBACK_PORT}`, \
         `podman system service --time 0 tcp://127.0.0.1:{LOOPBACK_PORT}`, or a \
         docker-socket-proxy (verify with `curl http://127.0.0.1:{LOOPBACK_PORT}/_ping`), \
         (2) deploy/workload.yaml lists allowedHosts [\"{LOOPBACK_HOST}:{LOOPBACK_PORT}\"] and \
         allowedHostLoopbackPorts [\"{LOOPBACK_PORT}\"], and (3) Cosmonic Desktop Settings -> \
         Security -> 'allow host loopback' is on. Retrying without changing one of those \
         returns the same result. Setup guide: {SETUP_SKILL}."
    )
}

/// The registry-credential instruction shared by the missing and invalid
/// credential errors.
pub fn registry_auth_hint() -> String {
    format!(
        "For a private image create a read-only registry token (Docker Hub: {HUB_TOKENS_URL}; \
         GHCR: a PAT with read:packages; ECR/GCR: their short-lived tokens) and register \
         {{\"username\":\"<user>\",\"password\":\"<token>\",\"serveraddress\":\"<registry host, \
         e.g. index.docker.io or ghcr.io>\"}} as the `{REGISTRY_AUTH_REF}` secret (env \
         {REGISTRY_AUTH_ENV}): paste it in Cosmonic Desktop -> Secrets, or run \
         cosmonic_set_secret name={REGISTRY_AUTH_REF} uri=keychain://cosmonic/{REGISTRY_AUTH_REF} \
         env={REGISTRY_AUTH_ENV} value='<json>', then add secretFrom: [{{name: \
         {REGISTRY_AUTH_REF}}}] under localResources.environment in deploy/workload.yaml and \
         re-apply. Public images never need it."
    )
}

/// The refusal every write tool returns under the read-only default.
pub fn read_only_refusal(tool: &str) -> String {
    format!(
        "docker-mcp is read-only ({READ_ONLY_ENV}=true, the default): {tool} was not sent to \
         the daemon. Set {READ_ONLY_ENV}: \"false\" under localResources.environment.config \
         in deploy/workload.yaml (and enable POST=1 plus ALLOW_START/ALLOW_STOP/ALLOW_RESTARTS \
         on a docker-socket-proxy if one fronts the daemon) and re-apply the workload to \
         enable {tool}. Read tools keep working."
    )
}

// --- errors -----------------------------------------------------------------

/// A failed exchange (or a refusal before any exchange), classified so the
/// tools render one actionable message and agents can tell retryable from
/// permanent.
#[derive(Debug)]
pub enum Error {
    /// The deployment's configuration is unusable (bad DOCKER_HOST scheme,
    /// malformed API version, unparsable registry credential).
    Config(String),
    /// A parameter was refused in-guest; nothing was sent.
    Invalid(String),
    /// The read-only gate; nothing was sent.
    Gated(String),
    /// HTTP 400 `client version X is too old/new`.
    VersionWindow { path: String, message: String },
    /// Any other non-2xx/304 answer, with the daemon's `message` when present.
    Api {
        path: String,
        status: u16,
        message: String,
        retry_after: Option<String>,
    },
    /// The host could not complete the exchange (DNS, policy, refused,
    /// timeout, oversized body).
    Transport { path: String, detail: String },
    /// A 2xx whose body is not what the endpoint documents.
    Malformed { path: String, detail: String },
}

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }

    /// The caller-facing message: what happened and what to do about it.
    pub fn message(&self) -> String {
        match self {
            Error::Config(detail) => format!("docker-mcp is misconfigured: {detail}"),
            Error::Invalid(detail) | Error::Gated(detail) => detail.clone(),
            Error::VersionWindow { path, message } => format!(
                "the daemon rejected the API version prefix on {path} (HTTP 400: {message}). \
                 {API_VERSION_ENV} (currently the prefix of that path) must satisfy \
                 MinAPIVersion <= value <= ApiVersion as reported by the `version` tool; \
                 Docker 29+ needs at least v1.44, older daemons reject versions above their \
                 ApiVersion. Set {API_VERSION_ENV} accordingly in deploy/workload.yaml and \
                 re-apply."
            ),
            Error::Api {
                path,
                status,
                message,
                retry_after,
            } => api_message(path, *status, message, retry_after.as_deref()),
            Error::Transport { path, detail } => {
                format!(
                    "could not reach the Docker daemon for {path}: {detail}. {}",
                    transport_hint(detail)
                )
            }
            Error::Malformed { path, detail } => format!(
                "the daemon answered {path} with something this server does not understand: \
                 {detail}. Check that {HOST_ENV} points at a Docker Engine / podman REST \
                 endpoint (GET /_ping must answer OK) and retry once."
            ),
        }
    }
}

/// What to do when the sandbox cannot reach the daemon at all.
pub fn transport_hint(detail: &str) -> String {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("timed out") {
        "The exchange outlived MCP_OUTBOUND_TIMEOUT_MS. For pull_image this means the pull was \
         cut short (Docker cancels a pull when the connection closes; podman may finish it in \
         the background) — pre-pull large images with the CLI, raise MCP_OUTBOUND_TIMEOUT_MS \
         in deploy/workload.yaml, or re-run (already-downloaded layers are reused). For \
         run_container wait=true the container is still running: use container_logs / \
         inspect_container."
            .to_owned()
    } else if lower.contains("size limit") || lower.contains("too large") {
        "The response body exceeded MCP_OUTBOUND_MAX_BYTES (4 MiB default). Avoid raw=true, \
         narrow the filters, or raise MCP_OUTBOUND_MAX_BYTES in deploy/workload.yaml."
            .to_owned()
    } else if lower.contains("dns") {
        // Observed on Cosmonic Desktop 0.5.27: with the Security toggle off
        // the sentinel name does not resolve at all, so the failure arrives
        // as `ErrorCode::DnsError(... "address not available")` rather than
        // a policy denial.
        format!(
            "A DNS failure for {LOOPBACK_HOST} ('address not available') is what a closed \
             loopback door looks like on Cosmonic Desktop: the sentinel name only resolves \
             once Settings -> Security -> 'allow host loopback' is on and the port is in \
             allowedHostLoopbackPorts; a DNS failure for any other name means {HOST_ENV} has a \
             typo. {}",
            grant_hint()
        )
    } else {
        // Policy denials and connection refused: all four legs of the
        // loopback grant are candidates, and none is retryable.
        grant_hint()
    }
}

fn api_message(path: &str, status: u16, message: &str, retry_after: Option<&str>) -> String {
    let head = format!("the daemon returned HTTP {status} for {path}: {message}");
    let lower = message.to_ascii_lowercase();
    let hint = match status {
        400 => {
            if lower.contains("x-registry-auth") || lower.contains("registry-auth") {
                format!(
                    "The daemon could not parse the X-Registry-Auth header (it expects padded \
                     base64url JSON, Go's base64.URLEncoding). docker-mcp always re-encodes the \
                     {REGISTRY_AUTH_ENV} secret that way, so check that the secret is the JSON \
                     object (or its base64) and not a placeholder. {}",
                    registry_auth_hint()
                )
            } else if lower.contains("at least one stream") {
                "Pass at least one of stdout/stderr=true.".to_owned()
            } else if lower.contains("filter") {
                "Filters must be an object of string arrays, e.g. {\"status\":[\"running\"]}, \
                 using only the keys listed in skill://docker-mcp/references/TOOLS.md."
                    .to_owned()
            } else {
                "The daemon rejected the request as malformed; check the parameters against \
                 skill://docker-mcp/references/TOOLS.md."
                    .to_owned()
            }
        }
        401 => format!(
            "The registry rejected the credential sent as X-Registry-Auth. {}",
            registry_auth_hint()
        ),
        403 => {
            if lower.contains("denied")
                || lower.contains("access")
                || path.contains("/images/create")
            {
                format!(
                    "Pull denied (podman answers 403 where Docker answers 404): the repository \
                     does not exist, is private, or the {REGISTRY_AUTH_ENV} credential is wrong. \
                     Check the reference (registry/namespace/name). {}",
                    registry_auth_hint()
                )
            } else {
                "A docker-socket-proxy (or similar) in front of the daemon denies this API \
                 section or all writes. Enable the section flag on the proxy (CONTAINERS=1, \
                 IMAGES=1, INFO=1, NETWORKS=1, VOLUMES=1, SYSTEM=1) and, for lifecycle tools, \
                 POST=1 plus ALLOW_START=1 ALLOW_STOP=1 ALLOW_RESTARTS=1. Read tools never \
                 need POST."
                    .to_owned()
            }
        }
        404 => {
            if path.contains("/images/create") {
                format!(
                    "Docker deliberately conflates 'repository does not exist' and 'private \
                     image without credentials'. Check the reference (registry/namespace/name, \
                     an explicit tag); for a private image register the credential. {}",
                    registry_auth_hint()
                )
            } else if path.contains("/containers/create") {
                "The image is not in the local store (create never pulls implicitly). Run \
                 pull_image with an explicit tag, or run_container with pull_if_missing=true, \
                 then retry."
                    .to_owned()
            } else if path.contains("/images/") {
                "No such image locally. list_images (all=true, filters {\"reference\":[..]}) \
                 shows what the daemon holds; pull_image fetches it."
                    .to_owned()
            } else if lower.contains("no such image") || lower.contains("image not known") {
                "The image is not in the local store. Run pull_image with an explicit tag, \
                 then retry."
                    .to_owned()
            } else if path.contains("/containers/") {
                "No such container: wrong id/name, or it was auto-removed / already deleted. \
                 list_containers with all=true (and filters {\"name\":[..]}) shows the 12-char \
                 ids to use."
                    .to_owned()
            } else {
                "The daemon has no such route: check DOCKER_HOST points at the Engine API root \
                 (not a sub-path) and that the API version is supported."
                    .to_owned()
            }
        }
        409 => {
            if lower.contains("cannot be forced")
                || lower.contains("running container") && path.contains("/images/")
            {
                "A running container uses this image; force cannot override. stop_container / \
                 remove_container that container first, then remove_image."
                    .to_owned()
            } else if path.contains("/images/") {
                "A stopped container or another tag still references the image: remove_image \
                 force=true untags/removes it, or remove the stopped container first."
                    .to_owned()
            } else if lower.contains("not running") || path.ends_with("/kill") {
                "The container is not running: signals only reach running containers. \
                 inspect_container for State.Status; use start_container or remove_container \
                 instead."
                    .to_owned()
            } else if lower.contains("remove") || lower.contains("running") {
                "The container is running (or paused) and force=false: stop_container first, \
                 or remove_container force=true if losing in-flight work is acceptable."
                    .to_owned()
            } else {
                "Conflict: the container/image is in a state that refuses this operation \
                 (a name already in use, a running container). inspect_container and pick \
                 another name or stop it first."
                    .to_owned()
            }
        }
        429 => format!(
            "Rate limited by a proxy in front of the daemon{}. Wait before retrying, never in \
             a tight loop.",
            retry_after
                .map(|s| format!(" (Retry-After: {s})"))
                .unwrap_or_default()
        ),
        500..=599 => {
            if path.contains("/images/create")
                && (lower.contains("auth token")
                    || lower.contains("unauthorized")
                    || lower.contains("username")
                    || lower.contains("password")
                    || lower.contains("authentication required"))
            {
                // podman: 500 "unable to retrieve auth token: invalid
                // username/password"; Docker: 500 "unauthorized: incorrect
                // username or password". The header was parsed; the
                // registry said no.
                format!(
                    "The registry rejected the credential the daemon forwarded from \
                     X-Registry-Auth (it is sent on every pull once configured, public \
                     images included). {}",
                    registry_auth_hint()
                )
            } else if lower.contains("container state improper") || lower.contains("without force")
            {
                "podman reports a running/paused container with HTTP 500 where Docker uses 409: \
                 stop_container first, or remove_container force=true."
                    .to_owned()
            } else if lower.contains("container is stopped")
                || lower.contains("not running")
                || lower.contains("can only kill running")
            {
                "The container is not running (podman answers 500 where Docker returns zeros / \
                 409). Only call container_stats / kill_container on running containers — \
                 list_containers shows the state."
                    .to_owned()
            } else if lower.contains("logging driver") {
                "The container uses a log driver other than json-file/journald, so its logs \
                 cannot be read through the API; recreate it with the default driver or read \
                 the logs at the driver's sink."
                    .to_owned()
            } else if lower.contains("makechan") {
                "A negative/non-numeric tail reached the daemon. This tool clamps tail to \
                 1..=5000, so DOCKER_HOST is a proxy rewriting queries — check it."
                    .to_owned()
            } else if lower.contains("filter") {
                "The filters were not a JSON object of string arrays or used an unknown key; \
                 use {\"status\":[\"running\"]} shapes."
                    .to_owned()
            } else if lower.contains("manifest unknown") || lower.contains("not found") {
                "The pull failed: the tag/digest does not exist or is not available for the \
                 requested platform. Check the tag on the registry."
                    .to_owned()
            } else {
                "Daemon-side failure. Retry once after a few seconds; if it persists check \
                 the daemon logs (journalctl -u docker / podman system service output)."
                    .to_owned()
            }
        }
        _ => String::new(),
    };
    if hint.is_empty() {
        head
    } else {
        format!("{head}. {hint}")
    }
}

// --- HTTP client -------------------------------------------------------------

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

    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The `message` of a Docker/podman error envelope, or a bounded text
/// snippet of a non-JSON body.
fn envelope_message(reply: &Reply) -> String {
    if let Some(value) = reply.json() {
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            let mut text = message.trim().to_owned();
            if let Some(cause) = value.get("cause").and_then(Value::as_str) {
                if !cause.is_empty() && !text.contains(cause) {
                    text = format!("{text} (cause: {cause})");
                }
            }
            return bounded(&text, SNIPPET_CHARS);
        }
        if let Some(err) = value.get("error").and_then(Value::as_str) {
            return bounded(err.trim(), SNIPPET_CHARS);
        }
    }
    let text = String::from_utf8_lossy(&reply.body);
    let text = text.trim();
    if text.is_empty() {
        format!("HTTP {} with an empty body", reply.status)
    } else {
        bounded(&strip_tags(text), SNIPPET_CHARS)
    }
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decodes base64 in any of the four common variants (url-safe or standard
/// alphabet, padded or not). The header itself is always re-encoded padded
/// url-safe, so the registered form does not matter.
fn decode_base64_lenient(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    if s.is_empty() || s.len() > 64 * 1024 {
        return None;
    }
    let unpadded = s.trim_end_matches('=');
    URL_SAFE
        .decode(s)
        .ok()
        .or_else(|| STANDARD.decode(s).ok())
        .or_else(|| URL_SAFE_NO_PAD.decode(unpadded).ok())
        .or_else(|| STANDARD_NO_PAD.decode(unpadded).ok())
}

/// The HTTP client: base URL plus the API version prefix and the optional
/// registry credential.
#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    version: String,
    registry_auth: Option<String>,
}

impl Client {
    pub fn new(cfg: &Config) -> Result<Self, Error> {
        let base = cfg.host.as_str();
        if let Some(scheme) = base.split("://").next().filter(|_| base.contains("://")) {
            let lower = scheme.to_ascii_lowercase();
            if lower != "http" && lower != "https" {
                return Err(Error::Config(format!(
                    "{HOST_ENV} uses the {lower}:// scheme, which a sandboxed component cannot \
                     open (no unix sockets, no ssh, no raw TCP). Use http://{LOOPBACK_HOST}:{LOOPBACK_PORT} \
                     with a daemon TCP listener (see {SETUP_SKILL}); TLS daemons with client \
                     certificates are unsupported."
                )));
            }
        } else {
            return Err(Error::Config(format!(
                "{HOST_ENV} must be a URL with a scheme (http:// or https://), got {base:?}"
            )));
        }
        if base.len() > 512 || base.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::Config(format!("{HOST_ENV} is not a valid URL")));
        }
        let version = normalize_api_version(&cfg.api_version)?;
        Ok(Self {
            base: base.to_owned(),
            version,
            registry_auth: cfg.registry_auth.clone(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The prefix used on every path, e.g. `v1.44`.
    pub fn api_version(&self) -> &str {
        &self.version
    }

    pub fn has_registry_auth(&self) -> bool {
        self.registry_auth.is_some()
    }

    /// `X-Registry-Auth` value, or `None` when no credential is configured.
    ///
    /// The daemon decodes the header with Go's `base64.URLEncoding` (RFC 4648
    /// section 5 **with** padding): an unpadded value fails to parse and the
    /// whole pull is answered with HTTP 400 "unexpected EOF", so the value is
    /// always re-emitted as padded base64url regardless of how the secret was
    /// registered (raw JSON, or standard/url-safe base64 with or without
    /// padding).
    pub fn registry_auth_header(&self) -> Result<Option<String>, Error> {
        let Some(raw) = self.registry_auth.as_deref() else {
            return Ok(None);
        };
        let trimmed = raw.trim();
        if trimmed.starts_with('{') {
            let parsed: Value = serde_json::from_str(trimmed).map_err(|err| {
                Error::Config(format!(
                    "{REGISTRY_AUTH_ENV} starts with '{{' but is not valid JSON ({err}). {}",
                    registry_auth_hint()
                ))
            })?;
            if !parsed.is_object() {
                return Err(Error::Config(format!(
                    "{REGISTRY_AUTH_ENV} must be a JSON object. {}",
                    registry_auth_hint()
                )));
            }
            let compact = serde_json::to_string(&parsed).unwrap_or_default();
            return Ok(Some(
                base64::engine::general_purpose::URL_SAFE.encode(compact.as_bytes()),
            ));
        }
        let decoded = decode_base64_lenient(trimmed);
        let is_json_object = decoded
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
            .is_some_and(|v| v.is_object());
        match decoded {
            Some(bytes) if is_json_object => Ok(Some(
                base64::engine::general_purpose::URL_SAFE.encode(&bytes),
            )),
            _ => Err(Error::Config(format!(
                "{REGISTRY_AUTH_ENV} is set but is neither a JSON object nor base64url-encoded \
                 JSON (a placeholder value?). Register a real credential as the \
                 `{REGISTRY_AUTH_REF}` secret or remove it. {}",
                registry_auth_hint()
            ))),
        }
    }

    /// One exchange against `/{version}{path}`. `path` starts with `/` and
    /// carries its own (already encoded) query string.
    pub async fn send(
        &self,
        method: http::Method,
        path: &str,
        headers: &[(&str, String)],
        body: Bytes,
    ) -> Result<Reply, Error> {
        let versioned = format!("/{}{}", self.version, path);
        self.send_raw(method, &versioned, headers, body).await
    }

    /// One exchange against an unversioned path (`/version`, `/_ping`).
    pub async fn send_raw(
        &self,
        method: http::Method,
        path: &str,
        headers: &[(&str, String)],
        body: Bytes,
    ) -> Result<Reply, Error> {
        let url = format!("{}{}", self.base, path);
        let mut builder = http::Request::builder()
            .method(method)
            .uri(&url)
            .header(
                "User-Agent",
                concat!("docker-mcp/", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", "application/json");
        if !body.is_empty() {
            builder = builder.header("Content-Type", "application/json");
        }
        for (name, value) in headers {
            builder = builder.header(*name, value.as_str());
        }
        let request = builder.body(body).map_err(|err| {
            Error::Invalid(format!("could not build the request for {path}: {err}"))
        })?;
        // Method + path only (never headers or bodies): the JSON log layer
        // serialises the enclosing tool span with every event.
        tracing::debug!(method = %request.method(), path = %path, "dialling the daemon");
        let response = crate::bridge::outbound::fetch(request)
            .await
            .map_err(|err| Error::Transport {
                path: path.to_owned(),
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

    /// Turns a non-2xx (and non-304) reply into an error.
    pub fn ok(reply: Reply, path: &str) -> Result<Reply, Error> {
        if reply.is_success() || reply.status == 304 {
            return Ok(reply);
        }
        let message = envelope_message(&reply);
        let lower = message.to_ascii_lowercase();
        if reply.status == 400
            && lower.contains("client version")
            && (lower.contains("too old") || lower.contains("too new"))
        {
            return Err(Error::VersionWindow {
                path: path.to_owned(),
                message,
            });
        }
        Err(Error::Api {
            path: path.to_owned(),
            status: reply.status,
            message,
            retry_after: reply.header("retry-after"),
        })
    }

    /// GET a versioned path and parse the JSON body.
    pub async fn get_json(&self, path: &str) -> Result<Value, Error> {
        let reply = self
            .send(http::Method::GET, path, &[], Bytes::new())
            .await?;
        let reply = Self::ok(reply, path)?;
        reply.json().ok_or_else(|| Error::Malformed {
            path: path.to_owned(),
            detail: format!(
                "expected a JSON body, got {} bytes of {}",
                reply.body.len(),
                reply.header("content-type").unwrap_or_default()
            ),
        })
    }

    /// POST with an optional JSON body; returns the reply for status
    /// inspection (204/304 are meaningful for lifecycle calls).
    pub async fn post(&self, path: &str, body: Option<&Value>) -> Result<Reply, Error> {
        let bytes = match body {
            Some(value) => Bytes::from(serde_json::to_vec(value).unwrap_or_default()),
            None => Bytes::new(),
        };
        let reply = self.send(http::Method::POST, path, &[], bytes).await?;
        Self::ok(reply, path)
    }

    pub async fn delete(&self, path: &str) -> Result<Reply, Error> {
        let reply = self
            .send(http::Method::DELETE, path, &[], Bytes::new())
            .await?;
        Self::ok(reply, path)
    }
}

/// `v1.44` / `1.44` -> `v1.44`; anything else is a configuration error.
fn normalize_api_version(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let valid = digits
        .strip_prefix("1.")
        .is_some_and(|minor| minor.len() == 2 && minor.bytes().all(|b| b.is_ascii_digit()));
    if !valid {
        return Err(Error::Config(format!(
            "{API_VERSION_ENV} must look like v1.44 (v1.NN), got {trimmed:?}"
        )));
    }
    Ok(format!("v{digits}"))
}

// --- query strings & identifiers ------------------------------------------

/// Everything but unreserved characters is percent-encoded in query values
/// (RFC 3986), so a JSON `filters` value travels as `%7B%22...%7D`.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub fn encode_query_value(value: &str) -> String {
    utf8_percent_encode(value, QUERY_VALUE).to_string()
}

/// An ordered query-string builder that encodes every value.
#[derive(Debug, Default)]
pub struct Query(Vec<(String, String)>);

impl Query {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, key: &str, value: impl ToString) -> &mut Self {
        self.0.push((key.to_owned(), value.to_string()));
        self
    }

    pub fn push_opt(&mut self, key: &str, value: Option<&str>) -> &mut Self {
        if let Some(value) = value {
            self.0.push((key.to_owned(), value.to_owned()));
        }
        self
    }

    /// The pushed pairs, in order (unencoded).
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// `?k=v&...`, or an empty string when nothing was pushed.
    pub fn render(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|(k, v)| format!("{}={}", encode_query_value(k), encode_query_value(v)))
            .collect();
        format!("?{}", parts.join("&"))
    }
}

/// A container id or name: `^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$`. Safe to
/// splice into a path without encoding.
pub fn container_ref(id: &str, what: &str) -> Result<String, Error> {
    let id = id.trim();
    let mut bytes = id.bytes();
    let first_ok = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric());
    let rest_ok = bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if !first_ok || !rest_ok || id.len() > MAX_ID_CHARS {
        return Err(Error::Invalid(format!(
            "{what} must match ^[A-Za-z0-9][A-Za-z0-9_.-]{{0,{}}}$ (a container name or id; \
             12-char ids from list_containers work), got {:?}",
            MAX_ID_CHARS - 1,
            bounded(id, 80)
        )));
    }
    Ok(id.to_owned())
}

/// An image reference (`name[:tag|@digest]` or an id):
/// `^[A-Za-z0-9][A-Za-z0-9._:/@+-]{0,255}$` with no empty, `.` or `..` path
/// segments. Every allowed byte is a valid path character, so the value is
/// spliced into `/images/{name}/...` verbatim (slashes preserved, as the
/// Engine API expects).
pub fn image_ref(name: &str) -> Result<String, Error> {
    let name = name.trim();
    let mut bytes = name.bytes();
    let first_ok = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric());
    let rest_ok = bytes.all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'+' | b'-')
    });
    let segments_ok = name
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if !first_ok || !rest_ok || !segments_ok || name.len() > MAX_IMAGE_REF_CHARS {
        return Err(Error::Invalid(format!(
            "image must be a reference like alpine:3.20, ghcr.io/org/app@sha256:..., or an \
             image id (^[A-Za-z0-9][A-Za-z0-9._:/@+-]{{0,{}}}$, no empty or '..' segments), \
             got {:?}",
            MAX_IMAGE_REF_CHARS - 1,
            bounded(name, 80)
        )));
    }
    Ok(name.to_owned())
}

/// A signal name (`SIGTERM`, `TERM`) or number (`9`).
pub fn signal(value: &str) -> Result<String, Error> {
    let value = value.trim().to_ascii_uppercase();
    let numeric =
        !value.is_empty() && value.len() <= 3 && value.bytes().all(|b| b.is_ascii_digit());
    let named = {
        let body = value.strip_prefix("SIG").unwrap_or(&value);
        !body.is_empty()
            && body.len() <= 12
            && body
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    };
    if !numeric && !named {
        return Err(Error::Invalid(format!(
            "signal must be a name like SIGTERM/TERM or a number 0-999, got {:?}",
            bounded(&value, 40)
        )));
    }
    Ok(value)
}

/// `os[/arch[/variant]]`, lowercase.
pub fn platform(value: &str) -> Result<String, Error> {
    let value = value.trim();
    let ok = !value.is_empty()
        && value.len() <= 64
        && value.split('/').count() <= 3
        && value.split('/').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if !ok {
        return Err(Error::Invalid(format!(
            "platform must be os[/arch[/variant]] such as linux/amd64 or linux/arm64/v8, got {:?}",
            bounded(value, 40)
        )));
    }
    Ok(value.to_owned())
}

/// Validates and JSON-encodes a `filters` object for the list endpoints:
/// `{"status":["running"],"label":["app=web"]}`. A bare string value is
/// wrapped into a one-element array. Keys are checked against the endpoint's
/// documented list before anything is sent.
pub fn filters_json(
    filters: Option<&BTreeMap<String, Value>>,
    allowed: &[&str],
    endpoint: &str,
) -> Result<Option<String>, Error> {
    let Some(filters) = filters else {
        return Ok(None);
    };
    if filters.is_empty() {
        return Ok(None);
    }
    if filters.len() > MAX_FILTER_KEYS {
        return Err(Error::Invalid(format!(
            "filters may carry at most {MAX_FILTER_KEYS} keys"
        )));
    }
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, value) in filters {
        if !allowed.contains(&key.as_str()) {
            return Err(Error::Invalid(format!(
                "unknown filter key {:?} for {endpoint}; allowed: {}",
                bounded(key, 60),
                allowed.join(", ")
            )));
        }
        let values: Vec<String> = match value {
            Value::String(s) => vec![s.clone()],
            Value::Bool(b) => vec![b.to_string()],
            Value::Number(n) => vec![n.to_string()],
            Value::Array(items) => {
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::String(s) => list.push(s.clone()),
                        Value::Bool(b) => list.push(b.to_string()),
                        Value::Number(n) => list.push(n.to_string()),
                        _ => {
                            return Err(Error::Invalid(format!(
                                "filter {key:?} values must be strings (got {})",
                                kind_of(item)
                            )))
                        }
                    }
                }
                list
            }
            other => {
                return Err(Error::Invalid(format!(
                    "filter {key:?} must be a string or an array of strings (got {})",
                    kind_of(other)
                )))
            }
        };
        if values.is_empty() {
            return Err(Error::Invalid(format!("filter {key:?} has no values")));
        }
        if values.len() > MAX_FILTER_VALUES {
            return Err(Error::Invalid(format!(
                "filter {key:?} may carry at most {MAX_FILTER_VALUES} values"
            )));
        }
        for v in &values {
            if v.chars().count() > MAX_FILTER_VALUE_CHARS || v.chars().any(char::is_control) {
                return Err(Error::Invalid(format!(
                    "filter {key:?} value is longer than {MAX_FILTER_VALUE_CHARS} characters or \
                     contains control characters"
                )));
            }
        }
        out.insert(key.clone(), values);
    }
    let encoded = serde_json::to_string(&out).unwrap_or_default();
    if encoded.len() > MAX_FILTER_BYTES {
        return Err(Error::Invalid(format!(
            "filters serialize to {} bytes; the limit is {MAX_FILTER_BYTES}",
            encoded.len()
        )));
    }
    Ok(Some(encoded))
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

/// `repo[:tag|@digest]` split for `POST /images/create?fromImage=&tag=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub repo: String,
    /// The tag or the `sha256:...` digest.
    pub tag: String,
    /// `latest` was substituted because the reference carried neither.
    pub defaulted: bool,
}

pub fn split_image_ref(reference: &str) -> Result<ImageRef, Error> {
    let name = image_ref(reference)?;
    if let Some((repo, digest)) = name.split_once('@') {
        if repo.is_empty() || !digest.contains(':') {
            return Err(Error::Invalid(format!(
                "digest references look like repo@sha256:<hex>, got {:?}",
                bounded(&name, 80)
            )));
        }
        return Ok(ImageRef {
            repo: repo.to_owned(),
            tag: digest.to_owned(),
            defaulted: false,
        });
    }
    let last_slash = name.rfind('/').map(|i| i + 1).unwrap_or(0);
    let tail = &name[last_slash..];
    if let Some(colon) = tail.rfind(':') {
        let repo = &name[..last_slash + colon];
        let tag = &tail[colon + 1..];
        if repo.is_empty() || tag.is_empty() {
            return Err(Error::Invalid(format!(
                "image reference has an empty repository or tag: {:?}",
                bounded(&name, 80)
            )));
        }
        return Ok(ImageRef {
            repo: repo.to_owned(),
            tag: tag.to_owned(),
            defaulted: false,
        });
    }
    Ok(ImageRef {
        repo: name,
        tag: "latest".to_owned(),
        defaulted: true,
    })
}

// --- time ---------------------------------------------------------------------

/// A point in time for `since`/`until`: unix seconds, or an RFC 3339 string
/// such as `2026-09-01T12:00:00Z` (converted to seconds before sending).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TimeSpec {
    Seconds(i64),
    Text(String),
}

pub fn unix_seconds(spec: &TimeSpec, what: &str) -> Result<i64, Error> {
    match spec {
        TimeSpec::Seconds(s) => {
            if *s < 0 {
                return Err(Error::Invalid(format!("{what} must not be negative")));
            }
            Ok(*s)
        }
        TimeSpec::Text(text) => {
            let text = text.trim();
            if let Ok(s) = text.parse::<i64>() {
                if s < 0 {
                    return Err(Error::Invalid(format!("{what} must not be negative")));
                }
                return Ok(s);
            }
            parse_rfc3339(text).ok_or_else(|| {
                Error::Invalid(format!(
                    "{what} must be unix seconds or an RFC 3339 timestamp like \
                     2026-09-01T12:00:00Z, got {:?}",
                    bounded(text, 40)
                ))
            })
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM]` -> unix seconds (fraction dropped).
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let slice = text.get(from..to)?;
        if !slice.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        slice.parse::<i64>().ok()
    };
    if bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &after_dot[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return None,
            };
            let body = &rest[1..];
            if body.len() != 5 || body.as_bytes()[2] != b':' {
                return None;
            }
            let oh = body[..2].parse::<i64>().ok()?;
            let om = body[3..].parse::<i64>().ok()?;
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Howard Hinnant's days-from-civil (proleptic Gregorian, 1970 epoch).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// --- run_container inputs -----------------------------------------------------

/// One `[hostip:]hostport:containerport[/proto]` publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub host_ip: String,
    /// Empty string = let the daemon pick (spec `0`).
    pub host_port: String,
    pub container_port: u16,
    pub proto: String,
}

impl PortSpec {
    pub fn key(&self) -> String {
        format!("{}/{}", self.container_port, self.proto)
    }
}

pub fn parse_port(spec: &str) -> Result<PortSpec, Error> {
    let spec = spec.trim();
    let bad = |why: &str| {
        Error::Invalid(format!(
            "port {:?}: {why}; expected [hostip:]hostport:containerport[/proto] such as \
             8080:80, 127.0.0.1:8080:80/tcp or 0:80 (daemon picks the host port)",
            bounded(spec, 60)
        ))
    };
    let (body, proto) = match spec.rsplit_once('/') {
        Some((body, proto)) => (body, proto.to_ascii_lowercase()),
        None => (spec, "tcp".to_owned()),
    };
    if !matches!(proto.as_str(), "tcp" | "udp" | "sctp") {
        return Err(bad("protocol must be tcp, udp or sctp"));
    }
    let (host_ip, rest) = if let Some(after) = body.strip_prefix('[') {
        let end = after
            .find(']')
            .ok_or_else(|| bad("unterminated IPv6 bracket"))?;
        let ip = &after[..end];
        let rest = after[end + 1..]
            .strip_prefix(':')
            .ok_or_else(|| bad("expected ':' after the IPv6 address"))?;
        (ip.to_owned(), rest)
    } else {
        match body.matches(':').count() {
            1 => ("127.0.0.1".to_owned(), body),
            2 => {
                let (ip, rest) = body.split_once(':').unwrap_or((body, ""));
                (ip.to_owned(), rest)
            }
            _ => return Err(bad("wrong number of ':' separators")),
        }
    };
    if host_ip.is_empty() || host_ip.len() > 45 || host_ip.parse::<std::net::IpAddr>().is_err() {
        return Err(bad("host ip is not an IP address"));
    }
    let (host_port, container_port) = rest.split_once(':').ok_or_else(|| bad("missing ':'"))?;
    let host_port_num = host_port
        .parse::<u16>()
        .map_err(|_| bad("host port must be 0-65535"))?;
    let container_port = container_port
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| bad("container port must be 1-65535"))?;
    Ok(PortSpec {
        host_ip,
        host_port: if host_port_num == 0 {
            String::new()
        } else {
            host_port_num.to_string()
        },
        container_port,
        proto,
    })
}

/// Validates `KEY=VALUE` entries: POSIX names, no control characters, bounded
/// count and total size.
pub fn env_entries(entries: Vec<String>) -> Result<Vec<String>, Error> {
    if entries.len() > MAX_ENV_ENTRIES {
        return Err(Error::Invalid(format!(
            "env may carry at most {MAX_ENV_ENTRIES} entries (got {})",
            entries.len()
        )));
    }
    let total: usize = entries.iter().map(String::len).sum();
    if total > MAX_ENV_BYTES {
        return Err(Error::Invalid(format!(
            "env entries total {total} bytes; the limit is {MAX_ENV_BYTES}"
        )));
    }
    for entry in &entries {
        let (key, _) = entry.split_once('=').ok_or_else(|| {
            Error::Invalid(format!(
                "env entry {:?} is not KEY=VALUE",
                bounded(entry, 60)
            ))
        })?;
        let name_ok = key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !name_ok {
            return Err(Error::Invalid(format!(
                "env key {:?} must match ^[A-Za-z_][A-Za-z0-9_]*$",
                bounded(key, 60)
            )));
        }
        if entry.chars().any(|c| c == '\0' || c == '\n' || c == '\r') {
            return Err(Error::Invalid(format!(
                "env entry for {key} contains a control character"
            )));
        }
    }
    Ok(entries)
}

/// Bounded string list (cmd/entrypoint).
pub fn argv(list: Vec<String>, what: &str) -> Result<Vec<String>, Error> {
    if list.len() > MAX_ARGV {
        return Err(Error::Invalid(format!(
            "{what} may carry at most {MAX_ARGV} elements"
        )));
    }
    for arg in &list {
        if arg.len() > MAX_ARG_CHARS || arg.contains('\0') {
            return Err(Error::Invalid(format!(
                "{what} element is longer than {MAX_ARG_CHARS} bytes or contains NUL"
            )));
        }
    }
    Ok(list)
}

pub fn labels(map: BTreeMap<String, String>) -> Result<BTreeMap<String, String>, Error> {
    if map.len() > MAX_LABELS {
        return Err(Error::Invalid(format!(
            "labels may carry at most {MAX_LABELS} entries"
        )));
    }
    for (k, v) in &map {
        if k.is_empty()
            || k.len() > MAX_LABEL_CHARS
            || v.len() > MAX_LABEL_CHARS
            || k.chars().any(char::is_control)
            || v.chars().any(char::is_control)
        {
            return Err(Error::Invalid(format!(
                "label {:?}: keys and values are 1..={MAX_LABEL_CHARS} characters without \
                 control characters",
                bounded(k, 60)
            )));
        }
    }
    Ok(map)
}

// --- log demultiplexing ---------------------------------------------------------

/// Demultiplexed container logs.
#[derive(Debug, Default)]
pub struct LogOutput {
    pub tty: bool,
    pub stdout: String,
    pub stderr: String,
    pub combined: String,
    pub truncated: bool,
    /// Bytes of payload returned (after truncation).
    pub bytes: usize,
    /// Bytes of payload the daemon sent.
    pub total_bytes: usize,
    pub frames: usize,
    /// The body ended inside a frame (header announced more than arrived).
    pub partial_frame: bool,
}

/// Non-TTY bodies are a stream of `[type, 0, 0, 0, len(u32 BE)][payload]`
/// frames (type 0 stdin, 1 stdout, 2 stderr); TTY bodies are raw bytes with
/// CRLF line ends. The result keeps the *tail* when the payload exceeds
/// `max_bytes` (the newest lines are the useful ones), cutting the oldest
/// surviving frame on a character boundary.
pub fn demux_logs(body: &[u8], tty: bool, max_bytes: usize) -> LogOutput {
    let mut frames: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut partial_frame = false;
    let looks_framed =
        body.len() >= 8 && matches!(body[0], 0..=2) && body[1] == 0 && body[2] == 0 && body[3] == 0;
    if tty || !looks_framed {
        if !body.is_empty() {
            frames.push((1, body.to_vec()));
        }
    } else {
        let mut at = 0usize;
        while at + 8 <= body.len() {
            let stream = body[at];
            let len = u32::from_be_bytes([body[at + 4], body[at + 5], body[at + 6], body[at + 7]])
                as usize;
            let start = at + 8;
            let end = start.saturating_add(len).min(body.len());
            if start.saturating_add(len) > body.len() {
                partial_frame = true;
            }
            if !matches!(stream, 0..=2) {
                // Not a frame header after all: keep the rest raw.
                frames.push((1, body[at..].to_vec()));
                break;
            }
            let stream = if stream == 0 { 1 } else { stream };
            frames.push((stream, body[start..end].to_vec()));
            at = end;
            if len == 0 && end == start {
                // Zero-length frame: keep going (no progress hazard: `at`
                // advanced by the 8-byte header).
                continue;
            }
        }
        if at < body.len() && at + 8 > body.len() {
            // Trailing bytes shorter than a header.
            partial_frame = true;
        }
    }

    let total_bytes: usize = frames.iter().map(|(_, p)| p.len()).sum();
    let mut truncated = false;
    if total_bytes > max_bytes {
        truncated = true;
        let mut budget = max_bytes;
        let mut keep_from = frames.len();
        while keep_from > 0 {
            let len = frames[keep_from - 1].1.len();
            if len <= budget {
                budget -= len;
                keep_from -= 1;
            } else {
                break;
            }
        }
        if keep_from > 0 && budget > 0 {
            // Keep the tail of the frame that straddles the budget.
            let (stream, payload) = &frames[keep_from - 1];
            let cut = payload.len() - budget;
            let text = String::from_utf8_lossy(payload);
            let mut idx = cut.min(text.len());
            while idx < text.len() && !text.is_char_boundary(idx) {
                idx += 1;
            }
            let tail = text[idx..].as_bytes().to_vec();
            frames[keep_from - 1] = (*stream, tail);
            keep_from -= 1;
        }
        frames.drain(..keep_from);
    }

    let mut out = LogOutput {
        tty,
        truncated,
        total_bytes,
        partial_frame,
        frames: frames.len(),
        ..LogOutput::default()
    };
    let both = frames.iter().any(|(s, _)| *s == 1) && frames.iter().any(|(s, _)| *s == 2);
    for (stream, payload) in &frames {
        out.bytes += payload.len();
        let mut text = String::from_utf8_lossy(payload).into_owned();
        if tty {
            text = text.replace("\r\n", "\n");
        }
        if *stream == 2 {
            out.stderr.push_str(&text);
        } else {
            out.stdout.push_str(&text);
        }
        if both {
            let prefix = if *stream == 2 { "err|" } else { "out|" };
            for line in text.split_inclusive('\n') {
                out.combined.push_str(prefix);
                out.combined.push_str(line);
            }
            if !text.ends_with('\n') && !text.is_empty() {
                out.combined.push('\n');
            }
        } else {
            out.combined.push_str(&text);
        }
    }
    out
}

// --- pull progress -----------------------------------------------------------------

/// Summary of a `POST /images/create` NDJSON progress stream.
#[derive(Debug, Default)]
pub struct PullSummary {
    /// Final status per layer id (`Already exists`, `Pull complete`, ...).
    pub layers: BTreeMap<String, String>,
    /// Status lines without a layer id, in order (`Digest: ...`, `Status: ...`).
    pub status_lines: Vec<String>,
    pub digest: Option<String>,
    pub lines: usize,
    /// An `{error, errorDetail}` line ended the stream.
    pub error: Option<String>,
    /// The last [`MAX_PROGRESS_LINES`] raw lines.
    pub raw_tail: Vec<Value>,
}

pub fn parse_pull_stream(body: &[u8]) -> PullSummary {
    let mut summary = PullSummary::default();
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        summary.lines += 1;
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(err) = value.get("error").and_then(Value::as_str) {
            let detail = value
                .pointer("/errorDetail/message")
                .and_then(Value::as_str)
                .unwrap_or(err);
            summary.error = Some(bounded(detail, SNIPPET_CHARS));
        }
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        // Docker's opening line (`Pulling from library/alpine`) carries the
        // tag as `id`; it is not a layer.
        let layer_id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|_| !status.starts_with("Pulling from"));
        match layer_id {
            Some(id) if !status.is_empty() => {
                if summary.layers.len() < MAX_LAYERS || summary.layers.contains_key(id) {
                    summary.layers.insert(bounded(id, 80), bounded(status, 80));
                }
            }
            _ if !status.is_empty() => {
                if let Some(digest) = status.strip_prefix("Digest: ") {
                    summary.digest = Some(bounded(digest.trim(), 100));
                }
                if summary.status_lines.len() < MAX_PROGRESS_LINES {
                    summary.status_lines.push(bounded(status, 200));
                }
            }
            _ => {}
        }
        if summary.raw_tail.len() >= MAX_PROGRESS_LINES {
            summary.raw_tail.remove(0);
        }
        summary.raw_tail.push(value);
    }
    summary
}

// --- output trimming -----------------------------------------------------------------

/// First `max` characters of `s` (never a byte cut).
pub fn bounded(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// `sha256:abcdef...` / full ids -> the 12-char short form.
pub fn short_id(id: &str) -> String {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.chars().take(12).collect()
}

pub fn human_bytes(n: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = n.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", value as u64, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn str_of(v: &Value, key: &str) -> Value {
    v.get(key).cloned().unwrap_or(Value::Null)
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f.max(0.0) as u64)))
        .unwrap_or(0)
}

fn f64_of(v: &Value, pointer: &str) -> f64 {
    v.pointer(pointer).and_then(Value::as_f64).unwrap_or(0.0)
}

fn count_of(v: &Value, key: &str) -> usize {
    match v.get(key) {
        Some(Value::Object(m)) => m.len(),
        Some(Value::Array(a)) => a.len(),
        _ => 0,
    }
}

/// Unix seconds -> RFC 3339 (UTC), for `Created` fields.
pub fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn created_field(v: &Value) -> Value {
    match v.get("Created") {
        Some(Value::Number(n)) => n
            .as_i64()
            .map(|s| Value::String(rfc3339(s)))
            .unwrap_or(Value::Null),
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// `host:pub->priv/proto` strings from a `containers/json` `Ports` array.
pub fn render_ports(ports: &Value) -> Vec<String> {
    let Some(list) = ports.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<String> = list
        .iter()
        .take(MAX_ROWS)
        .map(|p| {
            let private = u64_of(p, "PrivatePort");
            let proto = p.get("Type").and_then(Value::as_str).unwrap_or("tcp");
            match p.get("PublicPort").and_then(Value::as_u64) {
                Some(public) => format!(
                    "{}:{public}->{private}/{proto}",
                    p.get("IP").and_then(Value::as_str).unwrap_or("")
                ),
                None => format!("{private}/{proto}"),
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One `list_containers` row.
pub fn container_row(v: &Value) -> Value {
    let names: Vec<String> = v
        .get("Names")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|n| n.trim_start_matches('/').to_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut row = json!({
        "id": short_id(v.get("Id").and_then(Value::as_str).unwrap_or("")),
        "names": names,
        "image": str_of(v, "Image"),
        "command": v.get("Command").and_then(Value::as_str).map(|c| bounded(c, 200)),
        "state": str_of(v, "State"),
        "status": str_of(v, "Status"),
        "created": created_field(v),
        "ports": render_ports(v.get("Ports").unwrap_or(&Value::Null)),
        "labels": count_of(v, "Labels"),
    });
    if let Some(size) = v.get("SizeRw") {
        row["size_rw"] = size.clone();
        row["size_root_fs"] = str_of(v, "SizeRootFs");
    }
    row
}

/// Env keys that look like credentials (heuristic, key-name based).
pub fn looks_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["pass", "secret", "token", "key", "credential", "auth"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// `["KEY=VALUE", ...]` with secret-looking values replaced by `***`.
pub fn redact_env(env: &Value, include_values: bool) -> Value {
    let Some(list) = env.as_array() else {
        return Value::Null;
    };
    Value::Array(
        list.iter()
            .take(MAX_ROWS)
            .map(|entry| {
                let Some(text) = entry.as_str() else {
                    return entry.clone();
                };
                match text.split_once('=') {
                    Some((key, _)) if !include_values && looks_secret(key) => {
                        Value::String(format!("{key}=***"))
                    }
                    _ => Value::String(bounded(text, 2000)),
                }
            })
            .collect(),
    )
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for key in keys {
        if let Some(value) = v.get(*key) {
            if !value.is_null() {
                out.insert((*key).to_owned(), value.clone());
            }
        }
    }
    Value::Object(out)
}

/// Trimmed `inspect_container` document.
pub fn container_detail(v: &Value, include_env_values: bool) -> Value {
    let config = v.get("Config").cloned().unwrap_or(Value::Null);
    let host = v.get("HostConfig").cloned().unwrap_or(Value::Null);
    let net = v.get("NetworkSettings").cloned().unwrap_or(Value::Null);
    let networks: serde_json::Map<String, Value> = net
        .get("Networks")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .take(MAX_ROWS)
                .map(|(name, n)| {
                    (
                        name.clone(),
                        json!({
                            "ip_address": str_of(n, "IPAddress"),
                            "gateway": str_of(n, "Gateway"),
                            "mac_address": str_of(n, "MacAddress"),
                            "network_id": n.get("NetworkID").and_then(Value::as_str).map(short_id),
                        }),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let mut cfg = pick(
        &config,
        &[
            "Image",
            "Cmd",
            "Entrypoint",
            "WorkingDir",
            "User",
            "Tty",
            "Labels",
            "ExposedPorts",
            "Hostname",
        ],
    );
    cfg["Env"] = redact_env(
        config.get("Env").unwrap_or(&Value::Null),
        include_env_values,
    );
    let mut env_redacted = false;
    if !include_env_values {
        if let Some(list) = config.get("Env").and_then(Value::as_array) {
            env_redacted = list.iter().filter_map(Value::as_str).any(|e| {
                e.split_once('=')
                    .map(|(k, _)| looks_secret(k))
                    .unwrap_or(false)
            });
        }
    }
    json!({
        "id": v.get("Id").and_then(Value::as_str).map(short_id),
        "full_id": str_of(v, "Id"),
        "name": v.get("Name").and_then(Value::as_str).map(|n| n.trim_start_matches('/')),
        "created": str_of(v, "Created"),
        "path": str_of(v, "Path"),
        "args": str_of(v, "Args"),
        "state": pick(
            v.get("State").unwrap_or(&Value::Null),
            &["Status", "Running", "Paused", "Restarting", "OOMKilled", "Dead", "Pid", "ExitCode", "Error", "StartedAt", "FinishedAt", "Health"],
        ),
        "image": str_of(v, "Image"),
        "restart_count": str_of(v, "RestartCount"),
        "platform": str_of(v, "Platform"),
        "config": cfg,
        "env_redacted": env_redacted,
        "host_config": pick(
            &host,
            &["RestartPolicy", "PortBindings", "Binds", "NetworkMode", "Privileged", "AutoRemove", "Memory", "NanoCpus", "CapAdd", "CapDrop", "LogConfig", "ReadonlyRootfs"],
        ),
        "mounts": v.get("Mounts").and_then(Value::as_array).map(|m| {
            m.iter().take(MAX_ROWS).map(|x| pick(x, &["Type", "Name", "Source", "Destination", "Mode", "RW"])).collect::<Vec<_>>()
        }),
        "network_settings": {
            "ports": str_of(&net, "Ports"),
            "networks": networks,
        },
    })
}

/// One `list_images` row.
pub fn image_row(v: &Value, digests: bool) -> Value {
    let tags: Vec<String> = v
        .get("RepoTags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let dangling = tags.is_empty() || tags.iter().all(|t| t == "<none>:<none>");
    let size = u64_of(v, "Size");
    let mut row = json!({
        "id": short_id(v.get("Id").and_then(Value::as_str).unwrap_or("")),
        "repo_tags": tags,
        "created": created_field(v),
        "size": size,
        "size_human": human_bytes(size as f64),
        "containers": v.get("Containers").and_then(Value::as_i64).filter(|c| *c >= 0),
        "labels": count_of(v, "Labels"),
        "dangling": dangling,
    });
    if digests {
        row["repo_digests"] = str_of(v, "RepoDigests");
    }
    row
}

/// Trimmed `inspect_image` document.
pub fn image_detail(v: &Value, include_env_values: bool) -> Value {
    let config = v.get("Config").cloned().unwrap_or(Value::Null);
    let mut cfg = pick(
        &config,
        &[
            "Cmd",
            "Entrypoint",
            "ExposedPorts",
            "Labels",
            "WorkingDir",
            "User",
            "Volumes",
            "StopSignal",
        ],
    );
    cfg["Env"] = redact_env(
        config.get("Env").unwrap_or(&Value::Null),
        include_env_values,
    );
    let size = u64_of(v, "Size");
    json!({
        "id": v.get("Id").and_then(Value::as_str).map(short_id),
        "full_id": str_of(v, "Id"),
        "repo_tags": str_of(v, "RepoTags"),
        "repo_digests": str_of(v, "RepoDigests"),
        "created": str_of(v, "Created"),
        "size": size,
        "size_human": human_bytes(size as f64),
        "architecture": str_of(v, "Architecture"),
        "os": str_of(v, "Os"),
        "variant": str_of(v, "Variant"),
        "author": str_of(v, "Author"),
        "config": cfg,
        "rootfs_layers": v.pointer("/RootFS/Layers").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
    })
}

pub fn network_row(v: &Value) -> Value {
    let ipam: Vec<Value> = v
        .pointer("/IPAM/Config")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(MAX_ROWS)
                .map(|c| json!({"subnet": str_of(c, "Subnet"), "gateway": str_of(c, "Gateway")}))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": v.get("Id").and_then(Value::as_str).map(short_id),
        "name": str_of(v, "Name"),
        "driver": str_of(v, "Driver"),
        "scope": str_of(v, "Scope"),
        "internal": str_of(v, "Internal"),
        "attachable": str_of(v, "Attachable"),
        "enable_ipv6": str_of(v, "EnableIPv6"),
        "ipam": ipam,
        "containers": count_of(v, "Containers"),
        "labels": count_of(v, "Labels"),
        "created": str_of(v, "Created"),
    })
}

pub fn volume_row(v: &Value) -> Value {
    json!({
        "name": str_of(v, "Name"),
        "driver": str_of(v, "Driver"),
        "mountpoint": str_of(v, "Mountpoint"),
        "scope": str_of(v, "Scope"),
        "created_at": str_of(v, "CreatedAt"),
        "labels": count_of(v, "Labels"),
        "options": str_of(v, "Options"),
        "usage": v.get("UsageData").map(|u| json!({"size": str_of(u, "Size"), "ref_count": str_of(u, "RefCount")})),
    })
}

/// `GET /version` trimmed, plus whether the configured prefix fits the
/// daemon's window.
pub fn version_summary(v: &Value, configured: &str) -> Value {
    let components: Vec<Value> = v
        .get("Components")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(50)
                .map(|c| json!({"name": str_of(c, "Name"), "version": str_of(c, "Version")}))
                .collect()
        })
        .unwrap_or_default();
    let engine = components
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .find(|n| n.contains("Engine"))
        .map(|n| {
            if n.contains("Podman") {
                "podman"
            } else {
                "docker"
            }
        })
        .unwrap_or("docker");
    let api = v.get("ApiVersion").and_then(Value::as_str).unwrap_or("");
    let min = v.get("MinAPIVersion").and_then(Value::as_str).unwrap_or("");
    let configured_num = configured.trim_start_matches('v');
    let ok = version_in_window(configured_num, min, api);
    json!({
        "engine": engine,
        "version": str_of(v, "Version"),
        "api_version": api,
        "min_api_version": min,
        "configured_api_version": configured,
        "api_version_ok": ok,
        "os": str_of(v, "Os"),
        "arch": str_of(v, "Arch"),
        "kernel_version": str_of(v, "KernelVersion"),
        "go_version": str_of(v, "GoVersion"),
        "build_time": str_of(v, "BuildTime"),
        "experimental": str_of(v, "Experimental"),
        "platform": v.pointer("/Platform/Name").cloned().unwrap_or(Value::Null),
        "components": components,
    })
}

/// `min <= value <= max` on dotted numeric versions; unknown bounds pass.
pub fn version_in_window(value: &str, min: &str, max: &str) -> bool {
    fn parse(v: &str) -> Option<(u64, u64)> {
        let (a, b) = v.trim().split_once('.')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    }
    let Some(value) = parse(value) else {
        return false;
    };
    let min_ok = parse(min).map(|m| value >= m).unwrap_or(true);
    let max_ok = parse(max).map(|m| value <= m).unwrap_or(true);
    min_ok && max_ok
}

/// `GET /info` trimmed to what an agent needs.
pub fn info_summary(v: &Value) -> Value {
    let runtimes: Vec<String> = v
        .get("Runtimes")
        .and_then(Value::as_object)
        .map(|m| m.keys().take(50).cloned().collect())
        .unwrap_or_default();
    let mut out = pick(
        v,
        &[
            "ServerVersion",
            "Name",
            "ID",
            "OperatingSystem",
            "OSType",
            "Architecture",
            "KernelVersion",
            "NCPU",
            "MemTotal",
            "Containers",
            "ContainersRunning",
            "ContainersPaused",
            "ContainersStopped",
            "Images",
            "Driver",
            "CgroupVersion",
            "CgroupDriver",
            "Rootless",
            "LoggingDriver",
            "DefaultRuntime",
            "DockerRootDir",
            "Warnings",
        ],
    );
    out["Runtimes"] = json!(runtimes);
    out["SwarmLocalNodeState"] = v
        .pointer("/Swarm/LocalNodeState")
        .cloned()
        .unwrap_or(Value::Null);
    out["MemTotalHuman"] = json!(human_bytes(u64_of(v, "MemTotal") as f64));
    out
}

/// One `GET /stats?stream=false` sample reduced to the numbers `docker stats`
/// shows.
pub fn stats_summary(v: &Value) -> Value {
    let cpu_total = f64_of(v, "/cpu_stats/cpu_usage/total_usage");
    let pre_total = f64_of(v, "/precpu_stats/cpu_usage/total_usage");
    let sys_total = f64_of(v, "/cpu_stats/system_cpu_usage");
    let pre_sys = f64_of(v, "/precpu_stats/system_cpu_usage");
    let online = v
        .pointer("/cpu_stats/online_cpus")
        .and_then(Value::as_f64)
        .filter(|n| *n > 0.0)
        .or_else(|| {
            v.pointer("/cpu_stats/cpu_usage/percpu_usage")
                .and_then(Value::as_array)
                .map(|a| a.len() as f64)
                .filter(|n| *n > 0.0)
        })
        .unwrap_or(1.0);
    let cpu_delta = (cpu_total - pre_total).max(0.0);
    let sys_delta = (sys_total - pre_sys).max(0.0);
    let cpu_percent = if sys_delta > 0.0 && cpu_delta > 0.0 {
        cpu_delta / sys_delta * online * 100.0
    } else {
        0.0
    };
    let mem_usage_raw = f64_of(v, "/memory_stats/usage");
    let cache = v
        .pointer("/memory_stats/stats/total_inactive_file")
        .and_then(Value::as_f64)
        .or_else(|| {
            v.pointer("/memory_stats/stats/inactive_file")
                .and_then(Value::as_f64)
        })
        .unwrap_or(0.0);
    let mem_usage = (mem_usage_raw - cache).max(0.0);
    let mem_limit = f64_of(v, "/memory_stats/limit");
    let mem_percent = if mem_limit > 0.0 {
        mem_usage / mem_limit * 100.0
    } else {
        0.0
    };
    let (mut rx, mut tx) = (0.0, 0.0);
    if let Some(nets) = v.get("networks").and_then(Value::as_object) {
        for n in nets.values().take(MAX_ROWS) {
            rx += f64_of(n, "/rx_bytes");
            tx += f64_of(n, "/tx_bytes");
        }
    }
    let (mut blk_read, mut blk_write) = (0.0, 0.0);
    if let Some(entries) = v
        .pointer("/blkio_stats/io_service_bytes_recursive")
        .and_then(Value::as_array)
    {
        for e in entries.iter().take(MAX_ROWS) {
            let op = e
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let value = f64_of(e, "/value");
            if op == "read" {
                blk_read += value;
            } else if op == "write" {
                blk_write += value;
            }
        }
    }
    let finite = |x: f64| if x.is_finite() { x } else { 0.0 };
    json!({
        "read": str_of(v, "read"),
        "name": v.get("name").and_then(Value::as_str).map(|n| n.trim_start_matches('/')),
        "id": v.get("id").and_then(Value::as_str).map(short_id),
        "cpu_percent": (finite(cpu_percent) * 100.0).round() / 100.0,
        "online_cpus": online,
        "memory_usage": finite(mem_usage) as u64,
        "memory_usage_human": human_bytes(mem_usage),
        "memory_limit": finite(mem_limit) as u64,
        "memory_limit_human": human_bytes(mem_limit),
        "memory_percent": (finite(mem_percent) * 100.0).round() / 100.0,
        "network_rx_bytes": finite(rx) as u64,
        "network_tx_bytes": finite(tx) as u64,
        "block_read_bytes": finite(blk_read) as u64,
        "block_write_bytes": finite(blk_write) as u64,
        "pids": v.pointer("/pids_stats/current").cloned().unwrap_or(Value::Null),
    })
}

/// `docker system df` totals computed from the `GET /system/df` arrays.
pub fn df_summary(v: &Value, detail: bool) -> Value {
    let images = v.get("Images").and_then(Value::as_array);
    let containers = v.get("Containers").and_then(Value::as_array);
    let volumes = v.get("Volumes").and_then(Value::as_array);
    let cache = v.get("BuildCache").and_then(Value::as_array);

    let (img_total, img_active, img_size, img_reclaim) = images
        .map(|list| {
            let mut active = 0usize;
            let mut size = 0.0;
            let mut reclaim = 0.0;
            for i in list {
                let s = u64_of(i, "Size") as f64;
                size += s;
                if i.get("Containers").and_then(Value::as_i64).unwrap_or(0) > 0 {
                    active += 1;
                } else {
                    reclaim += s;
                }
            }
            (list.len(), active, size, reclaim)
        })
        .unwrap_or((0, 0, 0.0, 0.0));
    let (ctr_total, ctr_active, ctr_size, ctr_reclaim) = containers
        .map(|list| {
            let mut active = 0usize;
            let mut size = 0.0;
            let mut reclaim = 0.0;
            for c in list {
                let s = u64_of(c, "SizeRw") as f64;
                size += s;
                if c.get("State").and_then(Value::as_str) == Some("running") {
                    active += 1;
                } else {
                    reclaim += s;
                }
            }
            (list.len(), active, size, reclaim)
        })
        .unwrap_or((0, 0, 0.0, 0.0));
    let (vol_total, vol_active, vol_size, vol_reclaim) = volumes
        .map(|list| {
            let mut active = 0usize;
            let mut size = 0.0;
            let mut reclaim = 0.0;
            for vol in list {
                let usage = vol.get("UsageData").unwrap_or(&Value::Null);
                let s = usage
                    .get("Size")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .max(0.0);
                size += s;
                if usage.get("RefCount").and_then(Value::as_i64).unwrap_or(0) > 0 {
                    active += 1;
                } else {
                    reclaim += s;
                }
            }
            (list.len(), active, size, reclaim)
        })
        .unwrap_or((0, 0, 0.0, 0.0));
    let (bc_total, bc_active, bc_size, bc_reclaim) = cache
        .map(|list| {
            let mut active = 0usize;
            let mut size = 0.0;
            let mut reclaim = 0.0;
            for b in list {
                let s = u64_of(b, "Size") as f64;
                size += s;
                if b.get("InUse").and_then(Value::as_bool).unwrap_or(false) {
                    active += 1;
                } else {
                    reclaim += s;
                }
            }
            (list.len(), active, size, reclaim)
        })
        .unwrap_or((0, 0, 0.0, 0.0));
    let cat = |total: usize, active: usize, size: f64, reclaim: f64, present: bool| {
        json!({
            "total": total,
            "active": active,
            "size": size as u64,
            "size_human": human_bytes(size),
            "reclaimable": reclaim as u64,
            "reclaimable_human": human_bytes(reclaim),
            "reported": present,
        })
    };
    let mut out = json!({
        "layers_size": u64_of(v, "LayersSize"),
        "layers_size_human": human_bytes(u64_of(v, "LayersSize") as f64),
        "images": cat(img_total, img_active, img_size, img_reclaim, images.is_some()),
        "containers": cat(ctr_total, ctr_active, ctr_size, ctr_reclaim, containers.is_some()),
        "volumes": cat(vol_total, vol_active, vol_size, vol_reclaim, volumes.is_some()),
        "build_cache": cat(bc_total, bc_active, bc_size, bc_reclaim, cache.is_some()),
    });
    if detail {
        let mut imgs: Vec<&Value> = images.map(|l| l.iter().collect()).unwrap_or_default();
        imgs.sort_by_key(|b| std::cmp::Reverse(u64_of(b, "Size")));
        out["image_items"] = Value::Array(
            imgs.iter()
                .take(MAX_DF_ITEMS)
                .map(|i| {
                    json!({
                        "id": i.get("Id").and_then(Value::as_str).map(short_id),
                        "repo_tags": str_of(i, "RepoTags"),
                        "size": u64_of(i, "Size"),
                        "shared_size": str_of(i, "SharedSize"),
                        "containers": str_of(i, "Containers"),
                    })
                })
                .collect(),
        );
        out["container_items"] = Value::Array(
            containers
                .map(|l| {
                    l.iter()
                        .take(MAX_DF_ITEMS)
                        .map(|c| {
                            json!({
                                "id": c.get("Id").and_then(Value::as_str).map(short_id),
                                "names": c.get("Names").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|n| n.trim_start_matches('/').to_owned()).collect::<Vec<_>>()),
                                "image": str_of(c, "Image"),
                                "state": str_of(c, "State"),
                                "size_rw": u64_of(c, "SizeRw"),
                                "size_root_fs": u64_of(c, "SizeRootFs"),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        );
        out["volume_items"] = Value::Array(
            volumes
                .map(|l| {
                    l.iter()
                        .take(MAX_DF_ITEMS)
                        .map(|vol| {
                            json!({
                                "name": str_of(vol, "Name"),
                                "size": vol.pointer("/UsageData/Size").cloned().unwrap_or(Value::Null),
                                "ref_count": vol.pointer("/UsageData/RefCount").cloned().unwrap_or(Value::Null),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        );
    }
    out
}

/// A `raw=true` document bounded to [`RAW_CAP_CHARS`] characters of JSON.
pub fn bound_raw(value: &Value) -> Value {
    let text = serde_json::to_string(value).unwrap_or_default();
    if text.chars().count() <= RAW_CAP_CHARS {
        return value.clone();
    }
    json!({
        "truncated": true,
        "cap_chars": RAW_CAP_CHARS,
        "raw_prefix": bounded(&text, RAW_CAP_CHARS),
    })
}
