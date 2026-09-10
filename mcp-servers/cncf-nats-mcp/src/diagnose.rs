//! The rules engine: numbers in, verdicts out.
//!
//! Every other module here answers "what is the value of X". This one answers
//! "is that bad, and what do I do about it" — which is the difference between
//! a query surface and a diagnostic one. A caller should not have to know that
//! `max_age: 0` together with `max_msgs: -1` means a stream grows until the
//! disk fills, or that `num_pending` rising between two samples is the only
//! way to tell a backlog from a busy consumer.
//!
//! ## Two sources, and an honest gap between them
//!
//! - **Monitoring** ([`crate::monitor`], when the operator enabled it). One
//!   `/jsz` request carries every stream's config *and* state *and* every
//!   consumer, so the full rule set runs across the whole server with no NATS
//!   grants at all.
//! - **The NATS binding** (always available). `stream_info` and
//!   `consumer_info` report state but not retention configuration, and there
//!   is no list call, so the caller must name the resources. The retention
//!   rules cannot run at all. [`Report::notes`] says so rather than quietly
//!   returning fewer findings.
//!
//! ## Why everything is sampled twice
//!
//! A single reading cannot distinguish a stream that holds 9,000 messages
//! because it is idle from one that holds 9,000 because it is filling at 900 a
//! minute, and cannot distinguish a consumer with a backlog from one that is
//! falling behind. Every rate and trend here comes from two readings separated
//! by `sample_ms`.

use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use crate::inventory::{self, Inventory, MonitorState};
use crate::monitor;
use crate::nats::{self, Failure};

/// Default gap between the two readings, in milliseconds.
const DEFAULT_SAMPLE_MS: u32 = 2_000;

/// Ceiling on the sampling window.
///
/// The bridge holds the request lock for the whole exchange, so the sampling
/// gap is a lease on this instance. The same reasoning bounds `timeout_ms` in
/// [`crate::nats`].
const MAX_SAMPLE_MS: u32 = 15_000;

/// Fraction of `max_ack_pending` at which a consumer is called saturated.
const ACK_SATURATION: f64 = 0.9;

/// Fractions of JetStream's storage limit that raise a warning / a critical.
const STORAGE_WARN: f64 = 0.75;
const STORAGE_CRITICAL: f64 = 0.90;

// ---------------------------------------------------------------------------
// Output shape
// ---------------------------------------------------------------------------

/// One thing worth telling the operator about.
///
/// The three fields after the summary are the point: `evidence` is the numbers
/// the verdict rests on (so a reader can disagree), and `remedy` is the action
/// (so nobody has to re-derive it).
#[derive(Debug, Serialize)]
pub struct Finding {
    /// `critical`, `warning`, or `info`.
    pub severity: &'static str,
    /// Stable machine-readable rule id; branch on this, not on `summary`.
    pub code: &'static str,
    /// What the finding is about — a stream, a consumer, or `server`.
    pub resource: String,
    /// One sentence stating the problem.
    pub summary: String,
    /// The measurements behind the verdict.
    pub evidence: Value,
    /// What to actually do.
    pub remedy: String,
}

impl Finding {
    fn rank(&self) -> u8 {
        match self.severity {
            "critical" => 0,
            "warning" => 1,
            _ => 2,
        }
    }
}

