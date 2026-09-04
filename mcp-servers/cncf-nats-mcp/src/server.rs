//! The NATS MCP server: tools over `wasmcloud:nats@0.1.0`.
//!
//! Three families, matching the capability's three interfaces:
//!
//! - **Core** (`nats_publish`, `nats_request`) — fire-and-forget and RPC. No
//!   durability.
//! - **JetStream** (`jetstream_*`) — durable publish, non-destructive reads
//!   (`jetstream_scan`, `jetstream_get_message`), introspection, and
//!   consumer-driven `jetstream_fetch`.
//! - **KV** (`kv_*`) — get/put/create/CAS/delete/purge/keys/history/status on
//!   a JetStream key-value bucket.
//!
//! Two boundaries shape this surface, and both are deliberate:
//!
//! 1. **No lifecycle.** Creating or deleting streams, consumers, and buckets
//!    is absent from `wasmcloud:nats` rather than exported-and-denied.
//!    Provision them out-of-band (the `nats` CLI, deployment tooling); these
//!    tools read and write what already exists.
//! 2. **Deny-by-default grants.** Subjects, streams, and buckets are checked
//!    host-side against the workload's grants. A `denied` failure is an
//!    operator change — see [`crate::nats`].

use std::collections::BTreeMap;

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
use serde::{Deserialize, Serialize};

use crate::nats::{self, Failure, Settle};
use crate::{diagnose, inventory, monitor, skills};

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so no per-session state
/// lives here. Durable state belongs in NATS KV, which is what `kv_*` is for.
#[derive(Clone)]
pub struct NatsServer {
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

/// Renders a successful operation as `structuredContent`.
fn ok<T: Serialize>(value: T) -> Result<CallToolResult, ErrorData> {
    serde_json::to_value(value)
        .map(CallToolResult::structured)
        .map_err(|err| ErrorData::internal_error(err.to_string(), None))
}

/// Renders a NATS failure as a **tool** error rather than a protocol error:
/// the caller asked a reasonable question and deserves the reason (and the
/// fix) as an answer it can act on.
fn failed(err: Failure) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        err.to_string(),
    )]))
}

/// `Ok`-only operations still return something an agent can read back.
fn acknowledged(action: &str) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::structured(
        serde_json::json!({ "ok": true, "action": action }),
    ))
}

fn default_batch() -> u32 {
    10
}

fn default_max_count() -> u32 {
    10
}

fn all_subjects() -> String {
    ">".to_string()
}

/// Rounds a rate to two decimals — enough precision to act on, few enough
/// digits that a reader is not misled about how exact a 2-second sample is.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn humanize(seconds: f64) -> String {
    diagnose::humanize_secs(seconds)
}

