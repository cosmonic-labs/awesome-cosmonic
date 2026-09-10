//! The `wasmcloud:nats@0.1.0` capability, wrapped for tool code.
//!
//! Three layers live here:
//!
//! 1. **Bindings.** [`wit_bindgen::generate!`] binds the host's async NATS
//!    imports. It must be the same `wit-bindgen` the `wasip3` crate uses: the
//!    async canonical ABI keeps its task state in that crate's runtime, so a
//!    second, differently-versioned generator would hand the NATS imports a
//!    different executor from the `wasi:http` export and trap on the first
//!    `await`. (See the dependency comment in `Cargo.toml`.)
//! 2. **Views.** Serde mirrors of the WIT records ([`Message`], [`Entry`],
//!    [`StreamInfo`], …). The generated types can't be `Serialize`d directly,
//!    and tools need stable, documented JSON anyway.
//! 3. **Operations.** Each public `async fn` is callable from tool (tokio)
//!    context. It packages the call as a job for [`crate::bridge::submit`],
//!    which runs it in component-model context and returns plain data.
//!
//! That last point is a hard constraint, not a style choice: host resource
//! handles (`kv::Bucket`, `jetstream::PullConsumer`, `MessageHandle`) are
//! neither `Send` nor valid outside component-model context. Every operation
//! here opens what it needs, uses it, and drops it before returning — no
//! handle ever reaches a tool.
//!
//! ## Grants
//!
//! Every name is checked host-side against the workload's grants before it
//! reaches the server, and they are deny-by-default: `subject-allow` covers
//! publish/request subjects, `stream-allow` stream reads, `bucket-allow` KV
//! buckets. They are set in the Workload's `wasmcloud:nats` `hostInterface`
//! config — by whoever deploys the workload, never by the guest — so a
//! [`Failure`] with code `denied` is an operator change, not a retry. See
//! [`describe`], which spells out which grant to widen.

use std::collections::BTreeMap;
use std::future::Future;

use base64::Engine as _;
use serde::Serialize;

wit_bindgen::generate!({
    path: "wit",
    world: "nats-client",
    generate_all,
});

use self::wasmcloud::nats::{core, jetstream, kv, types};

/// Largest reply/entry/message body rendered into a tool result, in bytes,
/// overridable with `MCP_NATS_MAX_PAYLOAD_BYTES`.
///
/// A NATS server's `max_payload` is commonly 1 MiB and streams can hold far
/// more in aggregate; splicing that into an MCP result would blow up the
/// caller's context for no benefit. Bodies over the cap are truncated with
/// `truncated: true` so a reader can tell a clipped payload from a short one.
const DEFAULT_MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Ceiling on any caller-supplied timeout, in milliseconds.
///
/// While a NATS job runs, the bridge is inside component-model context and the
/// tokio world is frozen with the request lock held (see [`crate::bridge`]) —
/// so a caller-chosen timeout is a lease on the whole instance. This bounds
/// the damage a careless `timeout_ms` can do.
const MAX_TIMEOUT_MS: u32 = 30_000;

