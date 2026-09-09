//! Pull-mode consume → transform → produce Service.
//!
//! One long-lived instance owns one consumer and ONE long-lived producer —
//! the pattern that avoids per-record client bootstrap (~100 ms each,
//! measured; see the handler template's notes) and lets `send-batch` batch.
//!
//! Semantics: at-least-once. The record's offset is stored when it is handed
//! to this loop; `commit([])` commits stored positions after the batch's
//! outputs are acknowledged. Crash between produce and commit ⇒ reprocessing,
//! never loss. Records that fail your processing go to YOUR dlq topic — pull
//! mode owns its own dead-lettering.
//!
//! Configuration comes from environment variables set in the workload
//! manifest (`localResources.environment.config`):
//!   IN_TOPIC (default input), OUT_TOPIC (default output),
//!   DLQ_TOPIC (default input.dlq), GROUP_ID, BATCH_SIZE (default 100).
//! Kafka connection config (`bootstrap.servers`, ...) is merged in by the
//! host from `hostInterfaces[].config` and cannot be overridden here.
//!
//! Throughput note: BATCH_SIZE trades latency for throughput. Per-record
//! send+await measured ~126 records/s (one broker round trip each);
//! batches of 100+ move tens of thousands per second. With low, bursty
//! traffic a partial batch waits for the next record — set BATCH_SIZE=1
//! if end-to-end latency matters more than throughput.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kafka-pull-service", generate_all });
    export!(Component);
}

use bindings::cosmonic::kafka::consumer::Consumer;
use bindings::cosmonic::kafka::producer::Producer;
use bindings::cosmonic::kafka::types::{ConfigEntry, ConsumedRecord, ProduceRecord};
use bindings::exports::wasi::cli::run::Guest as RunGuest;

struct Component;

/// Read one variable set by `localResources.environment.config`.
///
/// `std::env` rather than a generated `wasi:cli/environment` binding: the
/// wasip2 target lowers it to the same interface, and keeping it out of the
/// world is what lets every import there be 0.3.0.
fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn cfg(key: &str, value: &str) -> ConfigEntry {
    ConfigEntry { key: key.into(), value: value.into() }
}

/// Your transform. Return `Ok` with the output record, or `Err` with a reason
/// to send the input to the DLQ.
fn transform(rec: &ConsumedRecord) -> Result<ProduceRecord, String> {
    Ok(ProduceRecord {
        partition: None,
        key: rec.key.clone(),
        value: rec.value.clone(),
        headers: rec.headers.clone(),
        timestamp: None,
    })
}

impl RunGuest for Component {
    async fn run() -> Result<(), ()> {
        let in_topic = env("IN_TOPIC", "input");
        let out_topic = env("OUT_TOPIC", "output");
        let dlq_topic = env("DLQ_TOPIC", "input.dlq");
        let group_id = env("GROUP_ID", "pull-service-g1");
        let batch: usize = env("BATCH_SIZE", "100").parse().unwrap_or(100);

        let consumer = Consumer::open(vec![
            cfg("group.id", &group_id),
            cfg("auto.offset.reset", "earliest"),
            cfg("enable.auto.commit", "false"),
        ])
        .await
        .map_err(|_| ())?;
        consumer.subscribe(vec![in_topic]).await.map_err(|_| ())?;
        let (mut records, _terminal) = consumer.records().await.map_err(|_| ())?;
        let producer = Producer::open(Vec::new()).await.map_err(|_| ())?;

        let mut pending: Vec<ProduceRecord> = Vec::with_capacity(batch);
        while let Some(rec) = records.next().await {
            match transform(&rec) {
                Ok(out) => pending.push(out),
                Err(reason) => {
                    let mut headers = rec.headers.clone();
                    headers.push(bindings::cosmonic::kafka::types::Header {
                        key: "x-dlq-reason".into(),
                        value: Some(reason.into_bytes()),
                    });
                    let dead = ProduceRecord {
                        partition: None,
                        key: rec.key.clone(),
                        value: rec.value.clone(),
                        headers,
                        timestamp: None,
                    };
                    let _ = producer.send(dlq_topic.clone(), dead).await;
                }
            }
            if pending.len() >= batch {
                if producer
                    .send_batch(out_topic.clone(), std::mem::take(&mut pending))
                    .await
                    .is_ok()
                {
                    let _ = consumer.commit(Vec::new()).await;
                }
            }
        }
        // Stream ended (shutdown): flush the partial batch, then commit.
        if !pending.is_empty()
            && producer
                .send_batch(out_topic.clone(), std::mem::take(&mut pending))
                .await
                .is_ok()
        {
            let _ = consumer.commit(Vec::new()).await;
        }
        Ok(())
    }
}
