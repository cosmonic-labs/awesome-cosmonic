//! The MCP server implementation: tool definitions and result rendering.
//!
//! Every tool reads its configuration from the environment on each call,
//! validates parameters in-guest (ids, references, clamps, filter keys),
//! sends the request through [`crate::docker::Client`], and renders the
//! answer as `structuredContent` plus a readable text block. Upstream and
//! policy failures come back as `CallToolResult::error` so the caller sees
//! the message; JSON-RPC errors are reserved for requests the server cannot
//! route at all.
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use std::collections::BTreeMap;

use bytes::Bytes;
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

use crate::docker::{
    self, Client, Config, Error, Query, Reply, TimeSpec, DEFAULT_IMAGE_LIMIT, DEFAULT_LIST_LIMIT,
    DEFAULT_LOG_BYTES, DEFAULT_STOP_TIMEOUT, DEFAULT_TAIL, MAX_IMAGE_LIMIT, MAX_LIST_LIMIT,
    MAX_LOG_BYTES, MAX_PORTS, MAX_ROWS, MAX_STOP_TIMEOUT, MAX_TAIL, MIN_LOG_BYTES,
    MIN_MEMORY_BYTES, WAIT_LOG_TAIL,
};
use crate::skills;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so do not keep per-session
/// state on this struct.
#[derive(Clone)]
pub struct DockerServer {
    tool_router: ToolRouter<Self>,
}