/// The result of one diagnostic pass.
#[derive(Debug, Serialize)]
pub struct Report {
    /// `monitoring` (whole server) or `binding` (only what was named).
    pub source: &'static str,
    /// Measured gap between the two readings.
    pub sample_ms: u64,
    /// True when no rule fired. Findings of severity `info` do not clear it.
    pub healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<Value>,
    pub streams_examined: usize,
    pub consumers_examined: usize,
    pub buckets_examined: usize,
    /// What this pass could and could not check, and what would unlock more.
    /// Present so a thin report cannot be mistaken for a clean one.
    pub capability: Value,
    pub findings: Vec<Finding>,
    /// What this pass could *not* check, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Normalized facts
// ---------------------------------------------------------------------------

/// A stream as the rules need it, from either source.
#[derive(Debug, Clone, Default)]
struct StreamFacts {
    name: String,
    subjects: Vec<String>,
    /// `limits`, `interest`, or `workqueue`. Empty from the binding, which
    /// does not report it.
    retention: String,
    /// `0` means no age bound.
    max_age_ns: u64,
    /// `-1` (or `0`) means no bound.
    max_msgs: i64,
    max_bytes: i64,
    /// Per-subject cap. This is what actually bounds a KV bucket (its
    /// `history`), so it counts as a bound even when the global caps are unset.
    max_msgs_per_subject: i64,
    messages: u64,
    bytes: u64,
    /// Sequence of the oldest message still held. Above 1 it proves retention
    /// has discarded from the head at some point — the only evidence of a
    /// bound available without reading configuration.
    first_seq: u64,
    last_seq: u64,
    consumer_count: u64,
    /// False when the source could not report retention configuration, which
    /// gates the retention rules.
    config_known: bool,
    /// False for a placeholder created only to hold a consumer whose stream
    /// was not itself sampled. Stream-level rules skip those: their zeroed
    /// counters are absence of data, not evidence of an idle stream.
    state_known: bool,
    consumers: Vec<ConsumerFacts>,
}

/// A consumer as the rules need it.
#[derive(Debug, Clone, Default)]
struct ConsumerFacts {
    name: String,
    num_pending: u64,
    num_ack_pending: u64,
    num_redelivered: u64,
    num_waiting: u64,
    /// `-1` or `0` means unlimited.
    max_ack_pending: i64,
    ack_wait_ns: u64,
    /// Attempts before the server gives up on a message. `-1`/`0` is
    /// unlimited; a positive value means failing messages are eventually
    /// dropped or dead-lettered.
    max_deliver: i64,
}

/// A KV bucket as the rules need it.
#[derive(Debug, Clone, Default)]
struct BucketFacts {
    name: String,
    values: u64,
    bytes: u64,
    ttl_seconds: u64,
    history: u8,
}

// ---------------------------------------------------------------------------
// JSON access helpers (total, never panicking)
// ---------------------------------------------------------------------------

fn u64_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn i64_at(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn str_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn strings_at(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Renders a duration in whole units an operator reads at a glance.
pub fn humanize_secs(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "unknown".to_owned();
    }
    let seconds = seconds as u64;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
        _ => format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3600),
    }
}

/// True when a JetStream limit field means "no bound".
fn unbounded(limit: i64) -> bool {
    limit <= 0
}

/// Whether a stream is the backing store for a KV bucket or object store.
///
/// These are streams, but they are not managed as streams: a KV bucket is
/// bounded by its per-key `history` (`max_msgs_per_subject`) and edited with
/// `nats kv`, so telling an operator to run `nats stream edit KV_x --max-age`
/// would be wrong advice about the wrong resource. The `kv_*` tools address
/// them by their bare bucket name.
fn is_backing_store(name: &str) -> bool {
    name.starts_with("KV_") || name.starts_with("OBJ_")
}

// ---------------------------------------------------------------------------
// Parsing: monitoring
// ---------------------------------------------------------------------------

/// Pulls every stream (with config, state, and consumers) out of a detailed
/// `/jsz` response.
fn streams_from_jsz(jsz: &Value) -> Vec<StreamFacts> {
    let mut out = Vec::new();
    let accounts = jsz
        .get("account_details")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for account in &accounts {
        let details = account
            .get("stream_detail")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for stream in &details {
            let config = stream.get("config").cloned().unwrap_or(Value::Null);
            let state = stream.get("state").cloned().unwrap_or(Value::Null);

            let consumers = stream
                .get("consumer_detail")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|consumer| {
                    let consumer_config = consumer.get("config").cloned().unwrap_or(Value::Null);
                    ConsumerFacts {
                        name: str_at(consumer, "name"),
                        num_pending: u64_at(consumer, "num_pending"),
                        num_ack_pending: u64_at(consumer, "num_ack_pending"),
                        num_redelivered: u64_at(consumer, "num_redelivered"),
                        num_waiting: u64_at(consumer, "num_waiting"),
                        max_ack_pending: i64_at(&consumer_config, "max_ack_pending"),
                        ack_wait_ns: u64_at(&consumer_config, "ack_wait"),
                        max_deliver: i64_at(&consumer_config, "max_deliver"),
                    }
                })
                .collect();

            out.push(StreamFacts {
                name: str_at(stream, "name"),
                subjects: strings_at(&config, "subjects"),
                retention: str_at(&config, "retention"),
                max_age_ns: u64_at(&config, "max_age"),
                max_msgs: i64_at(&config, "max_msgs"),
                max_bytes: i64_at(&config, "max_bytes"),
                max_msgs_per_subject: i64_at(&config, "max_msgs_per_subject"),
                messages: u64_at(&state, "messages"),
                bytes: u64_at(&state, "bytes"),
                first_seq: u64_at(&state, "first_seq"),
                last_seq: u64_at(&state, "last_seq"),
                consumer_count: u64_at(&state, "consumer_count"),
                config_known: true,
                state_known: true,
                consumers,
            });
        }
    }
    out
}