fn max_payload_bytes() -> usize {
    std::env::var("MCP_NATS_MAX_PAYLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_PAYLOAD_BYTES)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failed NATS operation, shaped for a tool result.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    /// Stable machine-readable code, mirroring the `nats-error` variant
    /// (`denied`, `timeout`, `no-responders`, `key-not-found`, …). Callers
    /// should branch on this rather than on `message`.
    pub code: &'static str,
    /// Human-readable explanation, including the fix where there is one.
    pub message: String,
    /// Present only on `revision-mismatch`: the key's actual current
    /// revision, so a compare-and-swap retry needs no intervening read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<u64>,
}

impl Failure {
    /// Builds a failure. `pub(crate)` because the monitoring client and the
    /// diagnostics engine report through this same shape: a caller should not
    /// have to branch on where in the component an error came from.
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            current_revision: None,
        }
    }

    /// The bridge driver went away before the operation ran.
    fn bridge_closed() -> Self {
        Self::new(
            "bridge-closed",
            "the NATS bridge went away before the operation ran (the request \
             was cancelled or the component is shutting down); retry it",
        )
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Turns a host `nats-error` into a [`Failure`], naming the fix where the
/// error implies one.
fn describe(err: types::NatsError) -> Failure {
    use types::NatsError as E;
    match err {
        E::Connection(detail) => Failure::new(
            "connection",
            format!("transport error talking to NATS: {detail}"),
        ),
        E::Timeout(detail) => Failure::new("timeout", format!("operation timed out: {detail}")),
        E::NoResponders => Failure::new(
            "no-responders",
            "no subscriber is listening on that subject — unlike a timeout, \
             retrying will fail identically until a responder appears",
        ),
        E::Denied(denial) => Failure::new("denied", describe_denial(&denial)),
        E::MaxPayloadExceeded(limit) => Failure::new(
            "max-payload-exceeded",
            format!(
                "payload exceeds the server's max_payload of {limit} bytes \
                 (header bytes count toward it)"
            ),
        ),
        E::InvalidHeader(detail) => Failure::new(
            "invalid-header",
            format!(
                "header not representable on the NATS wire: {detail} — names \
                 must be printable ASCII without ':', values must not contain \
                 CR or LF"
            ),
        ),
        E::Jetstream(detail) => Failure::new("jetstream", format!("JetStream error: {detail}")),
        E::KeyNotFound => Failure::new(
            "key-not-found",
            "the key is absent, deleted, or purged from the bucket",
        ),
        E::RevisionMismatch(current) => Failure {
            code: "revision-mismatch",
            message: format!(
                "compare-and-swap failed: the key is now at revision {current}. \
                 Retry with expected_revision={current} — no re-read needed"
            ),
            current_revision: Some(current),
        },
        E::NoMessages => Failure::new(
            "no-messages",
            "the consumer had nothing to deliver within the timeout (the fetch \
             itself ran fine — this is an empty result, not a refusal)",
        ),
        E::LimitExceeded(detail) => Failure::new(
            "limit-exceeded",
            format!(
                "the fetch was refused before anything was delivered: {detail}. \
                 Either it exceeds a limit the consumer was provisioned with \
                 (compare jetstream_consumer_info's max_request_batch / \
                 max_request_max_bytes), or earlier fetched messages still hold \
                 the binding's memory budget. Retrying unchanged fails the same way"
            ),
        ),
        E::NotFound(name) => Failure::new(
            "not-found",
            format!("no such stream, bucket, or consumer: {name}"),
        ),
        E::UnsupportedByServer(minimum) => Failure::new(
            "unsupported-by-server",
            format!("the connected NATS server is too old for this operation (needs {minimum} or newer)"),
        ),
        E::Disconnected => Failure::new(
            "disconnected",
            "no live NATS connection is bound to this workload — check the \
             host's NATS plugin settings (Settings → Built-in plugins → NATS)",
        ),
        E::AlreadySettled => Failure::new(
            "already-settled",
            "the message was already acked, naked, or terminated; the work was \
             done, so this is not a lost message",
        ),
        E::AckOwnedByHost => Failure::new(
            "ack-owned-by-host",
            "this binding runs ack-mode: auto, so the host owns the settle — \
             change it in the Workload manifest, not the guest",
        ),
        E::Unexpected(detail) => Failure::new("unexpected", detail),
    }
}

/// Spells out a grant refusal: which name was refused, and which grant key an
/// operator would have to widen to permit it.
///
/// The `denied` code is prefixed by [`Failure`]'s `Display`, so this text must
/// not repeat it.
fn describe_denial(denial: &types::Denial) -> String {
    use types::{DeniedResource as R, DenialReason as Why};

    let (kind, grant) = match &denial.target {
        R::Subject => ("subject", Some("subject-allow")),
        R::Stream => ("stream", Some("stream-allow")),
        R::Bucket => ("KV bucket", Some("bucket-allow")),
        // The refusal deliberately names the sequence rather than the subject
        // it was stored on: naming it would let a caller walk sequences to
        // enumerate subjects it was never granted.
        R::Message(sequence) => {
            return format!(
                "stored message at sequence {sequence} in stream \
                 {name:?} is on a subject outside this workload's grant. \
                 Reading a stored message is checked against `subject-allow`, \
                 so widen that to cover the subject the stream keeps those \
                 messages on",
                name = denial.name,
            );
        }
    };

    let reason = match denial.reason {
        Why::Reserved => {
            "it is in a space the host reserves for itself (the JetStream API, \
             the KV/object-store key spaces, $SYS, or the host's own lattice). \
             No grant can open this one"
        }
        Why::NotGranted => {
            "no grant on this workload's binding covers it. Widen the grant in \
             the Workload's wasmcloud:nats hostInterface config — that is an \
             operator change, not something the server can do for you"
        }
        Why::WildcardNotAllowed => {
            "publish and request subjects must be literal: `*` and `>` are \
             only valid in subscription patterns"
        }
    };

    match (denial.reason, grant) {
        (Why::NotGranted, Some(grant)) => format!(
            "{kind} {name:?} — {reason} (the relevant key is `{grant}`)",
            name = denial.name,
        ),
        _ => format!("{kind} {name:?} — {reason}", name = denial.name),
    }
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// A message body rendered for a tool result.
///
/// Exactly one of `text` / `base64` is present: UTF-8 bodies are shown as
/// text (what an agent can actually read), anything else as base64.
#[derive(Debug, Serialize)]
pub struct Payload {
    /// The body as text, when it is valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The body base64-encoded, when it is not valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    /// Size of the body on the wire, before any truncation.
    pub bytes: usize,
    /// True when `text`/`base64` carry only a prefix of the body.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl Payload {
    fn encode(body: Vec<u8>) -> Self {
        let bytes = body.len();
        let limit = max_payload_bytes();
        let truncated = bytes > limit;

        match String::from_utf8(body) {
            Ok(mut text) => {
                if truncated {
                    // `String::truncate` panics off a char boundary, which any
                    // multibyte body longer than the limit would hit.
                    let mut end = limit;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                Self {
                    text: Some(text),
                    base64: None,
                    bytes,
                    truncated,
                }
            }
            Err(err) => {
                let raw = err.into_bytes();
                let clipped = &raw[..limit.min(raw.len())];
                Self {
                    text: None,
                    base64: Some(base64::engine::general_purpose::STANDARD.encode(clipped)),
                    bytes,
                    truncated,
                }
            }
        }
    }
}

/// Decodes a tool's payload arguments into message bytes.
///
/// `text` and `base64` are mutually exclusive; both absent means an empty
/// body, which is a legitimate NATS message.
pub fn decode_body(text: Option<String>, b64: Option<String>) -> Result<Vec<u8>, Failure> {
    match (text, b64) {
        (Some(_), Some(_)) => Err(Failure::new(
            "invalid-argument",
            "pass either `payload` or `payload_base64`, not both",
        )),
        (Some(text), None) => Ok(text.into_bytes()),
        (None, Some(b64)) => base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|err| {
                Failure::new(
                    "invalid-argument",
                    format!("`payload_base64` is not valid base64: {err}"),
                )
            }),
        (None, None) => Ok(Vec::new()),
    }
}

fn encode_headers(headers: Option<BTreeMap<String, String>>) -> Option<Vec<types::HeaderEntry>> {
    let headers = headers.filter(|map| !map.is_empty())?;
    Some(
        headers
            .into_iter()
            .map(|(name, value)| types::HeaderEntry { name, value })
            .collect(),
    )
}

/// Headers as a name→value map. NATS permits repeated names; on the rare
/// message that uses them the values are joined with `, `, matching how HTTP
/// treats a repeated field.
fn decode_headers(headers: Option<Vec<types::HeaderEntry>>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for entry in headers.unwrap_or_default() {
        map.entry(entry.name)
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&entry.value);
            })
            .or_insert(entry.value);
    }
    map
}