// --- parameter types ----------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InfoParams {
    /// Return the daemon's full `/info` document (capped at 200 KiB of JSON)
    /// instead of the trimmed summary. Default false.
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListContainersParams {
    /// Include stopped/exited containers. Default true (the Engine API's own
    /// default is false).
    pub all: Option<bool>,
    /// Maximum rows, clamped to 1..=500 (default 100). The daemon returns the
    /// most recently created containers first.
    pub limit: Option<i64>,
    /// Add `size_rw` / `size_root_fs` per row (slower). Default false.
    pub size: Option<bool>,
    /// Engine filters as an object of string arrays, e.g.
    /// `{"status":["running"],"label":["app=web"]}`. Keys: ancestor, before,
    /// expose, exited, health, id, isolation, is-task, label, name, network,
    /// publish, since, status (created|restarting|running|removing|paused|
    /// exited|dead; podman also `stopped`), volume. A bare string value is
    /// accepted for a single value.
    pub filters: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectContainerParams {
    /// Container name or id (12-char short ids from list_containers work).
    pub id: String,
    /// Include `SizeRw` / `SizeRootFs` (slower). Default false.
    pub size: Option<bool>,
    /// Show the values of environment variables whose key looks like a
    /// credential (pass/secret/token/key/credential/auth). Default false:
    /// those values are rendered as `***`.
    pub include_env_values: Option<bool>,
    /// Return the daemon's full inspect document (capped at 200 KiB) instead
    /// of the trimmed view. Env redaction does not apply to raw output.
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContainerLogsParams {
    /// Container name or id.
    pub id: String,
    /// Number of most recent lines, clamped to 1..=5000 (default 200).
    /// `all` is never sent.
    pub tail: Option<i64>,
    /// Only lines after this time: unix seconds or an RFC 3339 timestamp.
    pub since: Option<TimeSpec>,
    /// Only lines before this time: unix seconds or an RFC 3339 timestamp.
    pub until: Option<TimeSpec>,
    /// Prefix each line with its RFC 3339 timestamp. Default false.
    pub timestamps: Option<bool>,
    /// Include stdout. Default true.
    pub stdout: Option<bool>,
    /// Include stderr. Default true. At least one stream must be requested.
    pub stderr: Option<bool>,
    /// Cap on returned log bytes, clamped to 1024..=1048576 (default 65536).
    /// The newest bytes are kept and `truncated` is set when the cap trips.
    pub max_bytes: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContainerStatsParams {
    /// Container name or id (must be running on podman).
    pub id: String,
    /// Also return the daemon's raw stats sample. Default false.
    pub raw: Option<bool>,
}

/// Environment for run_container: either `["KEY=VALUE", ...]` or an object
/// `{"KEY": "VALUE"}`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum EnvSpec {
    List(Vec<String>),
    Map(BTreeMap<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    No,
    Always,
    UnlessStopped,
    OnFailure,
}

impl RestartPolicy {
    fn name(self) -> &'static str {
        match self {
            RestartPolicy::No => "no",
            RestartPolicy::Always => "always",
            RestartPolicy::UnlessStopped => "unless-stopped",
            RestartPolicy::OnFailure => "on-failure",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunContainerParams {
    /// Image reference with an explicit tag or digest (`alpine:3.20`,
    /// `ghcr.io/org/app@sha256:...`). Must already be in the local store
    /// unless pull_if_missing is true.
    pub image: String,
    /// Container name (`^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}$`). Daemon-generated
    /// when omitted.
    pub name: Option<String>,
    /// Command (argv) overriding the image's CMD.
    pub cmd: Option<Vec<String>>,
    /// Entrypoint (argv) overriding the image's ENTRYPOINT.
    pub entrypoint: Option<Vec<String>>,
    /// Environment: `["KEY=VALUE", ...]` or `{"KEY":"VALUE"}` (<= 200 entries,
    /// 8 KiB total).
    pub env: Option<EnvSpec>,
    /// Labels to set on the container.
    pub labels: Option<BTreeMap<String, String>>,
    /// Working directory inside the container.
    pub working_dir: Option<String>,
    /// User (name or uid[:gid]) the process runs as.
    pub user: Option<String>,
    /// Published ports as `[hostip:]hostport:containerport[/proto]`, e.g.
    /// `8080:80` or `0.0.0.0:8080:80/tcp`. The host ip defaults to 127.0.0.1
    /// (never 0.0.0.0 unless given); host port 0 lets the daemon pick.
    pub ports: Option<Vec<String>>,
    /// Restart policy: no (default), always, unless-stopped, on-failure.
    pub restart_policy: Option<RestartPolicy>,
    /// Memory limit in bytes (>= 6 MiB).
    pub memory_bytes: Option<u64>,
    /// CPU quota in CPUs (e.g. 0.5, 2), sent as NanoCpus.
    pub cpus: Option<f64>,
    /// Platform `os[/arch[/variant]]` for multi-arch images.
    pub platform: Option<String>,
    /// Wait for the container to exit (the daemon's blocking wait, bounded by
    /// MCP_OUTBOUND_TIMEOUT_MS) and return its exit code plus the last 200
    /// log lines. Default false. Only for short-lived commands.
    pub wait: Option<bool>,
    /// On "No such image", run the pull_image path once and retry the create.
    /// Default false.
    pub pull_if_missing: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContainerIdParams {
    /// Container name or id.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StopContainerParams {
    /// Container name or id.
    pub id: String,
    /// Seconds to wait after the stop signal before SIGKILL, clamped to
    /// 0..=300 (default 10) and to the outbound deadline minus 5 s.
    pub timeout: Option<i64>,
    /// Signal to send first (default: the container's StopSignal, usually
    /// SIGTERM). Name (`SIGTERM`/`TERM`) or number.
    pub signal: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KillContainerParams {
    /// Container name or id (must be running).
    pub id: String,
    /// Signal name (`SIGKILL`/`KILL`/`SIGHUP`) or number. Default SIGKILL.
    pub signal: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveContainerParams {
    /// Container name or id.
    pub id: String,
    /// Kill a running container before removing it. Default false.
    pub force: Option<bool>,
    /// Also remove anonymous volumes attached to the container. Default false.
    pub volumes: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListImagesParams {
    /// Include intermediate layers. Default false.
    pub all: Option<bool>,
    /// Include `repo_digests` per row. Default false.
    pub digests: Option<bool>,
    /// Engine filters as an object of string arrays: reference
    /// (`["alpine:*"]`), dangling (`["true"]`), label, before, since, until.
    pub filters: Option<BTreeMap<String, Value>>,
    /// Maximum rows after sorting by creation time (newest first), clamped to
    /// 1..=1000 (default 200).
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectImageParams {
    /// Image name (`name[:tag|@digest]`) or id.
    pub name: String,
    /// Show the values of secret-looking Env entries. Default false.
    pub include_env_values: Option<bool>,
    /// Return the daemon's full inspect document (capped at 200 KiB).
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PullImageParams {
    /// Image reference `repo[:tag|@sha256:digest]`. Without a tag or digest
    /// the tool pulls `latest` and says so (an empty tag would make the
    /// daemon pull EVERY tag of the repository).
    pub image: String,
    /// Platform `os[/arch[/variant]]` to pull for multi-arch images.
    pub platform: Option<String>,
    /// Also return the last 200 raw progress lines. Default false.
    pub raw_progress: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveImageParams {
    /// Image name (`name[:tag|@digest]`) or id.
    pub name: String,
    /// Remove even when stopped containers or other tags reference it.
    /// Cannot override a running container. Default false.
    pub force: Option<bool>,
    /// Keep untagged parent layers. Default false.
    pub noprune: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListNetworksParams {
    /// Engine filters as an object of string arrays: dangling, driver, id,
    /// label, name, scope, type (`builtin`|`custom`).
    pub filters: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListVolumesParams {
    /// Engine filters as an object of string arrays: dangling, driver, label,
    /// name.
    pub filters: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemDfParams {
    /// Add per-item lists: the 200 largest images, containers and volumes
    /// with their sizes. Default false.
    pub detail: Option<bool>,
}

// --- filter keys the Engine API documents per endpoint ------------------------

const CONTAINER_FILTERS: &[&str] = &[
    "ancestor",
    "before",
    "expose",
    "exited",
    "health",
    "id",
    "isolation",
    "is-task",
    "label",
    "name",
    "network",
    "publish",
    "since",
    "status",
    "volume",
];
const IMAGE_FILTERS: &[&str] = &["before", "dangling", "label", "reference", "since", "until"];
const NETWORK_FILTERS: &[&str] = &["dangling", "driver", "id", "label", "name", "scope", "type"];
const VOLUME_FILTERS: &[&str] = &["dangling", "driver", "label", "name"];

// --- small helpers -------------------------------------------------------------

/// A structured result with a hand-written readable text block instead of
/// the JSON dump `CallToolResult::structured` would produce.
fn shaped(value: Value, text: impl Into<String>) -> CallToolResult {
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

fn failed(err: &Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

fn finish(outcome: Result<CallToolResult, Error>) -> Result<CallToolResult, ErrorData> {
    Ok(outcome.unwrap_or_else(|err| failed(&err)))
}

fn clamp(value: Option<i64>, default: i64, min: i64, max: i64) -> i64 {
    value.unwrap_or(default).clamp(min, max)
}

fn require_writes(cfg: &Config, tool: &str) -> Result<(), Error> {
    if cfg.read_only {
        return Err(Error::Gated(docker::read_only_refusal(tool)));
    }
    Ok(())
}

fn client(cfg: &Config) -> Result<Client, Error> {
    Client::new(cfg)
}

/// Pretty JSON for text fallbacks, never failing.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn json_body(reply: &Reply, path: &str) -> Result<Value, Error> {
    reply.json().ok_or_else(|| Error::Malformed {
        path: path.to_owned(),
        detail: format!(
            "expected a JSON body, got {} bytes of {}",
            reply.body.len(),
            reply.header("content-type").unwrap_or_default()
        ),
    })
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// `GET /containers/{id}/logs` -> demultiplexed output document.
async fn fetch_logs(
    client: &Client,
    id: &str,
    tty: bool,
    tail: i64,
    max_bytes: usize,
    extra: &Query,
) -> Result<Value, Error> {
    let mut query = Query::new();
    query
        .push("follow", "false")
        .push("stdout", "true")
        .push("stderr", "true")
        .push("tail", tail);
    for (k, v) in extra.pairs() {
        query.push(k, v);
    }
    let path = format!("/containers/{id}/logs{}", query.render());
    let reply = client
        .send(http::Method::GET, &path, &[], Bytes::new())
        .await?;
    let reply = Client::ok(reply, &path)?;
    let logs = docker::demux_logs(&reply.body, tty, max_bytes);
    Ok(json!({
        "tty": logs.tty,
        "stdout": logs.stdout,
        "stderr": logs.stderr,
        "combined": logs.combined,
        "truncated": logs.truncated,
        "bytes": logs.bytes,
        "total_bytes": logs.total_bytes,
        "frames": logs.frames,
        "partial_frame": logs.partial_frame,
    }))
}

/// `Config.Tty` of a container, via inspect.
async fn container_tty(client: &Client, id: &str) -> Result<bool, Error> {
    let path = format!("/containers/{id}/json");
    let doc = client.get_json(&path).await?;
    Ok(doc
        .pointer("/Config/Tty")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

/// The pull_image path shared with run_container(pull_if_missing).
async fn pull(
    client: &Client,
    reference: &str,
    platform: Option<&str>,
    raw_progress: bool,
) -> Result<Value, Error> {
    let image = docker::split_image_ref(reference)?;
    let mut query = Query::new();
    query.push("fromImage", &image.repo).push("tag", &image.tag);
    if let Some(p) = platform {
        query.push("platform", p);
    }
    let path = format!("/images/create{}", query.render());
    let mut headers: Vec<(&str, String)> = Vec::new();
    let auth_sent = match client.registry_auth_header()? {
        Some(value) => {
            headers.push(("X-Registry-Auth", value));
            true
        }
        None => false,
    };
    let reply = client
        .send(http::Method::POST, &path, &headers, Bytes::new())
        .await?;
    let reply = Client::ok(reply, &path)?;
    let summary = docker::parse_pull_stream(&reply.body);
    if let Some(error) = summary.error {
        return Err(Error::Api {
            path: path.clone(),
            status: 500,
            message: format!("pull of {reference} failed mid-stream: {error}"),
            retry_after: None,
        });
    }
    let mut out = json!({
        "image": reference,
        "repo": image.repo,
        "tag": image.tag,
        "defaulted_to_latest": image.defaulted,
        "registry_auth_sent": auth_sent,
        "digest": summary.digest,
        "layers": summary.layers,
        "status": summary.status_lines,
        "progress_lines": summary.lines,
    });
    if image.defaulted {
        out["note"] = json!(
            "no tag or digest was given, so `latest` was pulled (an empty tag would pull \
             every tag of the repository)"
        );
    }
    if raw_progress {
        out["raw_progress"] = Value::Array(summary.raw_tail);
    }
    Ok(out)
}

// --- tools -----------------------------------------------------------------------

#[tool_router]
impl DockerServer {
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

    #[tool(
        description = "Connectivity check and daemon identity: engine (docker|podman), Version, \
                       ApiVersion, MinAPIVersion, OS/Arch/kernel, and whether the configured \
                       DOCKER_API_VERSION lies inside the daemon's window. Call this first."
    )]
    #[tracing::instrument(name = "tool.version", skip_all)]
    async fn version(&self) -> Result<CallToolResult, ErrorData> {
        finish(self.version_inner().await)
    }

    #[tool(
        description = "Daemon summary (GET /info) trimmed to server version, host name, OS, \
                       architecture, CPU/memory, container and image counts, storage/cgroup/log \
                       drivers, runtimes, rootless flag and Warnings; raw=true for the full document."
    )]
    #[tracing::instrument(name = "tool.info", skip_all)]
    async fn info(
        &self,
        Parameters(params): Parameters<InfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let doc = client.get_json("/info").await?;
                if params.raw.unwrap_or(false) {
                    let raw = docker::bound_raw(&doc);
                    let text = pretty(&raw);
                    return Ok(shaped(raw, text));
                }
                let summary = docker::info_summary(&doc);
                let text =
                    format!(
                "{} {} on {} ({} {} {}), {} CPU / {}, containers {} (running {}, paused {}, \
                 stopped {}), images {}, driver {}, cgroup v{}, rootless {}, warnings {}",
                str_field(&summary, "Name"),
                str_field(&summary, "ServerVersion"),
                str_field(&summary, "OperatingSystem"),
                str_field(&summary, "OSType"),
                str_field(&summary, "Architecture"),
                str_field(&summary, "KernelVersion"),
                summary.get("NCPU").cloned().unwrap_or(Value::Null),
                str_field(&summary, "MemTotalHuman"),
                summary.get("Containers").cloned().unwrap_or(Value::Null),
                summary.get("ContainersRunning").cloned().unwrap_or(Value::Null),
                summary.get("ContainersPaused").cloned().unwrap_or(Value::Null),
                summary.get("ContainersStopped").cloned().unwrap_or(Value::Null),
                summary.get("Images").cloned().unwrap_or(Value::Null),
                str_field(&summary, "Driver"),
                str_field(&summary, "CgroupVersion"),
                summary.get("Rootless").cloned().unwrap_or(Value::Null),
                summary
                    .get("Warnings")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
            );
                Ok(shaped(summary, text))
            }
            .await,
        )
    }

    #[tool(
        description = "List containers like `docker ps` (all=true by default, so exited ones \
                       appear) with Engine filters {\"status\":[\"running\"],\"name\":[..],\
                       \"label\":[..],\"ancestor\":[..]}; rows carry the 12-char id, names, image, \
                       command, state, status, created, published ports and label count."
    )]
    #[tracing::instrument(name = "tool.list_containers", skip_all)]
    async fn list_containers(
        &self,
        Parameters(params): Parameters<ListContainersParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let limit = clamp(params.limit, DEFAULT_LIST_LIMIT, 1, MAX_LIST_LIMIT);
                let filters = docker::filters_json(
                    params.filters.as_ref(),
                    CONTAINER_FILTERS,
                    "list_containers",
                )?;
                let mut query = Query::new();
                query
                    .push("all", params.all.unwrap_or(true))
                    .push("limit", limit)
                    .push("size", params.size.unwrap_or(false));
                query.push_opt("filters", filters.as_deref());
                let path = format!("/containers/json{}", query.render());
                let doc = client.get_json(&path).await?;
                let list = doc.as_array().ok_or_else(|| Error::Malformed {
                    path: path.clone(),
                    detail: "expected a JSON array of containers".to_owned(),
                })?;
                let rows: Vec<Value> = list
                    .iter()
                    .take(limit as usize)
                    .map(docker::container_row)
                    .collect();
                let mut text = format!("{} container(s) (limit {limit}):\n", rows.len());
                for row in &rows {
                    text.push_str(&format!(
                        "{}  {}  {}  {}  {}  {}\n",
                        str_field(row, "id"),
                        row.get("names")
                            .and_then(Value::as_array)
                            .map(|a| a
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(","))
                            .unwrap_or_default(),
                        str_field(row, "image"),
                        str_field(row, "state"),
                        str_field(row, "status"),
                        row.get("ports")
                            .and_then(Value::as_array)
                            .map(|a| a
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" "))
                            .unwrap_or_default(),
                    ));
                }
                Ok(shaped(
                    json!({
                        "count": rows.len(),
                        "limit": limit,
                        "all": params.all.unwrap_or(true),
                        "containers": rows,
                    }),
                    text,
                ))
            }
            .await,
        )
    }

    #[tool(
        description = "Inspect one container (name or id): trimmed State, Config (Cmd, \
                       Entrypoint, Env with secret-looking values redacted, Labels, ExposedPorts), \
                       HostConfig (RestartPolicy, PortBindings, Binds, NetworkMode, Privileged), \
                       Mounts and network addresses; raw=true for the full document."
    )]
    #[tracing::instrument(name = "tool.inspect_container", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn inspect_container(
        &self,
        Parameters(params): Parameters<InspectContainerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let id = docker::container_ref(&params.id, "id")?;
                let mut query = Query::new();
                if params.size.unwrap_or(false) {
                    query.push("size", "true");
                }
                let path = format!("/containers/{id}/json{}", query.render());
                let doc = client.get_json(&path).await?;
                if params.raw.unwrap_or(false) {
                    let raw = docker::bound_raw(&doc);
                    let text = pretty(&raw);
                    return Ok(shaped(raw, text));
                }
                let detail =
                    docker::container_detail(&doc, params.include_env_values.unwrap_or(false));
                let text = format!(
                    "{} ({}): status {}, image {}, exit code {}, started {}\n{}",
                    str_field(&detail, "name"),
                    str_field(&detail, "id"),
                    detail
                        .pointer("/state/Status")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    detail
                        .pointer("/config/Image")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    detail
                        .pointer("/state/ExitCode")
                        .cloned()
                        .unwrap_or(Value::Null),
                    detail
                        .pointer("/state/StartedAt")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    pretty(&detail),
                );
                Ok(shaped(detail, text))
            }
            .await,
        )
    }

    #[tool(
        description = "Fetch recent stdout/stderr of a container (never follows). Non-TTY \
                       containers' multiplexed frames are demultiplexed into labeled stdout/stderr \
                       text; TTY containers return raw text. tail 1..=5000 (default 200), since/until \
                       as unix seconds or RFC 3339, max_bytes keeps the newest bytes."
    )]
    #[tracing::instrument(name = "tool.container_logs", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn container_logs(
        &self,
        Parameters(params): Parameters<ContainerLogsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let id = docker::container_ref(&params.id, "id")?;
                let want_stdout = params.stdout.unwrap_or(true);
                let want_stderr = params.stderr.unwrap_or(true);
                if !want_stdout && !want_stderr {
                    return Err(Error::invalid(
                        "at least one of stdout/stderr must be true (the daemon answers 400 'you \
                     must choose at least one stream' otherwise)",
                    ));
                }
                let tail = clamp(params.tail, DEFAULT_TAIL, 1, MAX_TAIL);
                let max_bytes = clamp(
                    params.max_bytes,
                    DEFAULT_LOG_BYTES,
                    MIN_LOG_BYTES,
                    MAX_LOG_BYTES,
                );
                let since = params
                    .since
                    .as_ref()
                    .map(|s| docker::unix_seconds(s, "since"))
                    .transpose()?;
                let until = params
                    .until
                    .as_ref()
                    .map(|s| docker::unix_seconds(s, "until"))
                    .transpose()?;
                if let (Some(s), Some(u)) = (since, until) {
                    if u <= s {
                        return Err(Error::invalid("until must be later than since"));
                    }
                }
                let tty = container_tty(&client, &id).await?;
                let mut query = Query::new();
                query
                    .push("follow", "false")
                    .push("stdout", want_stdout)
                    .push("stderr", want_stderr)
                    .push("tail", tail)
                    .push("since", since.unwrap_or(0))
                    .push("until", until.unwrap_or(0))
                    .push("timestamps", params.timestamps.unwrap_or(false));
                let path = format!("/containers/{id}/logs{}", query.render());
                let reply = client
                    .send(http::Method::GET, &path, &[], Bytes::new())
                    .await?;
                let reply = Client::ok(reply, &path)?;
                let logs = docker::demux_logs(&reply.body, tty, max_bytes as usize);
                let mut text = String::new();
                if logs.truncated {
                    text.push_str(&format!(
                        "[truncated: showing the newest {} of {} bytes]\n",
                        logs.bytes, logs.total_bytes
                    ));
                }
                if logs.partial_frame {
                    text.push_str("[the daemon's stream ended inside a frame]\n");
                }
                text.push_str(&logs.combined);
                Ok(shaped(
                    json!({
                        "id": id,
                        "tty": logs.tty,
                        "tail": tail,
                        "stdout": logs.stdout,
                        "stderr": logs.stderr,
                        "combined": logs.combined,
                        "truncated": logs.truncated,
                        "bytes": logs.bytes,
                        "total_bytes": logs.total_bytes,
                        "frames": logs.frames,
                        "partial_frame": logs.partial_frame,
                    }),
                    text,
                ))
            }
            .await,
        )
    }

    #[tool(
        description = "One resource sample of a running container (GET /stats?stream=false): \
                       cpu_percent (from the pre/post CPU deltas like `docker stats`), memory \
                       usage/limit/percent (page cache excluded), network rx/tx, block read/write and \
                       pid count; raw=true adds the daemon's sample."
    )]
    #[tracing::instrument(name = "tool.container_stats", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn container_stats(
        &self,
        Parameters(params): Parameters<ContainerStatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(async {
            let cfg = docker::config();
            let client = client(&cfg)?;
            let id = docker::container_ref(&params.id, "id")?;
            let path = format!("/containers/{id}/stats?stream=false&one-shot=false");
            let doc = client.get_json(&path).await?;
            let mut summary = docker::stats_summary(&doc);
            if params.raw.unwrap_or(false) {
                summary["raw"] = docker::bound_raw(&doc);
            }
            let text = format!(
                "{id}: cpu {}%, memory {} / {} ({}%), net rx {} tx {}, block read {} write {}, pids {}",
                summary.get("cpu_percent").cloned().unwrap_or(Value::Null),
                str_field(&summary, "memory_usage_human"),
                str_field(&summary, "memory_limit_human"),
                summary.get("memory_percent").cloned().unwrap_or(Value::Null),
                summary.get("network_rx_bytes").cloned().unwrap_or(Value::Null),
                summary.get("network_tx_bytes").cloned().unwrap_or(Value::Null),
                summary.get("block_read_bytes").cloned().unwrap_or(Value::Null),
                summary.get("block_write_bytes").cloned().unwrap_or(Value::Null),
                summary.get("pids").cloned().unwrap_or(Value::Null),
            );
            Ok(shaped(summary, text))
        }
        .await)
    }

    #[tool(
        description = "Create and start a container (`docker run -d`): image with explicit tag, \
                       optional name, cmd, entrypoint, env, labels, working_dir, user, published \
                       ports (bound to 127.0.0.1 unless a host ip is given), restart policy, memory \
                       and CPU limits, platform; wait=true blocks for exit and returns the exit code \
                       and logs. No privileged/cap_add/binds/host-network by design; Tty is always \
                       false. Gated by DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.run_container", skip_all, fields(image = %docker::bounded(&params.image, 128)))]
    async fn run_container(
        &self,
        Parameters(params): Parameters<RunContainerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(async {
            let cfg = docker::config();
            require_writes(&cfg, "run_container")?;
            let client = client(&cfg)?;
            let image = docker::image_ref(&params.image)?;
            let name = params
                .name
                .as_deref()
                .map(|n| docker::container_ref(n, "name"))
                .transpose()?;
            let cmd = params
                .cmd
                .map(|c| docker::argv(c, "cmd"))
                .transpose()?;
            let entrypoint = params
                .entrypoint
                .map(|c| docker::argv(c, "entrypoint"))
                .transpose()?;
            let env = match params.env {
                Some(EnvSpec::List(list)) => Some(docker::env_entries(list)?),
                Some(EnvSpec::Map(map)) => Some(docker::env_entries(
                    map.into_iter().map(|(k, v)| format!("{k}={v}")).collect(),
                )?),
                None => None,
            };
            let labels = params.labels.map(docker::labels).transpose()?;
            if let Some(dir) = params.working_dir.as_deref() {
                if dir.is_empty() || dir.len() > 4096 || dir.contains('\0') {
                    return Err(Error::invalid("working_dir must be 1..4096 bytes without NUL"));
                }
            }
            if let Some(user) = params.user.as_deref() {
                if user.is_empty()
                    || user.len() > 256
                    || !user
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
                {
                    return Err(Error::invalid(
                        "user must be a user name or uid, optionally :group (letters, digits, _ - . :)",
                    ));
                }
            }
            let platform = params
                .platform
                .as_deref()
                .map(docker::platform)
                .transpose()?;
            if let Some(memory) = params.memory_bytes {
                if memory < MIN_MEMORY_BYTES {
                    return Err(Error::invalid(format!(
                        "memory_bytes must be at least {MIN_MEMORY_BYTES} (6 MiB)"
                    )));
                }
            }
            let nano_cpus = match params.cpus {
                Some(cpus) => {
                    if !cpus.is_finite() || cpus <= 0.0 || cpus > 1024.0 {
                        return Err(Error::invalid("cpus must be a number in (0, 1024]"));
                    }
                    Some((cpus * 1_000_000_000.0).round() as u64)
                }
                None => None,
            };
            let ports = params.ports.unwrap_or_default();
            if ports.len() > MAX_PORTS {
                return Err(Error::invalid(format!(
                    "ports may carry at most {MAX_PORTS} entries"
                )));
            }
            let mut exposed = serde_json::Map::new();
            let mut bindings = serde_json::Map::new();
            for spec in &ports {
                let port = docker::parse_port(spec)?;
                let key = port.key();
                exposed.insert(key.clone(), json!({}));
                let entry = bindings
                    .entry(key)
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Some(list) = entry.as_array_mut() {
                    list.push(json!({"HostIp": port.host_ip, "HostPort": port.host_port}));
                }
            }

            let mut body = json!({
                "Image": image,
                "Tty": false,
                "AttachStdin": false,
                "AttachStdout": false,
                "AttachStderr": false,
                "OpenStdin": false,
                "HostConfig": {
                    "RestartPolicy": {"Name": params.restart_policy.unwrap_or(RestartPolicy::No).name()},
                    "AutoRemove": false,
                },
            });
            if let Some(cmd) = cmd {
                body["Cmd"] = json!(cmd);
            }
            if let Some(entrypoint) = entrypoint {
                body["Entrypoint"] = json!(entrypoint);
            }
            if let Some(env) = env {
                body["Env"] = json!(env);
            }
            if let Some(labels) = labels {
                body["Labels"] = json!(labels);
            }
            if let Some(dir) = params.working_dir.as_deref() {
                body["WorkingDir"] = json!(dir);
            }
            if let Some(user) = params.user.as_deref() {
                body["User"] = json!(user);
            }
            if !exposed.is_empty() {
                body["ExposedPorts"] = Value::Object(exposed);
                body["HostConfig"]["PortBindings"] = Value::Object(bindings);
            }
            if let Some(memory) = params.memory_bytes {
                body["HostConfig"]["Memory"] = json!(memory);
            }
            if let Some(nano) = nano_cpus {
                body["HostConfig"]["NanoCpus"] = json!(nano);
            }

            let mut query = Query::new();
            query.push_opt("name", name.as_deref());
            query.push_opt("platform", platform.as_deref());
            let create_path = format!("/containers/create{}", query.render());

            let mut pulled: Option<Value> = None;
            let created = match client.post(&create_path, Some(&body)).await {
                Ok(reply) => reply,
                Err(Error::Api {
                    status: 404,
                    message,
                    ..
                }) if params.pull_if_missing.unwrap_or(false) => {
                    tracing::info!(image = %image, "image missing, pulling before create");
                    let summary = pull(&client, &image, platform.as_deref(), false).await?;
                    pulled = Some(json!({
                        "reason": message,
                        "digest": summary.get("digest").cloned().unwrap_or(Value::Null),
                        "layers": summary.get("layers").and_then(Value::as_object).map(|m| m.len()).unwrap_or(0),
                    }));
                    client.post(&create_path, Some(&body)).await?
                }
                Err(err) => return Err(err),
            };
            let created_doc = json_body(&created, &create_path)?;
            let id = created_doc
                .get("Id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::Malformed {
                    path: create_path.clone(),
                    detail: "create answered without an Id".to_owned(),
                })?;
            let warnings = created_doc
                .get("Warnings")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let short = docker::short_id(&id);
            let start_path = format!("/containers/{id}/start");
            let start = client.post(&start_path, None).await?;

            let mut out = json!({
                "id": short,
                "full_id": id,
                "name": name,
                "image": image,
                "warnings": warnings,
                "started": start.status != 304,
                "already_running": start.status == 304,
                "ports": ports,
                "pulled": pulled,
            });
            let mut text = format!(
                "started {} ({short}) from {image}{}",
                name.as_deref().unwrap_or("<daemon-named>"),
                if start.status == 304 { " (was already running)" } else { "" }
            );
            if params.wait.unwrap_or(false) {
                let wait_path = format!("/containers/{id}/wait?condition=not-running");
                match client.post(&wait_path, None).await {
                    Ok(reply) => {
                        let doc = json_body(&reply, &wait_path)?;
                        let exit_code = doc.get("StatusCode").cloned().unwrap_or(Value::Null);
                        let logs = fetch_logs(&client, &id, false, WAIT_LOG_TAIL, DEFAULT_LOG_BYTES as usize, &Query::new()).await?;
                        text.push_str(&format!(
                            "; exited with {exit_code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                            logs.get("stdout").and_then(Value::as_str).unwrap_or(""),
                            logs.get("stderr").and_then(Value::as_str).unwrap_or(""),
                        ));
                        out["exited"] = json!(true);
                        out["exit_code"] = exit_code;
                        out["wait_error"] = doc.pointer("/Error/Message").cloned().unwrap_or(Value::Null);
                        out["stdout"] = logs.get("stdout").cloned().unwrap_or(Value::Null);
                        out["stderr"] = logs.get("stderr").cloned().unwrap_or(Value::Null);
                        out["logs_truncated"] = logs.get("truncated").cloned().unwrap_or(Value::Null);
                    }
                    Err(Error::Transport { detail, .. }) if detail.contains("timed out") => {
                        out["exited"] = json!(false);
                        out["note"] = json!(format!(
                            "the container was still running when the outbound deadline \
                             elapsed ({detail}); it keeps running — use container_logs / \
                             inspect_container / stop_container on {short}"
                        ));
                        text.push_str("; still running after the wait deadline");
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok(shaped(out, text))
        }
        .await)
    }

    #[tool(
        description = "Start a created or exited container. HTTP 304 (already running) is \
                       reported as changed=false; podman restarts an exited container on start. \
                       Gated by DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.start_container", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn start_container(
        &self,
        Parameters(params): Parameters<ContainerIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(async {
            let cfg = docker::config();
            require_writes(&cfg, "start_container")?;
            let client = client(&cfg)?;
            let id = docker::container_ref(&params.id, "id")?;
            let path = format!("/containers/{id}/start");
            let reply = client.post(&path, None).await?;
            let changed = reply.status != 304;
            Ok(shaped(
                json!({"id": id, "changed": changed, "note": if changed { "started" } else { "already running" }}),
                if changed { format!("{id}: started") } else { format!("{id}: already running") },
            ))
        }
        .await)
    }

    #[tool(
        description = "Gracefully stop a container: the stop signal, then SIGKILL after `timeout` \
                       seconds (0..=300, default 10). HTTP 304 (already stopped) is changed=false. \
                       Gated by DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.stop_container", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn stop_container(
        &self,
        Parameters(params): Parameters<StopContainerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(async {
            let cfg = docker::config();
            require_writes(&cfg, "stop_container")?;
            let client = client(&cfg)?;
            let id = docker::container_ref(&params.id, "id")?;
            let (timeout, capped) = stop_timeout(&cfg, params.timeout);
            let signal = params
                .signal
                .as_deref()
                .map(docker::signal)
                .transpose()?;
            let mut query = Query::new();
            query.push("t", timeout);
            query.push_opt("signal", signal.as_deref());
            let path = format!("/containers/{id}/stop{}", query.render());
            let reply = client.post(&path, None).await?;
            let changed = reply.status != 304;
            let mut out = json!({"id": id, "changed": changed, "timeout": timeout, "note": if changed { "stopped" } else { "already stopped" }});
            if let Some(cap) = capped {
                out["timeout_note"] = json!(cap);
            }
            Ok(shaped(
                out,
                if changed { format!("{id}: stopped (t={timeout})") } else { format!("{id}: already stopped") },
            ))
        }
        .await)
    }

    #[tool(
        description = "Restart a container with the same grace timeout semantics as stop_container \
                       (0..=300 s, default 10). Gated by DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.restart_container", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn restart_container(
        &self,
        Parameters(params): Parameters<StopContainerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                require_writes(&cfg, "restart_container")?;
                let client = client(&cfg)?;
                let id = docker::container_ref(&params.id, "id")?;
                let (timeout, capped) = stop_timeout(&cfg, params.timeout);
                let signal = params.signal.as_deref().map(docker::signal).transpose()?;
                let mut query = Query::new();
                query.push("t", timeout);
                query.push_opt("signal", signal.as_deref());
                let path = format!("/containers/{id}/restart{}", query.render());
                client.post(&path, None).await?;
                let mut out =
                    json!({"id": id, "changed": true, "timeout": timeout, "note": "restarted"});
                if let Some(cap) = capped {
                    out["timeout_note"] = json!(cap);
                }
                Ok(shaped(out, format!("{id}: restarted (t={timeout})")))
            }
            .await,
        )
    }

    #[tool(
        description = "Send a signal (default SIGKILL) to a RUNNING container; a non-running \
                       container is a 409 error with a hint to use start/remove instead. Gated by \
                       DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.kill_container", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn kill_container(
        &self,
        Parameters(params): Parameters<KillContainerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                require_writes(&cfg, "kill_container")?;
                let client = client(&cfg)?;
                let id = docker::container_ref(&params.id, "id")?;
                let signal = docker::signal(params.signal.as_deref().unwrap_or("SIGKILL"))?;
                let mut query = Query::new();
                query.push("signal", &signal);
                let path = format!("/containers/{id}/kill{}", query.render());
                client.post(&path, None).await?;
                Ok(shaped(
                    json!({"id": id, "signal": signal, "changed": true}),
                    format!("{id}: sent {signal}"),
                ))
            }
            .await,
        )
    }

    #[tool(
        description = "Remove a container; force=true kills a running one first, volumes=true also \
                       removes its anonymous volumes. A running container without force is a clear \
                       error (Docker 409 / podman 500). Gated by DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.remove_container", skip_all, fields(id = %docker::bounded(&params.id, 128)))]
    async fn remove_container(
        &self,
        Parameters(params): Parameters<RemoveContainerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                require_writes(&cfg, "remove_container")?;
                let client = client(&cfg)?;
                let id = docker::container_ref(&params.id, "id")?;
                let force = params.force.unwrap_or(false);
                let volumes = params.volumes.unwrap_or(false);
                let mut query = Query::new();
                query.push("v", volumes).push("force", force);
                let path = format!("/containers/{id}{}", query.render());
                client.delete(&path).await?;
                Ok(shaped(
                    json!({"id": id, "removed": true, "force": force, "volumes": volumes}),
                    format!("{id}: removed{}", if force { " (forced)" } else { "" }),
                ))
            }
            .await,
        )
    }

    #[tool(
        description = "List local images: 12-char id, repo tags, created, size (bytes + human), \
                       container count, label count and dangling flag, newest first; Engine filters \
                       {\"reference\":[\"alpine:*\"],\"dangling\":[\"true\"],\"label\":[..]}; \
                       digests=true adds repo digests."
    )]
    #[tracing::instrument(name = "tool.list_images", skip_all)]
    async fn list_images(
        &self,
        Parameters(params): Parameters<ListImagesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(async {
            let cfg = docker::config();
            let client = client(&cfg)?;
            let limit = clamp(params.limit, DEFAULT_IMAGE_LIMIT, 1, MAX_IMAGE_LIMIT);
            let digests = params.digests.unwrap_or(false);
            let filters = docker::filters_json(params.filters.as_ref(), IMAGE_FILTERS, "list_images")?;
            let mut query = Query::new();
            query
                .push("all", params.all.unwrap_or(false))
                .push("digests", digests)
                .push("shared-size", "false");
            query.push_opt("filters", filters.as_deref());
            let path = format!("/images/json{}", query.render());
            let doc = client.get_json(&path).await?;
            let list = doc.as_array().ok_or_else(|| Error::Malformed {
                path: path.clone(),
                detail: "expected a JSON array of images".to_owned(),
            })?;
            let mut items: Vec<&Value> = list.iter().collect();
            items.sort_by(|a, b| {
                b.get("Created")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .cmp(&a.get("Created").and_then(Value::as_i64).unwrap_or(0))
            });
            let total = items.len();
            let rows: Vec<Value> = items
                .iter()
                .take(limit as usize)
                .map(|v| docker::image_row(v, digests))
                .collect();
            let mut text = format!("{} of {total} image(s):\n", rows.len());
            for row in &rows {
                text.push_str(&format!(
                    "{}  {}  {}  {}\n",
                    str_field(row, "id"),
                    row.get("repo_tags")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
                        .unwrap_or_default(),
                    str_field(row, "size_human"),
                    str_field(row, "created"),
                ));
            }
            Ok(shaped(
                json!({"count": rows.len(), "total": total, "limit": limit, "truncated": total > rows.len(), "images": rows}),
                text,
            ))
        }
        .await)
    }

    #[tool(
        description = "Inspect one image (name[:tag|@digest] or id): tags, digests, created, size, \
                       os/arch, Config (Cmd, Entrypoint, Env with secret-looking values redacted, \
                       ExposedPorts, Labels, WorkingDir, User, Volumes) and layer count; raw=true for \
                       the full document."
    )]
    #[tracing::instrument(name = "tool.inspect_image", skip_all, fields(name = %docker::bounded(&params.name, 128)))]
    async fn inspect_image(
        &self,
        Parameters(params): Parameters<InspectImageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let name = docker::image_ref(&params.name)?;
                let path = format!("/images/{name}/json");
                let doc = client.get_json(&path).await?;
                if params.raw.unwrap_or(false) {
                    let raw = docker::bound_raw(&doc);
                    let text = pretty(&raw);
                    return Ok(shaped(raw, text));
                }
                let detail = docker::image_detail(&doc, params.include_env_values.unwrap_or(false));
                let text = format!(
                    "{} ({}): {} {}/{}, {} layers, created {}\n{}",
                    name,
                    str_field(&detail, "id"),
                    str_field(&detail, "size_human"),
                    str_field(&detail, "os"),
                    str_field(&detail, "architecture"),
                    detail.get("rootfs_layers").cloned().unwrap_or(Value::Null),
                    str_field(&detail, "created"),
                    pretty(&detail),
                );
                Ok(shaped(detail, text))
            }
            .await,
        )
    }

    #[tool(
        description = "Pull an image synchronously (POST /images/create) and summarise the progress \
                       stream: per-layer status, digest, status lines; a missing tag defaults to \
                       `latest` and is reported. Sends X-Registry-Auth when DOCKER_REGISTRY_AUTH is \
                       set. Bounded by MCP_OUTBOUND_TIMEOUT_MS. Gated by DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.pull_image", skip_all, fields(image = %docker::bounded(&params.image, 128)))]
    async fn pull_image(
        &self,
        Parameters(params): Parameters<PullImageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                require_writes(&cfg, "pull_image")?;
                let client = client(&cfg)?;
                let platform = params
                    .platform
                    .as_deref()
                    .map(docker::platform)
                    .transpose()?;
                let summary = pull(
                    &client,
                    &params.image,
                    platform.as_deref(),
                    params.raw_progress.unwrap_or(false),
                )
                .await?;
                let layers = summary
                    .get("layers")
                    .and_then(Value::as_object)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let text = format!(
                    "pulled {}:{} ({layers} layer(s){}){}\n{}",
                    str_field(&summary, "repo"),
                    str_field(&summary, "tag"),
                    summary
                        .get("digest")
                        .and_then(Value::as_str)
                        .map(|d| format!(", digest {d}"))
                        .unwrap_or_default(),
                    summary
                        .get("note")
                        .and_then(Value::as_str)
                        .map(|n| format!(" — {n}"))
                        .unwrap_or_default(),
                    summary
                        .get("status")
                        .and_then(Value::as_array)
                        .map(|a| a
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n"))
                        .unwrap_or_default(),
                );
                Ok(shaped(summary, text))
            }
            .await,
        )
    }

    #[tool(
        description = "Remove (or only untag) an image; returns the daemon's Untagged/Deleted list. \
                       409 conflicts are mapped: stopped container / multiple tags -> pass force=true; \
                       running container -> stop it first. Gated by DOCKER_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.remove_image", skip_all, fields(name = %docker::bounded(&params.name, 128)))]
    async fn remove_image(
        &self,
        Parameters(params): Parameters<RemoveImageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(async {
            let cfg = docker::config();
            require_writes(&cfg, "remove_image")?;
            let client = client(&cfg)?;
            let name = docker::image_ref(&params.name)?;
            let force = params.force.unwrap_or(false);
            let noprune = params.noprune.unwrap_or(false);
            let mut query = Query::new();
            query.push("force", force).push("noprune", noprune);
            let path = format!("/images/{name}{}", query.render());
            let reply = client.delete(&path).await?;
            let doc = if reply.body.is_empty() {
                json!([])
            } else {
                json_body(&reply, &path)?
            };
            let list = doc.as_array().cloned().unwrap_or_default();
            let untagged: Vec<String> = list
                .iter()
                .filter_map(|e| e.get("Untagged").and_then(Value::as_str))
                .take(MAX_ROWS)
                .map(str::to_owned)
                .collect();
            let deleted: Vec<String> = list
                .iter()
                .filter_map(|e| e.get("Deleted").and_then(Value::as_str))
                .take(MAX_ROWS)
                .map(docker::short_id)
                .collect();
            Ok(shaped(
                json!({"name": name, "force": force, "untagged": untagged, "deleted": deleted, "changes": list.len()}),
                format!(
                    "{name}: untagged {} reference(s), deleted {} layer(s)",
                    untagged.len(),
                    deleted.len()
                ),
            ))
        }
        .await)
    }

    #[tool(
        description = "List networks: 12-char id, name, driver, scope, internal/attachable/ipv6 \
                       flags, IPAM subnets and gateways, container and label counts; Engine filters \
                       {\"driver\":[\"bridge\"],\"type\":[\"custom\"],\"name\":[..]}. Read-only (no \
                       create/remove/connect)."
    )]
    #[tracing::instrument(name = "tool.list_networks", skip_all)]
    async fn list_networks(
        &self,
        Parameters(params): Parameters<ListNetworksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let filters = docker::filters_json(
                    params.filters.as_ref(),
                    NETWORK_FILTERS,
                    "list_networks",
                )?;
                let mut query = Query::new();
                query.push_opt("filters", filters.as_deref());
                let path = format!("/networks{}", query.render());
                let doc = client.get_json(&path).await?;
                let list = doc.as_array().ok_or_else(|| Error::Malformed {
                    path: path.clone(),
                    detail: "expected a JSON array of networks".to_owned(),
                })?;
                let rows: Vec<Value> = list
                    .iter()
                    .take(MAX_ROWS)
                    .map(docker::network_row)
                    .collect();
                let mut text = format!("{} network(s):\n", rows.len());
                for row in &rows {
                    text.push_str(&format!(
                        "{}  {}  {}  {}  {}\n",
                        str_field(row, "id"),
                        str_field(row, "name"),
                        str_field(row, "driver"),
                        str_field(row, "scope"),
                        row.get("ipam")
                            .and_then(Value::as_array)
                            .map(|a| a
                                .iter()
                                .map(|c| str_field(c, "subnet"))
                                .collect::<Vec<_>>()
                                .join(","))
                            .unwrap_or_default(),
                    ));
                }
                Ok(shaped(json!({"count": rows.len(), "networks": rows}), text))
            }
            .await,
        )
    }

    #[tool(
        description = "List volumes: name, driver, mountpoint, scope, created, label count, \
                       options and any daemon Warnings; Engine filters {\"dangling\":[\"true\"],\
                       \"driver\":[..],\"label\":[..],\"name\":[..]}."
    )]
    #[tracing::instrument(name = "tool.list_volumes", skip_all)]
    async fn list_volumes(
        &self,
        Parameters(params): Parameters<ListVolumesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let filters =
                    docker::filters_json(params.filters.as_ref(), VOLUME_FILTERS, "list_volumes")?;
                let mut query = Query::new();
                query.push_opt("filters", filters.as_deref());
                let path = format!("/volumes{}", query.render());
                let doc = client.get_json(&path).await?;
                let list = doc
                    .get("Volumes")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let warnings = doc.get("Warnings").cloned().unwrap_or_else(|| json!([]));
                let rows: Vec<Value> = list.iter().take(MAX_ROWS).map(docker::volume_row).collect();
                let mut text = format!("{} volume(s):\n", rows.len());
                for row in &rows {
                    text.push_str(&format!(
                        "{}  {}  {}\n",
                        str_field(row, "name"),
                        str_field(row, "driver"),
                        str_field(row, "mountpoint"),
                    ));
                }
                Ok(shaped(
                    json!({"count": rows.len(), "volumes": rows, "warnings": warnings}),
                    text,
                ))
            }
            .await,
        )
    }

    #[tool(
        description = "Disk usage like `docker system df`: per category (images, containers, \
                       volumes, build cache) total/active counts, size and reclaimable bytes computed \
                       from GET /system/df, plus layers size; detail=true lists the largest items."
    )]
    #[tracing::instrument(name = "tool.system_df", skip_all)]
    async fn system_df(
        &self,
        Parameters(params): Parameters<SystemDfParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            async {
                let cfg = docker::config();
                let client = client(&cfg)?;
                let doc = client.get_json("/system/df").await?;
                let summary = docker::df_summary(&doc, params.detail.unwrap_or(false));
                let line = |cat: &str| {
                    let c = summary.get(cat).cloned().unwrap_or(Value::Null);
                    format!(
                        "{cat}: {} total, {} active, {} ({} reclaimable)",
                        c.get("total").cloned().unwrap_or(Value::Null),
                        c.get("active").cloned().unwrap_or(Value::Null),
                        str_field(&c, "size_human"),
                        str_field(&c, "reclaimable_human"),
                    )
                };
                let text = format!(
                    "{}\n{}\n{}\n{}\nlayers: {}",
                    line("images"),
                    line("containers"),
                    line("volumes"),
                    line("build_cache"),
                    str_field(&summary, "layers_size_human"),
                );
                Ok(shaped(summary, text))
            }
            .await,
        )
    }
}