/// Condenses `/varz`, `/jsz` and `/healthz` into the header of a report.
fn server_summary(varz: &Value, jsz: &Value, healthz: &Value) -> Value {
    let jetstream_config = jsz.get("config").cloned().unwrap_or(Value::Null);
    json!({
        "version": str_at(varz, "version"),
        "server_name": str_at(varz, "server_name"),
        "uptime": str_at(varz, "uptime"),
        "health": str_at(healthz, "status"),
        "connections": u64_at(varz, "connections"),
        "subscriptions": u64_at(varz, "subscriptions"),
        "slow_consumers": u64_at(varz, "slow_consumers"),
        "max_payload_bytes": u64_at(varz, "max_payload"),
        "mem_bytes": u64_at(varz, "mem"),
        "cpu_percent": varz.get("cpu").and_then(Value::as_f64).unwrap_or(0.0),
        "jetstream": {
            "streams": u64_at(jsz, "streams"),
            "consumers": u64_at(jsz, "consumers"),
            "messages": u64_at(jsz, "messages"),
            "storage_bytes": u64_at(jsz, "storage"),
            "memory_bytes": u64_at(jsz, "memory"),
            "max_storage_bytes": u64_at(&jetstream_config, "max_storage"),
            "max_memory_bytes": u64_at(&jetstream_config, "max_memory"),
            "api_errors": u64_at(&jsz.get("api").cloned().unwrap_or(Value::Null), "errors"),
        },
    })
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Clamps a caller-supplied sampling window.
pub fn clamp_sample_ms(requested: Option<u32>) -> u32 {
    requested.unwrap_or(DEFAULT_SAMPLE_MS).clamp(200, MAX_SAMPLE_MS)
}

/// Whole-server pass over the monitoring endpoints.
pub async fn via_monitoring(sample_ms: u32) -> Result<Report, Failure> {
    let first = monitor::jsz(true).await?;
    let started = Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(sample_ms))).await;
    let second = monitor::jsz(true).await?;
    let elapsed = started.elapsed().as_secs_f64().max(0.001);

    // Best-effort context: a server that answers /jsz but not these is odd but
    // should not sink the whole pass.
    let varz = monitor::varz().await.unwrap_or(Value::Null);
    let healthz = monitor::healthz().await.unwrap_or(Value::Null);

    let before = streams_from_jsz(&first);
    let after = streams_from_jsz(&second);

    let mut findings = Vec::new();
    let storage_limit =
        u64_at(&second.get("config").cloned().unwrap_or(Value::Null), "max_storage");

    findings.extend(server_rules(&varz, &second, &healthz));
    findings.extend(stream_rules(&before, &after, elapsed, storage_limit));

    let consumers_examined = after.iter().map(|stream| stream.consumers.len()).sum();
    let buckets_examined = after
        .iter()
        .filter(|stream| is_backing_store(&stream.name))
        .count();
    Ok(finish(Report {
        source: "monitoring",
        sample_ms: (elapsed * 1000.0) as u64,
        healthy: true,
        server: Some(server_summary(&varz, &second, &healthz)),
        streams_examined: after.len() - buckets_examined,
        consumers_examined,
        buckets_examined,
        capability: inventory::capability("monitoring", MonitorState::Reachable),
        findings,
        notes: Vec::new(),
    }))
}

