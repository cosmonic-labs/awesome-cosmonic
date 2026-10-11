//! Handler-mode (push) Kafka consumer.
//!
//! The host owns the consumer and calls `handle` with a batch of records from
//! one partition, in offset order — up to `handler.batch.size` (default 100)
//! and about 1 MiB. Calls may use different component instances.
//!
//! Return value semantics (at-least-once):
//! - `Ok(None)` handles the whole batch.
//! - `Ok(Some(offset))` handles through that record and retries the suffix.
//! - `Transient` retries from the first record.
//! - `Permanent` dead-letters the failing head record.
//! - A trap retries the head record and dead-letters it after five failures.
//!
//! Concurrency is one dispatch loop per assigned partition, capped at 64 per
//! component. `poolSize` and `maxConcurrency` bound elastic guest compute;
//! growing the pool does not add Kafka group members.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kafka-handler-consumer", generate_all });
    export!(Component);
}

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use bindings::cosmonic::kafka::types::ConsumedRecord;
use bindings::exports::cosmonic::kafka::handler::{Guest as Handler, HandlerError};

struct Component;

const HEARTBEAT_INTERVAL_MS: i64 = 10_000;

struct InstanceMetrics {
    id: u64,
    batches: u64,
    records: u64,
    started: bool,
    last_record_time_ms: Option<i64>,
}

impl InstanceMetrics {
    fn new() -> Self {
        Self {
            id: RandomState::new().hash_one(()),
            batches: 0,
            records: 0,
            started: false,
            last_record_time_ms: None,
        }
    }
}

thread_local! {
    static METRICS: RefCell<InstanceMetrics> = RefCell::new(InstanceMetrics::new());
}

fn observe_batch(records: &[ConsumedRecord]) {
    let record_time_ms = records.iter().filter_map(|rec| rec.timestamp).max();
    METRICS.with_borrow_mut(|metrics| {
        metrics.batches += 1;
        metrics.records += records.len() as u64;

        let heartbeat_due = !metrics.started
            || match (record_time_ms, metrics.last_record_time_ms) {
                (Some(now), Some(previous)) => {
                    now < previous || now.saturating_sub(previous) >= HEARTBEAT_INTERVAL_MS
                }
                (Some(_), None) => true,
                _ => false,
            };
        if heartbeat_due {
            metrics.started = true;
            if record_time_ms.is_some() {
                metrics.last_record_time_ms = record_time_ms;
            }
            eprintln!(
                "kafka handler heartbeat instance={:016x} batches={} records={} record_time_ms={:?}",
                metrics.id, metrics.batches, metrics.records, record_time_ms
            );
        }
    });
}

impl Handler for Component {
    async fn handle(records: Vec<ConsumedRecord>) -> Result<Option<i64>, HandlerError> {
        observe_batch(&records);

        // Preserve progress when a later record fails.
        let mut handled: Option<i64> = None;

        for rec in &records {
            let Some(value) = rec.value.as_deref() else {
                handled = Some(rec.offset);
                continue;
            };

            // Replace this with idempotent processing.
            let outcome = match std::str::from_utf8(value) {
                Ok(_text) => Ok(()),
                Err(_) => Err(HandlerError::Permanent(Some(
                    "value is not valid UTF-8".into(),
                ))),
            };

            match outcome {
                Ok(()) => handled = Some(rec.offset),
                // Return progress before reporting a later error.
                Err(e) => return handled.map_or(Err(e), |offset| Ok(Some(offset))),
            }
        }

        Ok(None)
    }
}