fn build_message(
    subject: String,
    body: Vec<u8>,
    headers: Option<BTreeMap<String, String>>,
) -> types::NatsMessage {
    types::NatsMessage {
        subject,
        body,
        // The host always substitutes its own per-workload inbox as the reply
        // subject, so setting this would be silently ignored.
        reply_to: None,
        headers: encode_headers(headers),
    }
}

fn clamp_timeout(timeout_ms: Option<u32>, default_ms: u32) -> u32 {
    timeout_ms.unwrap_or(default_ms).clamp(1, MAX_TIMEOUT_MS)
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// A NATS message as returned to a tool.
#[derive(Debug, Serialize)]
pub struct Message {
    pub subject: String,
    pub payload: Payload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl From<types::NatsMessage> for Message {
    fn from(msg: types::NatsMessage) -> Self {
        Self {
            subject: msg.subject,
            payload: Payload::encode(msg.body),
            reply_to: msg.reply_to,
            headers: decode_headers(msg.headers),
        }
    }
}

/// Acknowledgment from a JetStream publish.
#[derive(Debug, Serialize)]
pub struct PublishAck {
    pub stream: String,
    pub sequence: u64,
    /// True when the server matched this message's `Nats-Msg-Id` inside the
    /// stream's duplicate window and did *not* store it again.
    pub duplicate: bool,
}

/// A message read out of a stream (a snapshot; nothing is consumed).
#[derive(Debug, Serialize)]
pub struct StoredMessage {
    pub subject: String,
    pub sequence: u64,
    pub payload: Payload,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl From<jetstream::StoredMessage> for StoredMessage {
    fn from(msg: jetstream::StoredMessage) -> Self {
        Self {
            subject: msg.subject,
            sequence: msg.sequence,
            payload: Payload::encode(msg.data),
            headers: decode_headers(msg.headers),
        }
    }
}

/// Read-only snapshot of a stream.
#[derive(Debug, Serialize)]
pub struct StreamInfo {
    pub name: String,
    /// Subjects the stream is *configured* to capture (not what it holds).
    pub subjects: Vec<String>,
    pub messages: u64,
    pub bytes: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub consumer_count: u64,
}

impl From<jetstream::StreamInfo> for StreamInfo {
    fn from(info: jetstream::StreamInfo) -> Self {
        Self {
            name: info.name,
            subjects: info.subjects,
            messages: info.messages,
            bytes: info.bytes,
            first_sequence: info.first_sequence,
            last_sequence: info.last_sequence,
            consumer_count: info.consumer_count,
        }
    }
}

/// One subject a stream currently holds messages on.
#[derive(Debug, Serialize)]
pub struct SubjectCount {
    pub subject: String,
    pub messages: u64,
}

/// Read-only snapshot of a consumer, including its provisioned limits.
///
/// Zero means "unset" throughout — that is how the server reports an absent
/// limit, and it is preserved rather than reinterpreted.
#[derive(Debug, Serialize)]
pub struct ConsumerInfo {
    pub name: String,
    pub stream: String,
    /// Singular filter; empty when unset or when `filter_subjects` is used.
    pub filter_subject: String,
    /// Multi-subject filter (NATS 2.10+). Empty when the consumer uses the
    /// singular filter, or none at all.
    pub filter_subjects: Vec<String>,
    pub max_ack_pending: u64,
    pub max_waiting: u64,
    /// Largest `batch` one fetch may ask for. A fetch above it is *refused*
    /// with `limit-exceeded`, not truncated.
    pub max_request_batch: u64,
    /// Largest byte bound one fetch may ask for. Counts subject, reply
    /// subject, and payload (~63 bytes of overhead per small message).
    pub max_request_max_bytes: u64,
    pub max_deliver: u64,
    pub ack_wait_ms: u64,
    pub num_ack_pending: u64,
    pub num_pending: u64,
    pub num_redelivered: u64,
}

impl From<jetstream::ConsumerInfo> for ConsumerInfo {
    fn from(info: jetstream::ConsumerInfo) -> Self {
        Self {
            name: info.name,
            stream: info.stream_name,
            filter_subject: info.filter_subject,
            filter_subjects: info.filter_subjects,
            max_ack_pending: info.max_ack_pending,
            max_waiting: info.max_waiting,
            max_request_batch: info.max_request_batch,
            max_request_max_bytes: info.max_request_max_bytes,
            max_deliver: info.max_deliver,
            ack_wait_ms: info.ack_wait_ms,
            num_ack_pending: info.num_ack_pending,
            num_pending: info.num_pending,
            num_redelivered: info.num_redelivered,
        }
    }
}

/// One message delivered by a pull fetch, after settling.
#[derive(Debug, Serialize)]
pub struct FetchedMessage {
    pub subject: String,
    pub sequence: u64,
    /// 1 on first delivery; higher means this is a redelivery.
    pub delivery_count: u32,
    pub payload: Payload,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// How this message was settled, or why settling failed.
    pub settled: String,
}

/// The result of a pull fetch.
#[derive(Debug, Serialize)]
pub struct FetchedBatch {
    pub messages: Vec<FetchedMessage>,
    /// `batch-filled` (got everything asked for), `drained` (that was all the
    /// consumer had), or `byte-limit` (a byte bound ended it early — more is
    /// waiting, and the next fetch resumes where this stopped).
    pub stop: &'static str,
}

/// What to do with each message a fetch delivers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Settle {
    /// Leave the messages unsettled (default). Nothing is consumed, but the
    /// consumer stalls for its full `ack_wait` before redelivering them.
    #[default]
    None,
    /// Acknowledge: the messages are done and will not be redelivered.
    Ack,
    /// Negative-acknowledge for immediate redelivery.
    Nak,
    /// Terminate: never redeliver, without marking the work as done.
    Term,
}

/// How a KV entry came to be.
fn operation_name(operation: kv::KvOperation) -> &'static str {
    match operation {
        kv::KvOperation::Put => "put",
        kv::KvOperation::Delete => "delete",
        kv::KvOperation::Purge => "purge",
    }
}

/// A single KV entry.
#[derive(Debug, Serialize)]
pub struct Entry {
    pub key: String,
    pub value: Payload,
    pub revision: u64,
    pub created_at_unix_nanos: u64,
    /// `put`, `delete`, or `purge`. A `delete`/`purge` entry is a tombstone:
    /// its value is empty and the key is gone as of that revision.
    pub operation: &'static str,
}

impl From<kv::Entry> for Entry {
    fn from(entry: kv::Entry) -> Self {
        Self {
            key: entry.key,
            value: Payload::encode(entry.value),
            revision: entry.revision,
            created_at_unix_nanos: entry.created_at_unix_nanos,
            operation: operation_name(entry.operation),
        }
    }
}

/// One page of a bucket's key listing.
#[derive(Debug, Serialize)]
pub struct KeyPage {
    pub keys: Vec<String>,
    /// True when the bucket holds more keys than this page carries. The cap
    /// is host-side (1000 keys), so narrow `filter` to walk a larger bucket —
    /// a partial page is never a complete listing.
    pub truncated: bool,
}

/// Status snapshot of a KV bucket.
#[derive(Debug, Serialize)]
pub struct BucketStatus {
    pub bucket: String,
    /// Stored messages in the bucket's stream: live values *plus* retained
    /// history, matching `nats kv status`'s `Values`.
    pub values: u64,
    /// Revisions retained per key.
    pub history: u8,
    pub ttl_seconds: u64,
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Runs `work` in component-model context and returns its result.
///
/// Every public operation below funnels through here — see the module docs for
/// why host handles may not cross back.
async fn call<T, F, Fut>(work: F) -> Result<T, Failure>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, Failure>> + 'static,
    T: Send + 'static,
{
    crate::bridge::submit(work)
        .await
        .unwrap_or_else(|_| Err(Failure::bridge_closed()))
}

/// Publishes a message. Fire-and-forget: this resolves once the message is
/// written to the connection, **not** once a subscriber has seen it.
pub async fn publish(
    subject: String,
    body: Vec<u8>,
    headers: Option<BTreeMap<String, String>>,
) -> Result<(), Failure> {
    call(move || async move {
        core::publish(build_message(subject, body, headers))
            .await
            .map_err(describe)
    })
    .await
}

/// Sends a request and waits for one reply.
pub async fn request(
    subject: String,
    body: Vec<u8>,
    headers: Option<BTreeMap<String, String>>,
    timeout_ms: Option<u32>,
) -> Result<Message, Failure> {
    let timeout_ms = clamp_timeout(timeout_ms, 5_000);
    call(move || async move {
        core::request(build_message(subject, body, headers), timeout_ms)
            .await
            .map(Message::from)
            .map_err(describe)
    })
    .await
}

/// Publishes to a JetStream-managed subject and waits for the stream ack.
pub async fn jetstream_publish(
    subject: String,
    body: Vec<u8>,
    headers: Option<BTreeMap<String, String>>,
) -> Result<PublishAck, Failure> {
    call(move || async move {
        jetstream::publish(build_message(subject, body, headers))
            .await
            .map(|ack| PublishAck {
                stream: ack.stream_name,
                sequence: ack.sequence,
                duplicate: ack.duplicate,
            })
            .map_err(describe)
    })
    .await
}

/// Reads one stored message by stream sequence (direct-get). Non-destructive.
pub async fn get_by_sequence(stream: String, sequence: u64) -> Result<StoredMessage, Failure> {
    call(move || async move {
        jetstream::get_by_sequence(stream, sequence)
            .await
            .map(StoredMessage::from)
            .map_err(describe)
    })
    .await
}

/// Replays up to `max_count` messages from `start_sequence`. Stateless and
/// non-destructive: no durable consumer is created and nothing is acked.
///
/// Sequence numbers may gap — messages on subjects outside the workload's
/// grant are skipped and do not count against `max_count`.
pub async fn scan(
    stream: String,
    start_sequence: u64,
    max_count: u32,
) -> Result<Vec<StoredMessage>, Failure> {
    call(move || async move {
        jetstream::scan(stream, start_sequence, max_count)
            .await
            .map(|messages| messages.into_iter().map(StoredMessage::from).collect())
            .map_err(describe)
    })
    .await
}

/// Stream configuration and state.
pub async fn stream_info(stream: String) -> Result<StreamInfo, Failure> {
    call(move || async move {
        jetstream::get_stream_info(stream)
            .await
            .map(StreamInfo::from)
            .map_err(describe)
    })
    .await
}

/// The subjects a stream currently holds messages on, with per-subject counts.
pub async fn list_stream_subjects(
    stream: String,
    filter: String,
) -> Result<Vec<SubjectCount>, Failure> {
    call(move || async move {
        jetstream::list_stream_subjects(stream, filter)
            .await
            .map(|counts| {
                counts
                    .into_iter()
                    .map(|c| SubjectCount {
                        subject: c.subject,
                        messages: c.count,
                    })
                    .collect()
            })
            .map_err(describe)
    })
    .await
}

/// Consumer configuration and state, without attaching to it.
pub async fn consumer_info(stream: String, consumer: String) -> Result<ConsumerInfo, Failure> {
    call(move || async move {
        jetstream::get_consumer_info(stream, consumer)
            .await
            .map(ConsumerInfo::from)
            .map_err(describe)
    })
    .await
}

/// Fetches a batch from an existing pull consumer and settles each message as
/// `settle` directs.
///
/// The consumer must have been provisioned out-of-band (the NATS CLI, or
/// deployment tooling): this interface deliberately exposes no stream or
/// consumer lifecycle.
pub async fn fetch(
    stream: String,
    consumer: String,
    batch: u32,
    max_bytes: Option<u64>,
    timeout_ms: Option<u32>,
    settle: Settle,
) -> Result<FetchedBatch, Failure> {
    let timeout_ms = clamp_timeout(timeout_ms, 5_000);
    call(move || async move {
        let handle = jetstream::open_pull_consumer(stream, consumer)
            .await
            .map_err(describe)?;

        let batch = match max_bytes {
            Some(max_bytes) => handle.fetch_with_limits(batch, max_bytes, timeout_ms).await,
            None => handle.fetch(batch, timeout_ms).await,
        }
        .map_err(describe)?;

        let stop = match batch.stop {
            jetstream::FetchStop::BatchFilled => "batch-filled",
            jetstream::FetchStop::Drained => "drained",
            jetstream::FetchStop::ByteLimit => "byte-limit",
        };

        let mut messages = Vec::with_capacity(batch.messages.len());
        for message in batch.messages {
            // Read everything off the handle before settling it: a settled
            // handle is retired, and `message()` borrows through it.
            let sequence = message.sequence();
            let delivery_count = message.delivery_count();
            let raw = message.message();

            let settled = match settle {
                Settle::None => "unsettled".to_string(),
                Settle::Ack => settle_result("acked", message.ack().await),
                Settle::Nak => settle_result("naked", message.nak(None).await),
                Settle::Term => settle_result("terminated", message.term().await),
            };

            messages.push(FetchedMessage {
                subject: raw.subject,
                sequence,
                delivery_count,
                payload: Payload::encode(raw.body),
                headers: decode_headers(raw.headers),
                settled,
            });
        }

        Ok(FetchedBatch { messages, stop })
    })
    .await
}

/// Reports how one message settled, keeping a per-message failure out of the
/// batch's way — the message was still delivered and is worth returning.
fn settle_result(done: &str, result: Result<(), types::NatsError>) -> String {
    match result {
        Ok(()) => done.to_string(),
        Err(err) => format!("settle failed: {}", describe(err)),
    }
}

/// Reads the latest entry for a key.
pub async fn kv_get(bucket: String, key: String) -> Result<Entry, Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle.get(key).await.map(Entry::from).map_err(describe)
    })
    .await
}