/// One reading of everything in `inventory`, through the NATS binding.
///
/// Names that cannot be read (outside the workload's grants, or absent) become
/// notes rather than errors: one bad entry in a list must not cost the caller
/// the whole report.
async fn sample_binding(inventory: &Inventory) -> (Vec<StreamFacts>, Vec<BucketFacts>, Vec<String>) {
    let mut streams: Vec<StreamFacts> = Vec::new();
    let mut buckets = Vec::new();
    let mut problems = Vec::new();

    for name in &inventory.streams {
        match nats::stream_info(name.clone()).await {
            Ok(info) => streams.push(StreamFacts {
                name: info.name,
                subjects: info.subjects,
                messages: info.messages,
                bytes: info.bytes,
                first_seq: info.first_sequence,
                last_seq: info.last_sequence,
                consumer_count: info.consumer_count,
                config_known: false,
                state_known: true,
                ..StreamFacts::default()
            }),
            Err(err) => problems.push(format!("stream {name:?} could not be read: {err}")),
        }
    }

    for (stream, consumer) in &inventory.consumers {
        match nats::consumer_info(stream.clone(), consumer.clone()).await {
            Ok(info) => {
                let facts = ConsumerFacts {
                    name: info.name,
                    num_pending: info.num_pending,
                    num_ack_pending: info.num_ack_pending,
                    num_redelivered: info.num_redelivered,
                    // The binding does not report waiting pulls; leaving it 0
                    // would make every consumer look idle, so the rule that
                    // uses it also requires a flat backlog.
                    num_waiting: 0,
                    max_ack_pending: info.max_ack_pending as i64,
                    ack_wait_ns: info.ack_wait_ms.saturating_mul(1_000_000),
                    max_deliver: info.max_deliver as i64,
                };
                match streams.iter_mut().find(|candidate| &candidate.name == stream) {
                    Some(target) => target.consumers.push(facts),
                    // The consumer was named without its stream: hold it in a
                    // placeholder so the consumer rules still run.
                    None => streams.push(StreamFacts {
                        name: stream.clone(),
                        config_known: false,
                        state_known: false,
                        consumers: vec![facts],
                        ..StreamFacts::default()
                    }),
                }
            }
            Err(err) => problems.push(format!(
                "consumer {consumer:?} on stream {stream:?} could not be read: {err}"
            )),
        }
    }

    for name in &inventory.buckets {
        match nats::kv_status(name.clone()).await {
            Ok(status) => buckets.push(BucketFacts {
                name: status.bucket,
                values: status.values,
                bytes: status.bytes,
                ttl_seconds: status.ttl_seconds,
                history: status.history,
            }),
            Err(err) => problems.push(format!("bucket {name:?} could not be read: {err}")),
        }
    }

    (streams, buckets, problems)
}

/// Pass over an inventory using the NATS binding alone.
///
/// Works with no monitoring endpoint. The retention rules cannot run — the
/// binding reports stream *state* but not *configuration* — so their place is
/// taken by [`pruning_rules`], which infers what it can from sequence
/// movement. The report's `capability` block says exactly what was skipped.
pub async fn via_binding(inventory: Inventory, sample_ms: u32) -> Result<Report, Failure> {
    let (before, buckets_before, _) = sample_binding(&inventory).await;

    let started = Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(sample_ms))).await;
    let elapsed = started.elapsed().as_secs_f64().max(0.001);

    let (after, buckets_after, problems) = sample_binding(&inventory).await;

    let mut findings = stream_rules(&before, &after, elapsed, 0);
    findings.extend(pruning_rules(&before, &after, elapsed));
    findings.extend(bucket_rules(&buckets_before, &buckets_after, elapsed));

    let mut notes = vec![
        "Retention configuration is not readable through the NATS binding \
         (max_age / max_msgs / max_bytes are absent from stream_info), so a \
         stream cannot be proven unbounded from here. The `no-pruning-observed` \
         findings are the closest inference available; set \
         MCP_NATS_MONITOR_URL to replace them with the real check."
            .to_owned(),
    ];
    if inventory.monitor == MonitorState::Unreachable {
        notes.push(
            "Server-wide telemetry is configured but the endpoint refused the \
             connection, so this pass fell back to the NATS binding. Whatever \
             is not named here — and every stream outside this workload's \
             grants — was not examined at all."
                .to_owned(),
        );
    }
    if inventory.source != "monitoring" {
        notes.push(format!(
            "Scope was {} — {} stream(s), {} consumer(s) and {} bucket(s). The \
             binding has no list call, so nothing else on this server was \
             looked at.",
            inventory.source,
            inventory.streams.len(),
            inventory.consumers.len(),
            inventory.buckets.len(),
        ));
    }
    notes.extend(problems);

    let consumers_examined = after.iter().map(|stream| stream.consumers.len()).sum();
    let streams_examined = after.iter().filter(|stream| stream.state_known).count();
    Ok(finish(Report {
        source: "binding",
        sample_ms: (elapsed * 1000.0) as u64,
        healthy: true,
        server: None,
        streams_examined,
        consumers_examined,
        buckets_examined: buckets_after.len(),
        capability: inventory::capability(inventory.source, inventory.monitor),
        findings,
        notes,
    }))
}

/// Sorts findings by severity and sets the `healthy` verdict.
fn finish(mut report: Report) -> Report {
    report.findings.sort_by_key(Finding::rank);
    report.healthy = !report
        .findings
        .iter()
        .any(|finding| finding.severity != "info");
    report
}

// ---------------------------------------------------------------------------
// Rules: server-wide
// ---------------------------------------------------------------------------