impl DockerServer {
    async fn version_inner(&self) -> Result<CallToolResult, Error> {
        let cfg = docker::config();
        let client = client(&cfg)?;
        let (doc, window_error) = match client.get_json("/version").await {
            Ok(doc) => (doc, None),
            Err(Error::VersionWindow { message, .. }) => {
                // The versioned path was refused: ask the unversioned route
                // (always served) so the caller still learns the daemon's
                // window and gets the fix.
                let reply = client
                    .send_raw(http::Method::GET, "/version", &[], Bytes::new())
                    .await?;
                let reply = Client::ok(reply, "/version")?;
                let mut doc = json_body(&reply, "/version")?;
                let ping = client
                    .send_raw(http::Method::GET, "/_ping", &[], Bytes::new())
                    .await
                    .ok()
                    .and_then(|r| r.header("api-version"));
                if let Some(api) = ping {
                    if doc.get("ApiVersion").and_then(Value::as_str).is_none() {
                        doc["ApiVersion"] = json!(api);
                    }
                }
                (doc, Some(message))
            }
            Err(err) => return Err(err),
        };
        let mut summary = docker::version_summary(&doc, client.api_version());
        summary["docker_host"] = json!(client.base());
        summary["read_only"] = json!(cfg.read_only);
        summary["registry_auth_configured"] = json!(cfg.registry_auth.is_some());
        // Shape-check the optional registry credential here so a placeholder
        // or garbage secret surfaces on the designated first call instead of
        // on the first private pull. Never the value, only the verdict.
        if cfg.registry_auth.is_some() {
            match client.registry_auth_header() {
                Ok(_) => summary["registry_auth_valid"] = json!(true),
                Err(err) => {
                    summary["registry_auth_valid"] = json!(false);
                    summary["registry_auth_error"] = json!(err.message());
                }
            }
        }
        let ok = summary
            .get("api_version_ok")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(message) = window_error {
            summary["api_version_ok"] = json!(false);
            summary["error"] = json!(message);
        }
        if !ok || summary.get("error").is_some() {
            summary["hint"] = json!(format!(
                "set {} to a value between MinAPIVersion {} and ApiVersion {} (e.g. v{}) in \
                 deploy/workload.yaml and re-apply; podman ignores the prefix, Docker does not",
                docker::API_VERSION_ENV,
                str_field(&summary, "min_api_version"),
                str_field(&summary, "api_version"),
                str_field(&summary, "api_version"),
            ));
        }
        let text = format!(
            "{} {} (API {}, min {}), {} {} {}, configured {} -> {}{}{}",
            str_field(&summary, "engine"),
            str_field(&summary, "version"),
            str_field(&summary, "api_version"),
            str_field(&summary, "min_api_version"),
            str_field(&summary, "os"),
            str_field(&summary, "arch"),
            str_field(&summary, "kernel_version"),
            str_field(&summary, "configured_api_version"),
            if summary
                .get("api_version_ok")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "ok"
            } else {
                "OUTSIDE the daemon's window"
            },
            summary
                .get("hint")
                .and_then(Value::as_str)
                .map(|h| format!("; {h}"))
                .unwrap_or_default(),
            match summary.get("registry_auth_valid").and_then(Value::as_bool) {
                Some(true) => "; registry credential: configured and well-formed".to_owned(),
                Some(false) => format!(
                    "; registry credential: INVALID — {}",
                    str_field(&summary, "registry_auth_error")
                ),
                None => "; registry credential: not configured (public pulls only)".to_owned(),
            },
        );
        Ok(shaped(summary, text))
    }
}