/// Writes a key, last-write-wins. Returns the new revision.
pub async fn kv_put(bucket: String, key: String, value: Vec<u8>) -> Result<u64, Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle.put(key, value).await.map_err(describe)
    })
    .await
}

/// Writes a key only if it is absent. Returns the new revision.
pub async fn kv_create(bucket: String, key: String, value: Vec<u8>) -> Result<u64, Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle.create(key, value).await.map_err(describe)
    })
    .await
}

/// Compare-and-swap on revision. Fails with `revision-mismatch` (carrying the
/// current revision) if the key moved on.
pub async fn kv_update(
    bucket: String,
    key: String,
    value: Vec<u8>,
    expected_revision: u64,
) -> Result<u64, Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle
            .update(key, value, expected_revision)
            .await
            .map_err(describe)
    })
    .await
}

/// Deletes a key, leaving a tombstone and keeping history.
pub async fn kv_delete(bucket: String, key: String) -> Result<(), Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle.delete(key).await.map_err(describe)
    })
    .await
}

/// Purges a key, removing its history as well.
pub async fn kv_purge(bucket: String, key: String) -> Result<(), Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle.purge(key).await.map_err(describe)
    })
    .await
}

/// Lists keys matching `filter`, up to a host cap of 1000.
pub async fn kv_keys(bucket: String, filter: String) -> Result<KeyPage, Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle
            .keys(filter)
            .await
            .map(|page| KeyPage {
                keys: page.keys,
                truncated: page.truncated,
            })
            .map_err(describe)
    })
    .await
}

/// All retained revisions of a key, oldest first.
pub async fn kv_history(bucket: String, key: String) -> Result<Vec<Entry>, Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle
            .history(key)
            .await
            .map(|entries| entries.into_iter().map(Entry::from).collect())
            .map_err(describe)
    })
    .await
}

/// Bucket state, read fresh from the server.
pub async fn kv_status(bucket: String) -> Result<BucketStatus, Failure> {
    call(move || async move {
        let handle = kv::open(bucket).await.map_err(describe)?;
        handle
            .status()
            .await
            .map(|status| BucketStatus {
                bucket: status.bucket,
                values: status.values,
                history: status.history,
                ttl_seconds: status.ttl_seconds,
                bytes: status.bytes,
            })
            .map_err(describe)
    })
    .await
}