fn server_rules(varz: &Value, jsz: &Value, healthz: &Value) -> Vec<Finding> {
    let mut findings = Vec::new();

    let health = str_at(healthz, "status");
    if !health.is_empty() && health != "ok" {
        findings.push(Finding {
            severity: "critical",
            code: "server-unhealthy",
            resource: "server".to_owned(),
            summary: format!("the NATS server reports its own health as {health:?}, not \"ok\"."),
            evidence: healthz.clone(),
            remedy: "Read the server logs — /healthz only fails when the server \
                     itself believes something is wrong (JetStream not ready, an \
                     account or stream failing to recover)."
                .to_owned(),
        });
    }

    let slow = u64_at(varz, "slow_consumers");
    if slow > 0 {
        findings.push(Finding {
            severity: "critical",
            code: "slow-consumers",
            resource: "server".to_owned(),
            summary: format!(
                "the server has dropped clients {slow} time(s) for being slow consumers — \
                 those clients lost messages."
            ),
            evidence: json!({ "slow_consumers": slow }),
            remedy: "A slow consumer is a client that could not read as fast as the \
                     server wrote, so the server cut it off. Find it with \
                     nats_connections (look for high pending_bytes) and either speed \
                     up its handler, raise its pending limits, or move it to JetStream \
                     so flow control applies."
                .to_owned(),
        });
    }

    let config = jsz.get("config").cloned().unwrap_or(Value::Null);
    let used = u64_at(jsz, "storage");
    let limit = u64_at(&config, "max_storage");
    if limit > 0 {
        let ratio = used as f64 / limit as f64;
        if ratio >= STORAGE_WARN {
            let severity = if ratio >= STORAGE_CRITICAL {
                "critical"
            } else {
                "warning"
            };
            findings.push(Finding {
                severity,
                code: "jetstream-storage-pressure",
                resource: "server".to_owned(),
                summary: format!(
                    "JetStream file storage is {:.0}% of its limit ({used} of {limit} bytes).",
                    ratio * 100.0
                ),
                evidence: json!({
                    "storage_bytes": used,
                    "max_storage_bytes": limit,
                    "used_fraction": ratio,
                }),
                remedy: "Bound or trim the largest streams (see the per-stream \
                         findings), or raise the server's max_storage. When \
                         JetStream hits the limit, publishes to file-backed \
                         streams start failing."
                    .to_owned(),
            });
        }
    }

    if u64_at(varz, "connections") == 0 {
        findings.push(Finding {
            severity: "info",
            code: "no-connections",
            resource: "server".to_owned(),
            summary: "no clients are currently connected to this server.".to_owned(),
            evidence: json!({ "connections": 0 }),
            remedy: "Expected on an idle development server. On a live one it means \
                     every publisher and subscriber is gone."
                .to_owned(),
        });
    }

    findings
}

// ---------------------------------------------------------------------------
// Rules: per stream and consumer
// ---------------------------------------------------------------------------