/// Stop/restart grace period: clamped to 0..=300 and to what fits inside the
/// outbound deadline (the daemon holds the request open for `t` seconds).
fn stop_timeout(cfg: &Config, requested: Option<i64>) -> (i64, Option<String>) {
    let clamped = clamp(requested, DEFAULT_STOP_TIMEOUT, 0, MAX_STOP_TIMEOUT);
    let budget = (cfg.outbound_timeout_ms / 1000).saturating_sub(5) as i64;
    if clamped > budget {
        let budget = budget.max(0);
        (
            budget,
            Some(format!(
                "timeout {clamped} exceeds the outbound deadline budget ({budget} s = \
                 MCP_OUTBOUND_TIMEOUT_MS/1000 - 5); {budget} was sent"
            )),
        )
    } else {
        (clamped, None)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DockerServer {
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
            "docker-mcp: the Docker Engine / podman REST API (v1.44 by default) as a sandboxed \
             WebAssembly component on Cosmonic Desktop. Call `version` first — it proves the \
             daemon is reachable (through the host.wasmcloud.internal:2375 loopback grant) and \
             that DOCKER_API_VERSION fits the daemon's window. Read tools: info, \
             list_containers, inspect_container, container_logs (demultiplexes stdout/stderr), \
             container_stats, list_images, inspect_image, list_networks, list_volumes, \
             system_df. Write tools (run/start/stop/restart/kill/remove_container, pull_image, \
             remove_image) are listed but refuse until DOCKER_READ_ONLY=false is set in the \
             workload. Filters are objects of string arrays ({\"status\":[\"running\"]}); \
             ids are the 12-char short ids from the listings; Env values that look like \
             secrets are redacted unless include_env_values=true.\n\n\
             This server publishes skills — playbooks describing when and how to use its \
             tools. Read `skill://index.json` for the catalog, then \
             `skill://docker-mcp/SKILL.md` for the operating knowledge, the podman/Docker \
             differences and the error catalogue.",
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
