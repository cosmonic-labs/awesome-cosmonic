//! KV store — write records from a subject into a JetStream KV bucket.
//!
//! Each message on `demo.records.<key>` becomes one key in the bucket: the
//! part of the subject after the prefix is the key, and the body is the value.
//! A KV bucket is a JetStream stream underneath, so a write is durable and
//! every key keeps a revision history.
//!
//! `bucket-allow` on the binding is what grants access to the bucket, and it
//! is separate from `subject-allow` on purpose: being able to publish to a
//! subject does not grant reading or writing the bucket that captures it.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kv-store", generate_all });
    export!(Component);
}

use bindings::exports::wasmcloud::nats::core_handler::Guest as CoreHandler;
use bindings::wasmcloud::nats::types::NatsMessage;
use bindings::wasmcloud::nats::{jetstream, kv};

struct Component;

/// The bucket this component writes to. It has to exist already — a bucket is
/// created by an operator, not by the guest — and `bucket-allow` must name it.
const BUCKET: &str = "demo";

/// Subject prefix stripped to form the key: `demo.records.order-1` → `order-1`.
const SUBJECT_PREFIX: &str = "demo.records.";

impl CoreHandler for Component {
    async fn handle_message(msg: NatsMessage) -> Result<(), String> {
        // A subject with no key suffix, or one whose key would contain the
        // separator, has nothing to store. Core NATS will not redeliver, so
        // accept it rather than returning an error nobody acts on.
        let Some(key) = msg.subject.strip_prefix(SUBJECT_PREFIX) else {
            return Ok(());
        };
        if key.is_empty() || key.contains('.') {
            return Ok(());
        }

        let bucket = kv::open(BUCKET.to_string())
            .await
            .map_err(|e| format!("open bucket {BUCKET} failed: {e:?}"))?;

        // Replace this with your own processing. `put` overwrites blindly;
        // `create` fails if the key already exists, and `update` takes the
        // revision you read, which is how you get compare-and-swap.
        let revision = bucket
            .put(key.to_string(), msg.body.clone())
            .await
            .map_err(|e| format!("put {key} failed: {e:?}"))?;

        jetstream::publish(NatsMessage {
            subject: "done.demo.records".to_string(),
            body: format!("key={key};revision={revision}").into_bytes(),
            reply_to: None,
            headers: None,
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("receipt publish failed: {e:?}"))
    }
}