fn stream_rules(
    before: &[StreamFacts],
    after: &[StreamFacts],
    elapsed_secs: f64,
    storage_limit: u64,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for stream in after {
        if !stream.state_known {
            continue;
        }
        let previous = before.iter().find(|candidate| candidate.name == stream.name);

        // Sequence delta, not message count: message count also moves when
        // retention discards from the tail, which would mask real ingest.
        let ingest_per_sec = previous
            .map(|prev| {
                stream.last_seq.saturating_sub(prev.last_seq) as f64 / elapsed_secs
            })
            .unwrap_or(0.0);
        let bytes_per_sec = previous
            .map(|prev| stream.bytes.saturating_sub(prev.bytes) as f64 / elapsed_secs)
            .unwrap_or(0.0);
        let ingesting = ingest_per_sec > 0.0;

        let is_unbounded = stream.config_known
            && !is_backing_store(&stream.name)
            && stream.retention == "limits"
            && stream.max_age_ns == 0
            && unbounded(stream.max_msgs)
            && unbounded(stream.max_bytes)
            && unbounded(stream.max_msgs_per_subject);

        if is_unbounded {
            let mut evidence = json!({
                "retention": stream.retention,
                "max_age": stream.max_age_ns,
                "max_msgs": stream.max_msgs,
                "max_bytes": stream.max_bytes,
                "messages": stream.messages,
                "bytes": stream.bytes,
                "msgs_per_sec": (ingest_per_sec * 100.0).round() / 100.0,
                "bytes_per_sec": (bytes_per_sec * 100.0).round() / 100.0,
            });

            // An ETA only means something when it is filling and we know the
            // ceiling it is filling toward.
            if bytes_per_sec > 0.0 && storage_limit > u64_at(&evidence, "bytes") {
                let headroom = storage_limit.saturating_sub(stream.bytes) as f64;
                let eta = headroom / bytes_per_sec;
                if let Some(map) = evidence.as_object_mut() {
                    map.insert("storage_limit_bytes".to_owned(), json!(storage_limit));
                    map.insert("time_to_fill_estimate".to_owned(), json!(humanize_secs(eta)));
                }
            }

            findings.push(Finding {
                severity: if ingesting { "critical" } else { "warning" },
                code: "unbounded-stream",
                resource: stream.name.clone(),
                summary: if ingesting {
                    format!(
                        "stream {:?} has no retention bound at all and is taking \
                         {:.1} msgs/sec — it grows until the disk fills.",
                        stream.name, ingest_per_sec
                    )
                } else {
                    format!(
                        "stream {:?} has no retention bound at all (max_age, \
                         max_msgs and max_bytes are all unset). It is idle now, but \
                         nothing will ever reclaim its space.",
                        stream.name
                    )
                },
                evidence,
                remedy: format!(
                    "Give it a bound: `nats stream edit {} --max-age=<duration>` \
                     (time-based), `--max-msgs=<n>`, or `--max-bytes=<n>`. If the \
                     stream is a replay buffer, max_age is usually the right one. \
                     Leaving all three unset is only safe when something else \
                     prunes the stream.",
                    stream.name
                ),
            });
        }

        // A KV bucket legitimately has no consumers: reads are direct gets.
        if stream.consumer_count == 0 && ingesting && !is_backing_store(&stream.name) {
            findings.push(Finding {
                severity: "warning",
                code: "stream-no-consumers",
                resource: stream.name.clone(),
                summary: format!(
                    "stream {:?} is taking {:.1} msgs/sec but has no consumers — \
                     nothing is reading what it stores.",
                    stream.name, ingest_per_sec
                ),
                evidence: json!({
                    "consumer_count": 0,
                    "msgs_per_sec": (ingest_per_sec * 100.0).round() / 100.0,
                    "messages": stream.messages,
                    "subjects": stream.subjects,
                }),
                remedy: "This is normal when the stream exists only for replay or \
                         audit, or when the live path is core NATS subscribers (who \
                         bypass the stream entirely) — in that case make sure \
                         retention is bounded. If something was supposed to be \
                         draining it, its consumer is missing."
                    .to_owned(),
            });
        }

        // A backing store's consumers belong to the KV/object layer — its
        // watchers are ephemeral and nothing "pulls" them in the sense these
        // rules mean. Advising an operator to go restart a worker for one
        // would be wrong about the wrong resource.
        if is_backing_store(&stream.name) {
            continue;
        }

        for consumer in &stream.consumers {
            findings.extend(consumer_rules(
                stream,
                consumer,
                previous.and_then(|prev| {
                    prev.consumers
                        .iter()
                        .find(|candidate| candidate.name == consumer.name)
                }),
                elapsed_secs,
            ));
        }
    }

    findings
}