// Total accessors for monitoring JSON. The monitoring endpoints' shape varies
// across NATS versions, so a missing field must degrade to a zero or an empty
// string rather than fail the whole tool.
fn field_str(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn field_u64(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

fn field_i64(value: &serde_json::Value, key: &str) -> i64 {
    value.get(key).and_then(serde_json::Value::as_i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// A message body. `payload` and `payload_base64` are mutually exclusive;
/// omitting both sends an empty body, which NATS allows.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishParams {
    /// Literal subject to publish on. Wildcards (`*`, `>`) are rejected —
    /// they are only valid in subscription patterns.
    pub subject: String,
    /// Message body as UTF-8 text.
    pub payload: Option<String>,
    /// Message body as base64, for bodies that are not valid UTF-8.
    pub payload_base64: Option<String>,
    /// Message headers. Names must be printable ASCII without `:`; values
    /// must not contain CR or LF.
    pub headers: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RequestParams {
    /// Literal subject to send the request on.
    pub subject: String,
    /// Request body as UTF-8 text.
    pub payload: Option<String>,
    /// Request body as base64.
    pub payload_base64: Option<String>,
    /// Request headers.
    pub headers: Option<BTreeMap<String, String>>,
    /// How long to wait for a reply, in milliseconds (default 5000, max
    /// 30000). The reply subject is always the host's per-workload inbox.
    pub timeout_ms: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JetStreamPublishParams {
    /// Subject to publish on. It must be captured by an existing stream, or
    /// the publish fails with no stream ack.
    pub subject: String,
    /// Message body as UTF-8 text.
    pub payload: Option<String>,
    /// Message body as base64.
    pub payload_base64: Option<String>,
    /// Message headers.
    pub headers: Option<BTreeMap<String, String>>,
    /// Idempotency key, sent as the `Nats-Msg-Id` header. A repeat within the
    /// stream's duplicate window is acked with `duplicate: true` and stored
    /// once. The window is stream configuration, not a guarantee of this tool.
    pub msg_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetMessageParams {
    /// Stream to read from.
    pub stream: String,
    /// Stream sequence number of the message.
    pub sequence: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScanParams {
    /// Stream to replay.
    pub stream: String,
    /// Sequence to start from (default 1, the beginning of the stream). Use
    /// `jetstream_stream_info` for the live first/last sequence.
    pub start_sequence: Option<u64>,
    /// Maximum messages to return (default 10). Large bodies are truncated
    /// per-message, so raise this deliberately.
    #[serde(default = "default_max_count")]
    pub max_count: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StreamParams {
    /// Stream name.
    pub stream: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSubjectsParams {
    /// Stream to inspect.
    pub stream: String,
    /// Subject filter; `>` (the default) matches every subject. At most 1000
    /// subjects come back, so narrow this to page through a wide stream.
    #[serde(default = "all_subjects")]
    pub filter: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConsumerParams {
    /// Stream the consumer belongs to.
    pub stream: String,
    /// Consumer name.
    pub consumer: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchParams {
    /// Stream the consumer belongs to.
    pub stream: String,
    /// Name of an existing **pull** consumer. Provision it out-of-band.
    pub consumer: String,
    /// Maximum messages to pull (default 10). Above the consumer's
    /// `max_request_batch` the fetch is refused outright, not truncated.
    #[serde(default = "default_batch")]
    pub batch: u32,
    /// Optional byte bound for the batch. Counts subject, reply subject, and
    /// payload (~63 bytes of overhead per small message).
    pub max_bytes: Option<u64>,
    /// How long to wait for the batch, in milliseconds (default 5000, max
    /// 30000).
    pub timeout_ms: Option<u32>,
    /// What to do with each delivered message. Default `none` leaves them
    /// unsettled — nothing is consumed, but the consumer stalls for its full
    /// `ack_wait` before redelivering. Use `ack` to consume them for good,
    /// `nak` for immediate redelivery, `term` to never redeliver.
    #[serde(default)]
    pub settle: Settle,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyParams {
    /// KV bucket name.
    pub bucket: String,
    /// Key within the bucket.
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvWriteParams {
    /// KV bucket name.
    pub bucket: String,
    /// Key to write.
    pub key: String,
    /// Value as UTF-8 text.
    pub value: Option<String>,
    /// Value as base64, for values that are not valid UTF-8.
    pub value_base64: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvUpdateParams {
    /// KV bucket name.
    pub bucket: String,
    /// Key to update.
    pub key: String,
    /// New value as UTF-8 text.
    pub value: Option<String>,
    /// New value as base64.
    pub value_base64: Option<String>,
    /// Revision the key must currently be at. On mismatch the tool reports
    /// the actual current revision so the retry needs no extra read.
    pub expected_revision: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeysParams {
    /// KV bucket name.
    pub bucket: String,
    /// Key filter; `>` (the default) matches every key. At most 1000 keys
    /// come back, with `truncated: true` when the bucket holds more.
    #[serde(default = "all_subjects")]
    pub filter: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BucketParams {
    /// KV bucket name.
    pub bucket: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiagnoseParams {
    /// Streams to examine. Leave empty to diagnose every stream on the server,
    /// which requires the monitoring endpoint.
    pub streams: Option<Vec<String>>,
    /// Consumers to examine, each as `"stream/consumer"`. Ignored when the
    /// whole server is being diagnosed — consumers are discovered there.
    pub consumers: Option<Vec<String>>,
    /// KV buckets to examine.
    pub buckets: Option<Vec<String>>,
    /// Gap between the two readings, in milliseconds (default 2000, max
    /// 15000). Rates and trends are measured across it, so a longer window
    /// gives a steadier number on bursty traffic.
    pub sample_ms: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StreamRateParams {
    /// Stream to measure.
    pub stream: String,
    /// Sampling window in milliseconds (default 2000, max 15000).
    pub sample_ms: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConsumerLagParams {
    /// Stream the consumer belongs to.
    pub stream: String,
    /// Consumer name.
    pub consumer: String,
    /// Sampling window in milliseconds (default 2000, max 15000).
    pub sample_ms: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServerStreamsParams {
    /// Include each stream's consumers (default true). Set false for a
    /// shorter listing when only the streams matter.
    pub include_consumers: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectionsParams {
    /// Maximum connections to return (default 64, capped at 512).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckAccessParams {
    /// Streams to probe. Leave both lists empty to discover and probe
    /// everything on the server (needs the monitoring endpoint).
    pub streams: Option<Vec<String>>,
    /// KV buckets to probe.
    pub buckets: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl NatsServer {
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

    // --- Core -------------------------------------------------------------

    #[tool(
        description = "Publish a message to a NATS subject (core NATS, fire-and-forget). \
                       Resolves once the message is written to the connection, not once \
                       any subscriber has seen it — there is no delivery confirmation. \
                       Use jetstream_publish when you need a durable, acked write."
    )]
    #[tracing::instrument(name = "tool.nats_publish", skip(self, params), fields(subject = %params.subject))]
    async fn nats_publish(
        &self,
        Parameters(params): Parameters<PublishParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let body = match nats::decode_body(params.payload, params.payload_base64) {
            Ok(body) => body,
            Err(err) => return failed(err),
        };
        match nats::publish(params.subject, body, params.headers).await {
            Ok(()) => acknowledged("published"),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Send a request on a NATS subject and wait for a single reply \
                       (core NATS request/reply). Fails fast with `no-responders` when \
                       nothing is listening, which is distinct from a timeout: retrying \
                       will fail identically until a responder appears."
    )]
    #[tracing::instrument(name = "tool.nats_request", skip(self, params), fields(subject = %params.subject))]
    async fn nats_request(
        &self,
        Parameters(params): Parameters<RequestParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let body = match nats::decode_body(params.payload, params.payload_base64) {
            Ok(body) => body,
            Err(err) => return failed(err),
        };
        match nats::request(params.subject, body, params.headers, params.timeout_ms).await {
            Ok(message) => ok(message),
            Err(err) => failed(err),
        }
    }

    // --- JetStream --------------------------------------------------------

    #[tool(
        description = "Publish a message to a JetStream-managed subject and wait for the \
                       stream acknowledgment (stream name + sequence). The subject must be \
                       captured by an existing stream. Pass msg_id for idempotency."
    )]
    #[tracing::instrument(name = "tool.jetstream_publish", skip(self, params), fields(subject = %params.subject))]
    async fn jetstream_publish(
        &self,
        Parameters(params): Parameters<JetStreamPublishParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let body = match nats::decode_body(params.payload, params.payload_base64) {
            Ok(body) => body,
            Err(err) => return failed(err),
        };
        // `msg_id` is surfaced as its own argument because callers should not
        // have to know the wire header that implements it.
        let mut headers = params.headers.unwrap_or_default();
        if let Some(msg_id) = params.msg_id {
            headers.insert("Nats-Msg-Id".to_string(), msg_id);
        }
        match nats::jetstream_publish(params.subject, body, Some(headers)).await {
            Ok(ack) => ok(ack),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Read one stored message from a stream by its sequence number. \
                       Non-destructive: nothing is consumed or acknowledged."
    )]
    #[tracing::instrument(name = "tool.jetstream_get_message", skip(self, params), fields(stream = %params.stream))]
    async fn jetstream_get_message(
        &self,
        Parameters(params): Parameters<GetMessageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::get_by_sequence(params.stream, params.sequence).await {
            Ok(message) => ok(message),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Replay messages from a stream in sequence order, without creating a \
                       consumer or acknowledging anything — the tool to reach for when \
                       inspecting stream contents. Sequence numbers may gap: messages on \
                       subjects outside this workload's grant are skipped and do not count \
                       against max_count."
    )]
    #[tracing::instrument(name = "tool.jetstream_scan", skip(self, params), fields(stream = %params.stream))]
    async fn jetstream_scan(
        &self,
        Parameters(params): Parameters<ScanParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = params.start_sequence.unwrap_or(1);
        match nats::scan(params.stream, start, params.max_count).await {
            Ok(messages) => ok(serde_json::json!({
                "count": messages.len(),
                "messages": messages,
            })),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Stream configuration and state: captured subjects, message and byte \
                       counts, first/last sequence, and consumer count."
    )]
    #[tracing::instrument(name = "tool.jetstream_stream_info", skip(self, params), fields(stream = %params.stream))]
    async fn jetstream_stream_info(
        &self,
        Parameters(params): Parameters<StreamParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::stream_info(params.stream).await {
            Ok(info) => ok(info),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "List the subjects a stream currently holds messages on, with a \
                       message count each. Unlike stream_info's `subjects` (what the stream \
                       is configured to capture) this reports what it actually holds. \
                       Requires NATS server 2.7.2 or newer."
    )]
    #[tracing::instrument(name = "tool.jetstream_list_subjects", skip(self, params), fields(stream = %params.stream))]
    async fn jetstream_list_subjects(
        &self,
        Parameters(params): Parameters<ListSubjectsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::list_stream_subjects(params.stream, params.filter).await {
            Ok(subjects) => ok(serde_json::json!({
                "count": subjects.len(),
                "subjects": subjects,
            })),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Consumer configuration and state without attaching to it: filters, \
                       provisioned limits (max_request_batch, max_request_max_bytes, \
                       max_ack_pending), and live counters (num_pending, num_ack_pending, \
                       num_redelivered). Check this before sizing a jetstream_fetch."
    )]
    #[tracing::instrument(name = "tool.jetstream_consumer_info", skip(self, params), fields(stream = %params.stream, consumer = %params.consumer))]
    async fn jetstream_consumer_info(
        &self,
        Parameters(params): Parameters<ConsumerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::consumer_info(params.stream, params.consumer).await {
            Ok(info) => ok(info),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Fetch a batch of messages from an existing pull consumer and settle \
                       them. This drives real consumer state: `settle: \"ack\"` consumes the \
                       messages permanently. The default `none` consumes nothing but stalls \
                       the consumer for its ack_wait before redelivery — to browse a stream \
                       without touching consumer state, use jetstream_scan instead."
    )]
    #[tracing::instrument(name = "tool.jetstream_fetch", skip(self, params), fields(stream = %params.stream, consumer = %params.consumer))]
    async fn jetstream_fetch(
        &self,
        Parameters(params): Parameters<FetchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::fetch(
            params.stream,
            params.consumer,
            params.batch,
            params.max_bytes,
            params.timeout_ms,
            params.settle,
        )
        .await
        {
            Ok(batch) => ok(serde_json::json!({
                "count": batch.messages.len(),
                "stop": batch.stop,
                "messages": batch.messages,
            })),
            Err(err) => failed(err),
        }
    }

    // --- Key/Value --------------------------------------------------------

    #[tool(
        description = "Read the latest value for a key in a NATS KV bucket, with its \
                       revision. Fails with `key-not-found` when the key is absent, \
                       deleted, or purged."
    )]
    #[tracing::instrument(name = "tool.kv_get", skip(self, params), fields(bucket = %params.bucket, key = %params.key))]
    async fn kv_get(
        &self,
        Parameters(params): Parameters<KeyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::kv_get(params.bucket, params.key).await {
            Ok(entry) => ok(entry),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Write a key in a NATS KV bucket, last-write-wins, returning the new \
                       revision. Use kv_update for a compare-and-swap that will not \
                       clobber a concurrent write."
    )]
    #[tracing::instrument(name = "tool.kv_put", skip(self, params), fields(bucket = %params.bucket, key = %params.key))]
    async fn kv_put(
        &self,
        Parameters(params): Parameters<KvWriteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let value = match nats::decode_body(params.value, params.value_base64) {
            Ok(value) => value,
            Err(err) => return failed(err),
        };
        match nats::kv_put(params.bucket, params.key, value).await {
            Ok(revision) => ok(serde_json::json!({ "revision": revision })),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Write a key only if it does not already exist, returning the new \
                       revision. Fails rather than overwriting if the key is present."
    )]
    #[tracing::instrument(name = "tool.kv_create", skip(self, params), fields(bucket = %params.bucket, key = %params.key))]
    async fn kv_create(
        &self,
        Parameters(params): Parameters<KvWriteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let value = match nats::decode_body(params.value, params.value_base64) {
            Ok(value) => value,
            Err(err) => return failed(err),
        };
        match nats::kv_create(params.bucket, params.key, value).await {
            Ok(revision) => ok(serde_json::json!({ "revision": revision })),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Compare-and-swap a key: write only if it is still at \
                       expected_revision. On conflict the failure carries the actual \
                       current revision, so a retry needs no intervening read. This is the \
                       safe way to do read-modify-write against concurrent writers."
    )]
    #[tracing::instrument(name = "tool.kv_update", skip(self, params), fields(bucket = %params.bucket, key = %params.key))]
    async fn kv_update(
        &self,
        Parameters(params): Parameters<KvUpdateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let value = match nats::decode_body(params.value, params.value_base64) {
            Ok(value) => value,
            Err(err) => return failed(err),
        };
        match nats::kv_update(params.bucket, params.key, value, params.expected_revision).await {
            Ok(revision) => ok(serde_json::json!({ "revision": revision })),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Delete a key from a KV bucket. Leaves a tombstone and keeps the \
                       key's history — use kv_purge to remove the history too."
    )]
    #[tracing::instrument(name = "tool.kv_delete", skip(self, params), fields(bucket = %params.bucket, key = %params.key))]
    async fn kv_delete(
        &self,
        Parameters(params): Parameters<KeyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::kv_delete(params.bucket, params.key).await {
            Ok(()) => acknowledged("deleted"),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Purge a key from a KV bucket, removing all of its history as well \
                       as its current value. Irreversible."
    )]
    #[tracing::instrument(name = "tool.kv_purge", skip(self, params), fields(bucket = %params.bucket, key = %params.key))]
    async fn kv_purge(
        &self,
        Parameters(params): Parameters<KeyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::kv_purge(params.bucket, params.key).await {
            Ok(()) => acknowledged("purged"),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "List keys in a KV bucket matching a filter (`>` for all). Capped at \
                       1000 keys; `truncated: true` means the bucket holds more, so narrow \
                       the filter rather than treating the page as complete."
    )]
    #[tracing::instrument(name = "tool.kv_keys", skip(self, params), fields(bucket = %params.bucket))]
    async fn kv_keys(
        &self,
        Parameters(params): Parameters<KeysParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::kv_keys(params.bucket, params.filter).await {
            Ok(page) => ok(serde_json::json!({
                "count": page.keys.len(),
                "keys": page.keys,
                "truncated": page.truncated,
            })),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Every retained revision of a key, oldest first, including delete and \
                       purge tombstones. How many revisions survive is the bucket's \
                       `history` setting (see kv_status)."
    )]
    #[tracing::instrument(name = "tool.kv_history", skip(self, params), fields(bucket = %params.bucket, key = %params.key))]
    async fn kv_history(
        &self,
        Parameters(params): Parameters<KeyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::kv_history(params.bucket, params.key).await {
            Ok(entries) => ok(serde_json::json!({
                "count": entries.len(),
                "entries": entries,
            })),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "KV bucket status: stored values (live plus retained history), \
                       revisions kept per key, TTL, and size in bytes."
    )]
    #[tracing::instrument(name = "tool.kv_status", skip(self, params), fields(bucket = %params.bucket))]
    async fn kv_status(
        &self,
        Parameters(params): Parameters<BucketParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match nats::kv_status(params.bucket).await {
            Ok(status) => ok(status),
            Err(err) => failed(err),
        }
    }

    // --- Diagnostics ------------------------------------------------------

    #[tool(
        description = "Diagnose this NATS deployment: sample it twice, apply rules, and \
                       return findings with evidence and a remedy each — not raw numbers. \
                       Catches unbounded streams that grow until the disk fills, consumers \
                       falling behind, ack saturation, redelivery loops, slow-consumer \
                       drops, and JetStream storage pressure. With server-wide monitoring \
                       enabled it examines every stream on the server and needs no NATS \
                       grants; without it, pass the streams and consumers to look at and \
                       the retention rules are skipped. Start here when something is wrong \
                       and you do not yet know what."
    )]
    #[tracing::instrument(name = "tool.nats_diagnose", skip(self, params))]
    async fn nats_diagnose(
        &self,
        Parameters(params): Parameters<DiagnoseParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let sample_ms = diagnose::clamp_sample_ms(params.sample_ms);
        let streams = params.streams.unwrap_or_default();
        let consumers = inventory::parse_consumer_refs(&params.consumers.unwrap_or_default());
        let buckets = params.buckets.unwrap_or_default();

        // Nothing named: work at the best level this deployment supports.
        // Monitoring sees the whole server and unlocks the retention rules;
        // hints scope the binding path to what the operator declared.
        if streams.is_empty() && consumers.is_empty() && buckets.is_empty() {
            let mut monitor_state = inventory::MonitorState::Unconfigured;
            if monitor::is_enabled() {
                match diagnose::via_monitoring(sample_ms).await {
                    Ok(report) => return ok(report),
                    // Configured but down (NATS restarted without `-m`, port
                    // closed). Degrade to the lower levels instead of failing
                    // the call — a diagnostic tool that goes dark the moment
                    // one input goes dark is the opposite of useful.
                    Err(err) if err.code.starts_with("monitor-") => {
                        tracing::warn!(code = err.code, "monitoring unreachable; falling back");
                        monitor_state = inventory::MonitorState::Unreachable;
                    }
                    Err(err) => return failed(err),
                }
            }
            let mut hinted = inventory::hinted();
            hinted.monitor = monitor_state;
            if hinted.is_empty() {
                let lead = if monitor_state == inventory::MonitorState::Unreachable {
                    "server-wide telemetry is configured but the endpoint refused \
                     the connection, and no fallback is configured either, so there \
                     is nothing to examine. Check that NATS is still running with a \
                     monitoring port (`nats-server -m 8222`) — a restart without \
                     `-m` silently removes it. Meanwhile"
                } else {
                    "nothing was named and this deployment has no way to discover \
                     what exists: the NATS binding has no list call. So"
                };
                return failed(nats::Failure::new(
                    "nothing-to-diagnose",
                    format!(
                        "{lead}: (1) pass `streams` (and `consumers` as \
                         \"stream/consumer\", and `buckets`) on the call; (2) set \
                         MCP_NATS_STREAMS / MCP_NATS_CONSUMERS / MCP_NATS_BUCKETS on \
                         the workload so zero-argument calls know where to look; \
                         (3) restore MCP_NATS_MONITOR_URL to examine every stream on \
                         the server and enable the retention rules."
                    ),
                ));
            }
            return match diagnose::via_binding(hinted, sample_ms).await {
                Ok(report) => ok(report),
                Err(err) => failed(err),
            };
        }

        let named = inventory::Inventory {
            streams,
            consumers,
            buckets,
            source: "caller-supplied",
            monitor: inventory::probe().await,
        };
        match diagnose::via_binding(named, sample_ms).await {
            Ok(report) => ok(report),
            Err(err) => failed(err),
        }
    }

    #[tool(
        description = "Measure how fast a stream is actually taking messages, by reading \
                       its state twice separated by sample_ms. A single stream_info cannot \
                       tell an idle stream holding 9,000 messages from one filling at 900 a \
                       minute; this can. Rates are computed from the sequence delta rather \
                       than the message count, so retention discarding from the tail does \
                       not mask live ingest."
    )]
    #[tracing::instrument(name = "tool.jetstream_stream_rate", skip(self, params), fields(stream = %params.stream))]
    async fn jetstream_stream_rate(
        &self,
        Parameters(params): Parameters<StreamRateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let sample_ms = diagnose::clamp_sample_ms(params.sample_ms);
        let before = match nats::stream_info(params.stream.clone()).await {
            Ok(info) => info,
            Err(err) => return failed(err),
        };
        let started = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(sample_ms))).await;
        let after = match nats::stream_info(params.stream.clone()).await {
            Ok(info) => info,
            Err(err) => return failed(err),
        };
        let elapsed = started.elapsed().as_secs_f64().max(0.001);

        let published = after.last_sequence.saturating_sub(before.last_sequence);
        let stored = after.messages as i64 - before.messages as i64;
        let bytes = after.bytes.saturating_sub(before.bytes);

        ok(serde_json::json!({
            "stream": after.name,
            "sample_ms": (elapsed * 1000.0) as u64,
            "msgs_per_sec": round2(published as f64 / elapsed),
            "msgs_per_min": round2(published as f64 / elapsed * 60.0),
            "bytes_per_sec": round2(bytes as f64 / elapsed),
            "published_during_sample": published,
            // Negative means retention discarded faster than publishers wrote.
            "stored_delta": stored,
            "messages": after.messages,
            "bytes": after.bytes,
            "first_sequence": after.first_sequence,
            "last_sequence": after.last_sequence,
            "consumer_count": after.consumer_count,
            "idle": published == 0,
        }))
    }

    #[tool(
        description = "Whether a consumer is keeping up, as a verdict rather than counters. \
                       Samples the consumer twice and reports backlog, its trend, ack-window \
                       saturation, redeliveries, and an estimated drain time when it is \
                       catching up. Use this instead of reading num_pending once — one \
                       reading cannot distinguish a busy consumer from a drowning one."
    )]
    #[tracing::instrument(name = "tool.jetstream_consumer_lag", skip(self, params), fields(stream = %params.stream, consumer = %params.consumer))]
    async fn jetstream_consumer_lag(
        &self,
        Parameters(params): Parameters<ConsumerLagParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let sample_ms = diagnose::clamp_sample_ms(params.sample_ms);
        let before = match nats::consumer_info(params.stream.clone(), params.consumer.clone()).await
        {
            Ok(info) => info,
            Err(err) => return failed(err),
        };
        let started = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(sample_ms))).await;
        let after = match nats::consumer_info(params.stream.clone(), params.consumer.clone()).await {
            Ok(info) => info,
            Err(err) => return failed(err),
        };
        let elapsed = started.elapsed().as_secs_f64().max(0.001);

        let change = after.num_pending as i64 - before.num_pending as i64;
        let change_per_sec = change as f64 / elapsed;

        // max_ack_pending is unlimited as either 0 or a saturated u64.
        let ack_limit = after.max_ack_pending;
        let saturation = (ack_limit > 0 && ack_limit < u64::MAX)
            .then(|| after.num_ack_pending as f64 / ack_limit as f64);

        let verdict = if after.num_pending == 0 {
            "caught-up"
        } else if change_per_sec > 0.0 {
            "falling-behind"
        } else if change_per_sec < 0.0 {
            "draining"
        } else if after.num_ack_pending > 0 {
            "working"
        } else {
            "stalled"
        };

        let drain_estimate = (change_per_sec < 0.0 && after.num_pending > 0).then(|| {
            let seconds = after.num_pending as f64 / -change_per_sec;
            humanize(seconds)
        });

        ok(serde_json::json!({
            "stream": after.stream,
            "consumer": after.name,
            "verdict": verdict,
            "sample_ms": (elapsed * 1000.0) as u64,
            "num_pending": after.num_pending,
            "pending_change": change,
            "pending_change_per_sec": round2(change_per_sec),
            "drain_estimate": drain_estimate,
            "num_ack_pending": after.num_ack_pending,
            "max_ack_pending": ack_limit,
            "ack_saturation": saturation.map(round2),
            "ack_saturated": saturation.map(|value| value >= 0.9).unwrap_or(false),
            "num_redelivered": after.num_redelivered,
            "ack_wait_ms": after.ack_wait_ms,
            "filter_subject": after.filter_subject,
            "interpretation": match verdict {
                "caught-up" => "Nothing is waiting; the consumer has drained the stream.",
                "falling-behind" => "The backlog grew during sampling — the consumer is \
                                     slower than the publishers and will not recover on \
                                     its own.",
                "draining" => "The backlog is shrinking; the consumer is catching up.",
                "working" => "The backlog is steady and messages are in flight — the \
                              consumer is keeping pace exactly.",
                _ => "Messages are pending, none are in flight, and the count is not \
                      moving: nothing is fetching from this consumer.",
            },
        }))
    }

    // --- Server-wide telemetry (optional monitoring endpoint) -------------

    #[tool(
        description = "The NATS server itself: version, uptime, health, connection and \
                       subscription counts, slow-consumer drops, memory/CPU, and JetStream \
                       usage against its configured limits. This data lives behind $SYS and \
                       the JetStream API, which the NATS binding reserves for the host and \
                       no grant can open, so it comes from the server's monitoring port \
                       instead — optional, read-only, and off unless an operator enabled \
                       it. When it is off this tool returns the exact steps to turn it on."
    )]
    #[tracing::instrument(name = "tool.nats_server_info", skip(self))]
    async fn nats_server_info(&self) -> Result<CallToolResult, ErrorData> {
        if !monitor::is_enabled() {
            return failed(monitor::not_configured());
        }
        let varz = match monitor::varz().await {
            Ok(value) => value,
            Err(err) => return failed(err),
        };
        let jsz = monitor::jsz(false).await.unwrap_or(serde_json::Value::Null);
        let healthz = monitor::healthz().await.unwrap_or(serde_json::Value::Null);
        let config = jsz.get("config").cloned().unwrap_or(serde_json::Value::Null);

        ok(serde_json::json!({
            "server_name": field_str(&varz, "server_name"),
            "server_id": field_str(&varz, "server_id"),
            "version": field_str(&varz, "version"),
            "go_version": field_str(&varz, "go"),
            "uptime": field_str(&varz, "uptime"),
            "start_time": field_str(&varz, "start"),
            "health": field_str(&healthz, "status"),
            "host": field_str(&varz, "host"),
            "port": field_u64(&varz, "port"),
            "max_payload_bytes": field_u64(&varz, "max_payload"),
            "jetstream_enabled": varz.get("jetstream").is_some(),
            "tls_required": varz.get("tls_required").and_then(serde_json::Value::as_bool),
            "auth_required": varz.get("auth_required").and_then(serde_json::Value::as_bool),
            "connections": {
                "current": field_u64(&varz, "connections"),
                "total_since_start": field_u64(&varz, "total_connections"),
                "subscriptions": field_u64(&varz, "subscriptions"),
                "slow_consumer_drops": field_u64(&varz, "slow_consumers"),
                "routes": field_u64(&varz, "routes"),
                "leafnodes": field_u64(&varz, "leafnodes"),
            },
            "traffic": {
                "in_msgs": field_u64(&varz, "in_msgs"),
                "out_msgs": field_u64(&varz, "out_msgs"),
                "in_bytes": field_u64(&varz, "in_bytes"),
                "out_bytes": field_u64(&varz, "out_bytes"),
            },
            "process": {
                "mem_bytes": field_u64(&varz, "mem"),
                "cpu_percent": varz.get("cpu").and_then(serde_json::Value::as_f64),
            },
            "jetstream": {
                "streams": field_u64(&jsz, "streams"),
                "consumers": field_u64(&jsz, "consumers"),
                "messages": field_u64(&jsz, "messages"),
                "bytes": field_u64(&jsz, "bytes"),
                "storage_bytes": field_u64(&jsz, "storage"),
                "max_storage_bytes": field_u64(&config, "max_storage"),
                "memory_bytes": field_u64(&jsz, "memory"),
                "max_memory_bytes": field_u64(&config, "max_memory"),
                "store_dir": field_str(&config, "store_dir"),
                "api_errors": field_u64(&jsz.get("api").cloned().unwrap_or(serde_json::Value::Null), "errors"),
            },
        }))
    }

    #[tool(
        description = "Every stream on the server with its retention configuration, live \
                       state, and consumers — the inventory the NATS binding cannot \
                       produce, since it has no list call and requires you to already know \
                       each name. Needs the optional monitoring endpoint. Use this to \
                       discover what exists before inspecting anything; set \
                       include_consumers=false for a shorter listing."
    )]
    #[tracing::instrument(name = "tool.nats_server_streams", skip(self, params))]
    async fn nats_server_streams(
        &self,
        Parameters(params): Parameters<ServerStreamsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if !monitor::is_enabled() {
            return failed(monitor::not_configured());
        }
        let jsz = match monitor::jsz(true).await {
            Ok(value) => value,
            Err(err) => return failed(err),
        };
        let include_consumers = params.include_consumers.unwrap_or(true);

        let mut streams = Vec::new();
        let accounts = jsz
            .get("account_details")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();

        for account in &accounts {
            let account_name = field_str(account, "name");
            let details = account
                .get("stream_detail")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();

            for stream in &details {
                let config = stream.get("config").cloned().unwrap_or(serde_json::Value::Null);
                let state = stream.get("state").cloned().unwrap_or(serde_json::Value::Null);
                let name = field_str(stream, "name");

                // A KV bucket is a stream named KV_<bucket>; say so, since the
                // kv_* tools address it by the bare bucket name.
                let kv_bucket = name.strip_prefix("KV_").map(str::to_owned);

                let max_age = field_u64(&config, "max_age");
                let max_msgs = field_i64(&config, "max_msgs");
                let max_bytes = field_i64(&config, "max_bytes");
                // A per-subject cap bounds the stream too — it is what limits
                // a KV bucket, whose `history` is stored as max_msgs_per_subject.
                let max_per_subject = field_i64(&config, "max_msgs_per_subject");
                let bounded =
                    max_age > 0 || max_msgs > 0 || max_bytes > 0 || max_per_subject > 0;

                let mut entry = serde_json::json!({
                    "name": name,
                    "account": account_name,
                    "kv_bucket": kv_bucket,
                    "subjects": config.get("subjects").cloned().unwrap_or(serde_json::Value::Null),
                    "retention": field_str(&config, "retention"),
                    "storage": field_str(&config, "storage"),
                    "discard": field_str(&config, "discard"),
                    "max_age_ns": max_age,
                    "max_msgs": max_msgs,
                    "max_bytes": max_bytes,
                    "max_msgs_per_subject": max_per_subject,
                    // The single most useful derived bit: an unbounded stream
                    // under a `limits` policy grows until the disk fills.
                    "bounded": bounded,
                    "messages": field_u64(&state, "messages"),
                    "bytes": field_u64(&state, "bytes"),
                    "first_seq": field_u64(&state, "first_seq"),
                    "last_seq": field_u64(&state, "last_seq"),
                    "num_subjects": field_u64(&state, "num_subjects"),
                    "num_deleted": field_u64(&state, "num_deleted"),
                    "consumer_count": field_u64(&state, "consumer_count"),
                });

                if include_consumers {
                    let consumers: Vec<serde_json::Value> = stream
                        .get("consumer_detail")
                        .and_then(serde_json::Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|consumer| {
                            let cfg =
                                consumer.get("config").cloned().unwrap_or(serde_json::Value::Null);
                            serde_json::json!({
                                "name": field_str(consumer, "name"),
                                "durable": !field_str(&cfg, "durable_name").is_empty(),
                                "ack_policy": field_str(&cfg, "ack_policy"),
                                "ack_wait_ms": field_u64(&cfg, "ack_wait") / 1_000_000,
                                "max_ack_pending": field_i64(&cfg, "max_ack_pending"),
                                "max_deliver": field_i64(&cfg, "max_deliver"),
                                "filter_subject": field_str(&cfg, "filter_subject"),
                                "num_pending": field_u64(consumer, "num_pending"),
                                "num_ack_pending": field_u64(consumer, "num_ack_pending"),
                                "num_redelivered": field_u64(consumer, "num_redelivered"),
                                "num_waiting": field_u64(consumer, "num_waiting"),
                            })
                        })
                        .collect();
                    if let Some(map) = entry.as_object_mut() {
                        map.insert("consumers".to_owned(), serde_json::Value::Array(consumers));
                    }
                }

                streams.push(entry);
            }
        }

        ok(serde_json::json!({
            "count": streams.len(),
            "streams": streams,
            "note": "This inventory comes from the monitoring endpoint and covers the \
                     whole server. Which of these this workload may actually read is a \
                     separate question — nats_check_access answers it.",
        }))
    }

    #[tool(
        description = "The clients currently connected to the NATS server: who they are, \
                       how many subscriptions each holds, and their pending (unflushed) \
                       bytes — the number that identifies a slow consumer before the server \
                       drops it. Needs the optional monitoring endpoint."
    )]
    #[tracing::instrument(name = "tool.nats_connections", skip(self, params))]
    async fn nats_connections(
        &self,
        Parameters(params): Parameters<ConnectionsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if !monitor::is_enabled() {
            return failed(monitor::not_configured());
        }
        let limit = params.limit.unwrap_or(64);
        let connz = match monitor::connz(limit).await {
            Ok(value) => value,
            Err(err) => return failed(err),
        };

        let connections: Vec<serde_json::Value> = connz
            .get("connections")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|connection| {
                serde_json::json!({
                    "cid": field_u64(connection, "cid"),
                    "name": field_str(connection, "name"),
                    "kind": field_str(connection, "kind"),
                    "language": field_str(connection, "lang"),
                    "version": field_str(connection, "version"),
                    "ip": field_str(connection, "ip"),
                    "port": field_u64(connection, "port"),
                    "uptime": field_str(connection, "uptime"),
                    "idle": field_str(connection, "idle"),
                    "subscriptions": field_u64(connection, "subscriptions"),
                    "subjects": connection.get("subscriptions_list").cloned(),
                    "in_msgs": field_u64(connection, "in_msgs"),
                    "out_msgs": field_u64(connection, "out_msgs"),
                    // Non-zero and climbing means the server is writing faster
                    // than this client reads: the slow-consumer precursor.
                    "pending_bytes": field_u64(connection, "pending_bytes"),
                })
            })
            .collect();

        ok(serde_json::json!({
            "num_connections": field_u64(&connz, "num_connections"),
            "total": field_u64(&connz, "total"),
            "returned": connections.len(),
            "connections": connections,
        }))
    }

    #[tool(
        description = "What this workload can actually reach, probed rather than assumed. \
                       Grants are deny-by-default and set by whoever deployed the workload; \
                       the guest cannot read them, so this tries each stream and bucket and \
                       reports granted or denied with the grant key to widen. With \
                       monitoring enabled it discovers every stream on the server and probes \
                       them all, which turns a denial into a map instead of a surprise."
    )]
    #[tracing::instrument(name = "tool.nats_check_access", skip(self, params))]
    async fn nats_check_access(
        &self,
        Parameters(params): Parameters<CheckAccessParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut streams = params.streams.unwrap_or_default();
        let mut buckets = params.buckets.unwrap_or_default();
        let mut discovered = false;
        let mut scope = "caller-supplied";

        // Nothing named: enumerate from monitoring if we can. KV buckets are
        // streams named KV_<bucket>, so one listing seeds both probes.
        if streams.is_empty() && buckets.is_empty() && monitor::is_enabled() {
            if let Ok(jsz) = monitor::jsz(true).await {
                discovered = true;
                scope = "monitoring";
                for account in jsz
                    .get("account_details")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                {
                    for stream in account
                        .get("stream_detail")
                        .and_then(serde_json::Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                    {
                        let name = field_str(stream, "name");
                        match name.strip_prefix("KV_") {
                            Some(bucket) => buckets.push(bucket.to_owned()),
                            None => streams.push(name),
                        }
                    }
                }
            }
        }

        if streams.is_empty() && buckets.is_empty() {
            // Either monitoring is unconfigured or it just failed; the
            // operator's hints are the remaining inventory. Probing them is
            // exactly how a caller learns a hinted name is outside the grants.
            let hinted = inventory::hinted();
            if !hinted.is_empty() {
                streams = hinted.streams;
                buckets = hinted.buckets;
                scope = if monitor::is_enabled() {
                    "hinted (monitoring unreachable)"
                } else {
                    "hinted"
                };
            }
        }

        if streams.is_empty() && buckets.is_empty() {
            return failed(nats::Failure::new(
                "nothing-to-check",
                "no streams or buckets were named and none could be discovered. \
                 Pass `streams` and/or `buckets` explicitly, set MCP_NATS_STREAMS / \
                 MCP_NATS_BUCKETS on the workload, or enable monitoring so this can \
                 enumerate the server itself — nats_server_info explains how.",
            ));
        }

        let mut stream_access = Vec::new();
        for name in streams {
            // stream_info is a pure read: probing costs nothing and mutates
            // nothing.
            let entry = match nats::stream_info(name.clone()).await {
                Ok(info) => serde_json::json!({
                    "stream": name,
                    "access": "granted",
                    "messages": info.messages,
                    "consumer_count": info.consumer_count,
                }),
                Err(err) => serde_json::json!({
                    "stream": name,
                    "access": if err.code == "denied" { "denied" } else { "error" },
                    "code": err.code,
                    "detail": err.message,
                }),
            };
            stream_access.push(entry);
        }

        let mut bucket_access = Vec::new();
        for name in buckets {
            let entry = match nats::kv_status(name.clone()).await {
                Ok(status) => serde_json::json!({
                    "bucket": name,
                    "access": "granted",
                    "values": status.values,
                    "bytes": status.bytes,
                }),
                Err(err) => serde_json::json!({
                    "bucket": name,
                    "access": if err.code == "denied" { "denied" } else { "error" },
                    "code": err.code,
                    "detail": err.message,
                }),
            };
            bucket_access.push(entry);
        }

        let granted = stream_access
            .iter()
            .chain(bucket_access.iter())
            .filter(|entry| entry.get("access").and_then(serde_json::Value::as_str) == Some("granted"))
            .count();
        let denied = stream_access
            .iter()
            .chain(bucket_access.iter())
            .filter(|entry| entry.get("access").and_then(serde_json::Value::as_str) == Some("denied"))
            .count();

        ok(serde_json::json!({
            "scope": scope,
            "discovered_from_monitoring": discovered,
            "granted": granted,
            "denied": denied,
            "streams": stream_access,
            "buckets": bucket_access,
            "note": "Subject grants (`subject-allow`) are not probed: the only way to \
                     test one is to publish, which would have side effects. Widening \
                     any of these is an operator change in the Workload's \
                     wasmcloud:nats hostInterface config — the guest cannot do it.",
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for NatsServer {
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
                "NATS tools and diagnostics, served by a WebAssembly component that \
                 reaches NATS through its host binding rather than the network.\n\n\
                 START HERE when something is wrong: nats_diagnose samples the \
                 deployment twice, applies rules, and returns findings with evidence \
                 and a remedy each. jetstream_stream_rate and jetstream_consumer_lag \
                 answer \"is this moving, and is it keeping up\" — a single reading \
                 cannot, which is why both sample over a window.\n\
                 Core: nats_publish (fire-and-forget), nats_request (RPC, fails fast with \
                 `no-responders`).\n\
                 JetStream: jetstream_publish (durable, acked); jetstream_scan and \
                 jetstream_get_message to READ a stream without consuming it; \
                 jetstream_stream_info / jetstream_list_subjects / jetstream_consumer_info \
                 to inspect; jetstream_fetch to drive a real pull consumer.\n\
                 KV: kv_get, kv_put, kv_create, kv_update (compare-and-swap), kv_delete, \
                 kv_purge, kv_keys, kv_history, kv_status.\n\
                 Server-wide (needs the optional monitoring endpoint): nats_server_info, \
                 nats_server_streams (the full stream/consumer inventory — the binding \
                 has no list call), nats_connections. nats_check_access probes what this \
                 workload may actually reach.\n\n\
                 Three things to know before you start.\n\n\
                 1. Prefer jetstream_scan over jetstream_fetch to look at stream contents. \
                 scan creates no consumer and acknowledges nothing; fetch moves real \
                 consumer state, and `settle: \"ack\"` consumes messages for good.\n\n\
                 2. Streams, consumers, and buckets are provisioned out-of-band and cannot \
                 be created here — this surface reads and writes what already exists. \
                 Subjects, streams, and buckets are also allow-listed per workload and \
                 deny-by-default: a `denied` failure names the grant an operator would have \
                 to widen, and no amount of retrying will change it.\n\n\
                 3. Server-wide telemetry (connections, slow consumers, the stream \
                 inventory) does NOT come from the NATS binding: $SYS and the JetStream \
                 API are reserved for the host there and no grant can open them. It \
                 comes from the NATS server's monitoring port over HTTP, which is \
                 optional and off by default. Those tools return the enablement steps \
                 when it is off; every other tool works regardless.\n\n\
                 Bodies round-trip as `payload`/`value` (UTF-8 text) or \
                 `payload_base64`/`value_base64` (anything else); results are truncated \
                 past 64 KiB and say so with `truncated: true`.\n\n\
                 This server publishes a skill — a playbook for operating and \
                 diagnosing NATS through these tools, including how to read a \
                 diagnosis, which reads are destructive, and what a `denied` failure \
                 means. Read `skill://index.json` for the catalog, then the SKILL.md \
                 it points at.",
            )
    }

    /// Skills over MCP: every skill file, plus the catalog, as resources.
    ///
    /// The whole set is returned in one page — a server embedding enough
    /// skills for that to be unwieldy should honour `request.cursor` and set
    /// `next_cursor` on the result instead.
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
