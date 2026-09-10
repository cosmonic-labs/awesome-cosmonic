//! KV Watcher — React to changes in a NATS KV bucket — put, delete, and purge events.
//!
//! WHEN TO USE THIS
//! You want a component invoked whenever a key changes — cache invalidation, config reload, projection updates, change-data-capture. The host maintains the watch; you just handle events.
//!
//! WHEN NOT TO
//! Do not use it as a work queue. Watch delivery follows KV semantics, not queue semantics, and a purge or a history-trimmed key can collapse several logical changes into one event.
//!
//! This implementation is extracted from the `nats-2.8-testing` campaign,
//! where it ran as part of 186 measured cells against the
//! wasmcloud:nats driver. See docs/tuning.md for the operational envelope and
//! the findings behind it.
//!
//! START HERE: the `handle_*` function below is the only thing you need to
//! change. Everything above it is binding glue.

wit_bindgen::generate!({
    path: "wit",
    world: "kv-watcher",
    generate_all,
});

use exports::wasmcloud::nats::kv_handler::Guest as KvHandler;
use wasmcloud::nats::jetstream;
use wasmcloud::nats::types::NatsMessage;
use wasmcloud::nats::kv::Entry;

struct Component;

fn field(body: &[u8], key: &str) -> Option<String> {
    let head = &body[..body.len().min(256)];
    let text = String::from_utf8_lossy(head);
    for part in text.split(';') {
        let (k, v) = part.split_once('=')?;
        if k == "pad" {
            return None;
        }
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

impl KvHandler for Component {
    async fn handle_event(bucket: String, entry: Entry) -> Result<(), String> {
        let run = field(&entry.value, "run").unwrap_or_else(|| "na".to_string());
        let receipt = NatsMessage {
            subject: format!("done.kv-watch.{run}"),
            body: format!(
                "bucket={bucket};key={};op={:?};rev={}",
                entry.key, entry.operation, entry.revision
            )
            .into_bytes(),
            reply_to: None,
            headers: None,
        };
        jetstream::publish(receipt)
            .await
            .map(|_| ())
            .map_err(|e| format!("receipt publish failed: {e:?}"))
    }
}

export!(Component);