fn consumer_rules(
    stream: &StreamFacts,
    consumer: &ConsumerFacts,
    previous: Option<&ConsumerFacts>,
    elapsed_secs: f64,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let resource = format!("{}/{}", stream.name, consumer.name);

    if let Some(prev) = previous {
        let growth = consumer.num_pending as i64 - prev.num_pending as i64;
        if growth > 0 && consumer.num_pending > 0 {
            let per_sec = growth as f64 / elapsed_secs;
            let drain_note = if per_sec > 0.0 {
                "it is losing ground, so the backlog has no natural end".to_owned()
            } else {
                String::new()
            };
            findings.push(Finding {
                severity: "critical",
                code: "consumer-backlog-growing",
                resource: resource.clone(),
                summary: format!(
                    "consumer {resource:?} is falling behind: {} pending and growing \
                     by {per_sec:.1}/sec — {drain_note}.",
                    consumer.num_pending
                ),
                evidence: json!({
                    "num_pending_before": prev.num_pending,
                    "num_pending_after": consumer.num_pending,
                    "growth_per_sec": (per_sec * 100.0).round() / 100.0,
                    "num_ack_pending": consumer.num_ack_pending,
                    "num_redelivered": consumer.num_redelivered,
                }),
                remedy: "The consumer cannot keep up with the stream. Add instances \
                         (raise the workload's poolSize), raise max-in-flight / \
                         max_ack_pending if the handler is idle waiting for slots, or \
                         find out why each message now takes longer than it used to."
                    .to_owned(),
            });
        }
    }

    if !unbounded(consumer.max_ack_pending) {
        let limit = consumer.max_ack_pending as f64;
        let ratio = consumer.num_ack_pending as f64 / limit;
        if ratio >= ACK_SATURATION {
            findings.push(Finding {
                severity: "warning",
                code: "consumer-ack-saturated",
                resource: resource.clone(),
                summary: format!(
                    "consumer {resource:?} is holding {} unacked messages against a \
                     max_ack_pending of {} ({:.0}%) — the server will stop delivering \
                     at the limit.",
                    consumer.num_ack_pending, consumer.max_ack_pending, ratio * 100.0
                ),
                evidence: json!({
                    "num_ack_pending": consumer.num_ack_pending,
                    "max_ack_pending": consumer.max_ack_pending,
                    "saturation": ratio,
                }),
                remedy: "Either the handler acks too slowly or the window is too \
                         small. Raise max_ack_pending if the handler is genuinely \
                         concurrent; otherwise speed up or parallelize the handler. \
                         At 100% delivery stops entirely until an ack frees a slot."
                    .to_owned(),
            });
        }
    }

    if consumer.num_redelivered > 0 {
        findings.push(Finding {
            severity: "warning",
            code: "consumer-redeliveries",
            resource: resource.clone(),
            summary: format!(
                "consumer {resource:?} has {} message(s) in redelivery — they were \
                 delivered and never acked.",
                consumer.num_redelivered
            ),
            evidence: json!({
                "num_redelivered": consumer.num_redelivered,
                "num_ack_pending": consumer.num_ack_pending,
                "ack_wait_ms": consumer.ack_wait_ns / 1_000_000,
            }),
            remedy: "Two usual causes: the handler is failing (or trapping) on those \
                     messages, or ack_wait is shorter than the handler's real \
                     runtime, so the server redelivers work that was still in \
                     progress. Compare ack_wait against actual handling time before \
                     raising max_deliver."
                .to_owned(),
        });
    }

    // Redeliveries against a finite attempt budget: these messages are on a
    // path to being dropped, which is a different problem from being slow.
    if consumer.num_redelivered > 0 && !unbounded(consumer.max_deliver) {
        findings.push(Finding {
            severity: "warning",
            code: "consumer-max-deliver-risk",
            resource: resource.clone(),
            summary: format!(
                "consumer {resource:?} is redelivering {} message(s) and gives up after \
                 {} attempts — messages that keep failing will be dropped.",
                consumer.num_redelivered, consumer.max_deliver
            ),
            evidence: json!({
                "num_redelivered": consumer.num_redelivered,
                "max_deliver": consumer.max_deliver,
                "ack_wait_ms": consumer.ack_wait_ns / 1_000_000,
            }),
            remedy: "Once a message hits max_deliver the server stops redelivering it: \
                     it is discarded, or routed to a dead-letter subject if one is \
                     configured. Fix the handler failure first — raising max_deliver \
                     without that just delays the loss. If these are poison messages, \
                     term them deliberately (jetstream_fetch with settle: \"term\") \
                     rather than letting the budget run out."
                .to_owned(),
        });
    }

    // A pull consumer with a backlog and nothing waiting to receive: no
    // client is asking for work, so the backlog will sit there indefinitely.
    if consumer.num_pending > 0
        && consumer.num_waiting == 0
        && consumer.num_ack_pending == 0
        && previous.map(|prev| prev.num_pending == consumer.num_pending) == Some(true)
    {
        findings.push(Finding {
            severity: "warning",
            code: "consumer-idle-with-backlog",
            resource: resource.clone(),
            summary: format!(
                "consumer {resource:?} has {} pending message(s) but nothing is \
                 pulling and the count did not move during sampling.",
                consumer.num_pending
            ),
            evidence: json!({
                "num_pending": consumer.num_pending,
                "num_waiting": 0,
                "num_ack_pending": 0,
            }),
            remedy: "No client is fetching from this consumer. Check that the \
                     workload meant to drain it is running and bound to this \
                     consumer name — a stopped worker looks exactly like this."
                .to_owned(),
        });
    }

    findings
}

// ---------------------------------------------------------------------------
// Rules: the binding-only substitute for the retention check
// ---------------------------------------------------------------------------

