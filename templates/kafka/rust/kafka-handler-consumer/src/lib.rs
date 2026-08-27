//! Handler-mode (push) Kafka consumer.
//!
//! The host dispatches records one at a time, sequentially, each on a fresh
//! instance — there is no state between records and nothing to keep alive.
//! Return value semantics (at-least-once):
//! - `Ok(None)`               — record handled; the host stores its offset.
//! - `Err(Transient(_))`      — redeliver: the host seeks back and retries.
//! - `Err(Permanent(_))`      — with `dead-letter.topic` configured the record
//!                              goes to the DLQ and the partition advances;
//!                              WITHOUT a DLQ the partition stalls forever —
//!                              always configure `dead-letter.topic`.
//! - panic / trap             — treated like Transient, but a record that traps
//!                              deterministically wedges its partition
//!                              (redelivery loops). Guard your parsing.
//!
//! Performance notes (measured on wasmCloud 2.8 / cosmonic:kafka 0.3.0):
//! - Plain dispatch sustains 10k+ records/s per component; the fresh-instance
//!   model is NOT the bottleneck.
//! - Do NOT open a `Producer` inside `handle`: a fresh client per record costs
//!   ~100 ms (measured ~1200x a plain dispatch). If you must produce per
//!   record, use the pull-service template instead — it holds one producer for
//!   its lifetime.
//! - `poolSize`/`maxConcurrency` do not apply to handler dispatch. Scale with
//!   partitions × replicas.
//! - The `topics` config key is BOTH the subscription list and the produce
//!   grant — a topic you add so you may produce to it is also a topic this
//!   handler gets subscribed to.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kafka-handler-consumer", generate_all });
    export!(Component);
}

use bindings::cosmonic::kafka::types::ConsumedRecord;
use bindings::exports::cosmonic::kafka::handler::{Guest as Handler, HandlerError};

struct Component;

impl Handler for Component {
    async fn handle(records: Vec<ConsumedRecord>) -> Result<Option<i64>, HandlerError> {
        for rec in &records {
            let Some(value) = rec.value.as_deref() else {
                continue; // tombstone
            };
            // Replace with your processing. Malformed input should be a
            // Permanent error (→ DLQ), never a panic.
            match std::str::from_utf8(value) {
                Ok(_text) => { /* process */ }
                Err(_) => {
                    return Err(HandlerError::Permanent(Some(
                        "value is not valid UTF-8".into(),
                    )))
                }
            }
        }
        Ok(None)
    }
}
