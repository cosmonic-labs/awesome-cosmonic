//! Where this server learns what exists — and how well it can work at each
//! level of knowledge.
//!
//! The `wasmcloud:nats` binding has **no list call**. `stream_info` and
//! `kv_status` take a name and answer about it; nothing enumerates. So the
//! diagnostics face a bootstrapping problem: they cannot examine what they
//! cannot name.
//!
//! Rather than requiring the richest source, the server degrades in steps and
//! says which step it is on. In increasing order of capability:
//!
//! | Level | Source | What works |
//! |---|---|---|
//! | `caller-supplied` | names in the tool call | everything except discovery |
//! | `hinted` | `MCP_NATS_STREAMS` / `_CONSUMERS` / `_BUCKETS` | zero-argument calls over the hinted set |
//! | `monitoring` | `/jsz` (see [`crate::monitor`]) | the whole server, plus the retention rules |
//!
//! Each level is a superset of the one above it, nothing below `monitoring`
//! requires an operator to open anything, and every report carries a
//! [`capability`] block saying what was *not* checked and what would unlock it.
//! A caller should never have to guess whether a clean report means "healthy"
//! or "barely looked".
//!
//! ## Why the hints exist
//!
//! The hint variables restate names the operator already wrote into the
//! Workload's `stream-allow` / `bucket-allow` grants. That duplication is
//! deliberate and it is opt-in: a guest cannot read its own grants (they are a
//! host-side ceiling), and until the binding grows a list call this is the only
//! way a zero-argument `nats_diagnose` can know where to look without an
//! operator standing up a monitoring port. Hints grant nothing — a hinted name
//! outside the grants still gets denied, and [`crate::server`]'s
//! `nats_check_access` is the tool that shows the difference.

use serde_json::{json, Value};

use crate::monitor;

/// Comma-separated stream names to examine when a call names none.
const STREAMS_VAR: &str = "MCP_NATS_STREAMS";
/// Comma-separated `stream/consumer` pairs to examine when a call names none.
const CONSUMERS_VAR: &str = "MCP_NATS_CONSUMERS";
/// Comma-separated KV bucket names to examine when a call names none.
const BUCKETS_VAR: &str = "MCP_NATS_BUCKETS";

/// Whether server-wide telemetry was actually usable for this pass.
///
/// `is_enabled()` only says a URL is *configured*. A configured endpoint that
/// refuses connections — NATS restarted without `-m`, a port closed — must
/// degrade to the lower levels rather than fail the call, and must not then
/// advise the caller to "just use monitoring".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorState {
    /// No `MCP_NATS_MONITOR_URL` set.
    #[default]
    Unconfigured,
    /// Configured and answering.
    Reachable,
    /// Configured but the endpoint could not be reached.
    Unreachable,
}

/// What a diagnostic pass was able to look at, and how it found out.
#[derive(Debug, Default)]
pub struct Inventory {
    pub streams: Vec<String>,
    /// `(stream, consumer)` pairs.
    pub consumers: Vec<(String, String)>,
    pub buckets: Vec<String>,
    /// `caller-supplied`, `hinted`, `monitoring`, or `none`.
    pub source: &'static str,
    /// Whether monitoring was usable, so the capability block can tell
    /// "not configured" apart from "configured but down".
    pub monitor: MonitorState,
}

impl Inventory {
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty() && self.consumers.is_empty() && self.buckets.is_empty()
    }
}

/// Splits a comma-separated environment variable into trimmed, non-empty items.
fn csv_var(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Parses `"stream/consumer"` pairs, ignoring entries without a separator.
pub fn parse_consumer_refs(entries: &[String]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter_map(|entry| {
            entry
                .split_once('/')
                .map(|(stream, consumer)| (stream.trim().to_owned(), consumer.trim().to_owned()))
        })
        .filter(|(stream, consumer)| !stream.is_empty() && !consumer.is_empty())
        .collect()
}

/// The operator's hints, if any were configured.
pub fn hinted() -> Inventory {
    let streams = csv_var(STREAMS_VAR);
    let consumers = parse_consumer_refs(&csv_var(CONSUMERS_VAR));
    let buckets = csv_var(BUCKETS_VAR);
    let empty = streams.is_empty() && consumers.is_empty() && buckets.is_empty();
    Inventory {
        streams,
        consumers,
        buckets,
        source: if empty { "none" } else { "hinted" },
        // Callers that care set this from `probe()`; hints alone say nothing
        // about whether monitoring is up.
        monitor: MonitorState::Unconfigured,
    }
}

/// Whether any hint variable is set.
pub fn has_hints() -> bool {
    !hinted().is_empty()
}

/// Everything on the server, from the monitoring endpoint.
///
/// KV buckets are streams named `KV_<bucket>`, so one `/jsz` read seeds both
/// the stream list and the bucket list; consumers come from the same document.
pub async fn discovered() -> Option<Inventory> {
    let jsz = monitor::jsz(true).await.ok()?;
    let mut inventory = Inventory {
        source: "monitoring",
        monitor: MonitorState::Reachable,
        ..Inventory::default()
    };

    for account in jsz
        .get("account_details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for stream in account
            .get("stream_detail")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = stream.get("name").and_then(Value::as_str) else {
                continue;
            };
            match name.strip_prefix("KV_") {
                Some(bucket) => inventory.buckets.push(bucket.to_owned()),
                None => inventory.streams.push(name.to_owned()),
            }
            for consumer in stream
                .get("consumer_detail")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(consumer_name) = consumer.get("name").and_then(Value::as_str) {
                    inventory
                        .consumers
                        .push((name.to_owned(), consumer_name.to_owned()));
                }
            }
        }
    }
    Some(inventory)
}

