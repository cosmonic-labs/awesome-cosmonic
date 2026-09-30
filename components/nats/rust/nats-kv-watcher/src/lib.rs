//! KV watcher — react to every change in a JetStream KV bucket.
//!
//! The host owns the watch (`kv-watches: demo:>` on the binding, meaning every
//! key in the `demo` bucket) and calls this once per change. The entry carries
//! the key, the new value, its revision, and which operation produced it.
//!
//! Watches replay. A watcher starting fresh sees the current value of every
//! key before it sees anything new, and a redelivery can repeat one. Make the
//! reaction idempotent, and compare `revision` when order matters — the
//! revision is the bucket's own sequence, so a lower one is stale.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kv-watcher", generate_all });
    export!(Component);
}

use bindings::exports::wasmcloud::nats::kv_handler::Guest as KvHandler;
use bindings::wasmcloud::nats::jetstream;
use bindings::wasmcloud::nats::kv::{Entry, KvOperation};
use bindings::wasmcloud::nats::types::NatsMessage;

struct Component;

impl KvHandler for Component {
    async fn handle_event(bucket: String, entry: Entry) -> Result<(), String> {
        // Replace this with your own processing. Branch on the operation
        // rather than on whether there are bytes: a delete and a purge both
        // arrive with an empty value, and so does a put of an empty value.
        let what = match entry.operation {
            KvOperation::Put => format!("put bytes={}", entry.value.len()),
            KvOperation::Delete => "delete".to_string(),
            KvOperation::Purge => "purge".to_string(),
        };

        jetstream::publish(NatsMessage {
            subject: "done.demo.kv".to_string(),
            body: format!(
                "bucket={bucket};key={};revision={};{what}",
                entry.key, entry.revision
            )
            .into_bytes(),
            reply_to: None,
            headers: None,
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("receipt publish failed: {e:?}"))
    }
}