/// Infers what can be inferred about retention without reading configuration.
///
/// `stream_info` reports no `max_age` / `max_msgs` / `max_bytes`, so a stream
/// cannot be *proven* unbounded through the binding. What it does report is
/// `first_sequence`, and that carries real evidence: retention discards from
/// the head, so a first sequence still at 1 means nothing has ever been
/// removed, and a first sequence that does not move while the stream grows
/// means nothing is being removed now.
///
/// That is weaker than the real check and the finding says so. It exists
/// because the alternative — staying silent until an operator stands up a
/// monitoring port — hides the single most common way a JetStream deployment
/// fills a disk.
fn pruning_rules(
    before: &[StreamFacts],
    after: &[StreamFacts],
    elapsed_secs: f64,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for stream in after {
        if !stream.state_known || is_backing_store(&stream.name) || stream.messages == 0 {
            continue;
        }
        let Some(previous) = before.iter().find(|candidate| candidate.name == stream.name) else {
            continue;
        };

        let ingest_per_sec =
            stream.last_seq.saturating_sub(previous.last_seq) as f64 / elapsed_secs;
        // Retention discards from the head, so a first sequence that ADVANCES
        // is the only positive proof that something is reclaiming space.
        let head_advanced = stream.first_seq > previous.first_seq;
        let growing = ingest_per_sec > 0.0;

        if growing && head_advanced {
            // Taking messages and discarding them: retention is doing its job.
            continue;
        }
        if !growing {
            // Idle. No reclamation is expected, so the only thing worth saying
            // is that nothing has EVER been discarded — and even that is only
            // legible when the stream still starts at its first sequence.
            // (A stream whose head was moved by an explicit delete rather than
            // by retention starts above 1, which is why this is not the test
            // used for a growing stream.)
            if stream.first_seq > 1 {
                continue;
            }
        }

        let growing = ingest_per_sec > 0.0;
        findings.push(Finding {
            severity: if growing { "warning" } else { "info" },
            code: "stream-no-pruning-observed",
            resource: stream.name.clone(),
            summary: if growing {
                format!(
                    "stream {:?} is taking {:.1} msgs/sec and reclaimed nothing while \
                     it did — its oldest message is still sequence {}. If it has no \
                     retention bound it grows until the disk fills.",
                    stream.name, ingest_per_sec, stream.first_seq
                )
            } else {
                format!(
                    "stream {:?} holds {} message(s) and has never discarded one \
                     (first_sequence is {}).",
                    stream.name, stream.messages, stream.first_seq
                )
            },
            evidence: json!({
                "first_sequence": stream.first_seq,
                "last_sequence": stream.last_seq,
                "messages": stream.messages,
                "bytes": stream.bytes,
                "msgs_per_sec": (ingest_per_sec * 100.0).round() / 100.0,
                "head_advanced_during_sample": false,
                "retention_config_readable": false,
            }),
            remedy: format!(
                "This is an inference, not the real check: the NATS binding cannot \
                 read retention configuration, so a bounded stream that has simply \
                 not reached its limit yet looks identical to an unbounded one. \
                 Confirm with `nats stream info {}` — if max_age, max_msgs and \
                 max_bytes are all unset, give it one. Setting MCP_NATS_MONITOR_URL \
                 replaces this finding with a definitive `unbounded-stream` check.",
                stream.name
            ),
        });
    }

    findings
}

// ---------------------------------------------------------------------------
// Rules: KV buckets
// ---------------------------------------------------------------------------

/// A KV bucket is bounded per key by `history` but not in the number of keys,
/// so a bucket used as a log or a cache grows without limit unless its TTL
/// reclaims entries.
fn bucket_rules(
    before: &[BucketFacts],
    after: &[BucketFacts],
    elapsed_secs: f64,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for bucket in after {
        let Some(previous) = before.iter().find(|candidate| candidate.name == bucket.name) else {
            continue;
        };
        let growth = bucket.values as i64 - previous.values as i64;
        if growth <= 0 || bucket.ttl_seconds != 0 {
            continue;
        }
        let per_sec = growth as f64 / elapsed_secs;
        findings.push(Finding {
            severity: "warning",
            code: "kv-bucket-unbounded-growth",
            resource: bucket.name.clone(),
            summary: format!(
                "bucket {:?} gained {} value(s) during sampling ({per_sec:.1}/sec) and has \
                 no TTL — nothing will ever reclaim a key that stops being written.",
                bucket.name, growth
            ),
            evidence: json!({
                "values_before": previous.values,
                "values_after": bucket.values,
                "growth_per_sec": (per_sec * 100.0).round() / 100.0,
                "ttl_seconds": 0,
                "history": bucket.history,
                "bytes": bucket.bytes,
            }),
            remedy: "A bucket with no TTL is bounded per key by `history` but unbounded \
                     in the number of keys, so a growing key space grows forever. That \
                     is correct for a config store with a fixed key set and wrong for a \
                     cache or a log: give those a TTL with `nats kv edit <bucket> \
                     --ttl=<duration>`."
                .to_owned(),
        });
    }

    findings
}
