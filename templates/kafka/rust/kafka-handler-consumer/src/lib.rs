//! Handler-mode (push) Kafka consumer.
//!
//! The host owns the consumer and calls `handle` with a batch of records from
//! one partition, in offset order — up to `handler.batch.size` (default 100)
//! and about 1 MiB. There is no state between calls and nothing to keep alive.
//!
//! Return value semantics (at-least-once):
//! - `Ok(None)`               — the whole batch was handled; the host stores
//!                              past its last record.
//! - `Ok(Some(offset))`       — handled through that record and no further.
//!                              The host stores past it and redelivers the
//!                              rest. `offset` must be a record's own offset
//!                              from this batch, not the next one to read.
//! - `Err(Transient(_))`      — none of it was handled: the host rewinds to
//!                              the batch's first record and redelivers,
//!                              indefinitely.
//! - `Err(Permanent(_))`      — on a single-record batch the record goes to
//!                              `dead-letter.topic` and the partition
//!                              advances. On a longer batch the host rewinds
//!                              and redelivers the first record alone, so the
//!                              verdict lands on one record rather than many.
//! - panic / trap             — treated like Transient, except that five
//!                              consecutive failures on the same record
//!                              dead-letter it, so a deterministically
//!                              trapping record cannot wedge its partition.
//!
//! Prefer `Ok(Some(..))` over an error once any record has succeeded: it is
//! what keeps the work already done. That is what the loop below does.
//!
//! Notes:
//! - Do NOT open a `Producer` inside `handle` for a topic this component
//!   produces to repeatedly if you can avoid it. An `open` with no config of
//!   its own returns the binding's shared client rather than building one, so
//!   it is cheap — but the pull-service template is still the better shape for
//!   consume-transform-produce.
//! - Concurrency is one dispatch loop per assigned partition. `poolSize` keeps
//!   instances warm for those calls and `maxConcurrency` bounds how many share
//!   one instance; total throughput is capped by partitions × replicas.
//! - `handler.topics` is the subscription. `topics` is the separate grant for
//!   this component's own producer/consumer calls, so subscribing to an input
//!   no longer forces you to grant it as an output.

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
        // The last record handled, so a failure part way through keeps the
        // work already done instead of replaying the whole batch.
        let mut handled: Option<i64> = None;

        for rec in &records {
            let Some(value) = rec.value.as_deref() else {
                handled = Some(rec.offset); // tombstone: nothing to do
                continue;
            };

            // Replace with your processing. Malformed input should be a
            // Permanent error (→ DLQ), never a panic.
            let outcome = match std::str::from_utf8(value) {
                Ok(_text) => Ok(()),
                Err(_) => Err(HandlerError::Permanent(Some(
                    "value is not valid UTF-8".into(),
                ))),
            };

            match outcome {
                Ok(()) => handled = Some(rec.offset),
                // Report progress rather than the error when earlier records
                // in this batch succeeded: the host stores past them and
                // redelivers from this one, which then arrives alone and gets
                // a verdict of its own.
                Err(e) => return handled.map_or(Err(e), |offset| Ok(Some(offset))),
            }
        }

        Ok(None)
    }
}