/// Resolves what to examine: what the caller named, else hints, else discovery.
///
/// Caller arguments win because a caller asking about one stream should get
/// that stream, not the whole server.
pub async fn resolve(
    caller_streams: Vec<String>,
    caller_consumers: Vec<(String, String)>,
    caller_buckets: Vec<String>,
) -> Inventory {
    if !caller_streams.is_empty() || !caller_consumers.is_empty() || !caller_buckets.is_empty() {
        return Inventory {
            streams: caller_streams,
            consumers: caller_consumers,
            buckets: caller_buckets,
            source: "caller-supplied",
            monitor: probe().await,
        };
    }
    if monitor::is_enabled() {
        if let Some(discovered) = discovered().await {
            if !discovered.is_empty() {
                return discovered;
            }
        }
    }
    let mut hinted = hinted();
    hinted.monitor = probe().await;
    hinted
}

/// Cheap reachability check: is the configured endpoint actually answering?
pub async fn probe() -> MonitorState {
    if !monitor::is_enabled() {
        return MonitorState::Unconfigured;
    }
    match monitor::healthz().await {
        Ok(_) => MonitorState::Reachable,
        Err(_) => MonitorState::Unreachable,
    }
}

/// The capability block every diagnostic report carries.
///
/// Its job is to stop a thin report from reading like a clean bill of health:
/// it names the rules that could not run and the one change that would enable
/// them.
pub fn capability(source: &str, monitor: MonitorState) -> Value {
    let monitoring = source == "monitoring";
    let unlocks = if monitoring {
        "Full rule set. Every stream on the server was examined."
    } else if monitor == MonitorState::Unreachable {
        // Configured but down. Telling the caller to "use monitoring" here
        // would send them at something that just refused the connection.
        "Server-wide telemetry is configured on this workload but the endpoint \
         could not be reached, so the retention and server-wide rules did not \
         run and nothing could be discovered. Check that NATS is still running \
         with a monitoring port (`nats-server -m 8222`) — a restart without \
         `-m` takes it away — and that MCP_NATS_MONITOR_URL still names a \
         reachable address. Until then, pass names explicitly or set \
         MCP_NATS_STREAMS / MCP_NATS_CONSUMERS / MCP_NATS_BUCKETS."
    } else if monitor == MonitorState::Reachable {
        // Configured and up; this pass simply did not use it, because the
        // caller asked about specific names. Saying "nothing is configured"
        // here would send an operator to fix what is not broken.
        "This call was scoped to the names it was given, so only those were \
         examined and the retention rules did not run. Monitoring IS configured \
         and reachable on this workload: call again with no \
         streams/consumers/buckets to examine every stream on the server with \
         the full rule set."
    } else if has_hints() {
        "Running on the NATS binding alone, which is a supported configuration \
         and needs no HTTP endpoint. Two classes of check are simply not \
         available on this path: retention configuration (max_age / max_msgs / \
         max_bytes) is not exposed by the binding, so `unbounded-stream` is \
         replaced by the `stream-no-pruning-observed` inference; and \
         server-wide state (slow consumers, connection counts, storage \
         pressure) has no binding equivalent at all. Scope is the streams, \
         consumers and buckets named here or in MCP_NATS_STREAMS / \
         MCP_NATS_CONSUMERS / MCP_NATS_BUCKETS."
    } else {
        "Nothing is configured for discovery. Set MCP_NATS_STREAMS / \
         MCP_NATS_CONSUMERS / MCP_NATS_BUCKETS so zero-argument calls know \
         where to look, or MCP_NATS_MONITOR_URL to examine the whole server \
         and enable the retention rules."
    };

    json!({
        "level": source,
        // The binding reports stream state but not configuration, so nothing
        // below `monitoring` can read max_age / max_msgs / max_bytes.
        "retention_rules": monitoring,
        "server_wide_rules": monitoring,
        // Whether this deployment can enumerate at all — not whether this
        // particular call needed to.
        "discovery_available": monitoring || monitor == MonitorState::Reachable || has_hints(),
        "monitoring": match monitor {
            MonitorState::Unconfigured => "unconfigured",
            MonitorState::Reachable => "reachable",
            MonitorState::Unreachable => "configured-but-unreachable",
        },
        "unlocks": unlocks,
    })
}
