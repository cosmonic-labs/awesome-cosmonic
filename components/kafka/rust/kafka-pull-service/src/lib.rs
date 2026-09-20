//! Pull-mode consume → transform → produce Service.
//!
//! One long-lived instance owns a consumer session. The host owns and reuses
//! the binding-scoped producer.
//!
//! Semantics: at-least-once. The record's offset is stored when it is handed
//! to this loop; `commit([])` commits stored positions after the batch's
//! outputs are acknowledged. A crash between produce and commit replays the
//! records. Transform failures go to the configured DLQ.
//!
//! Configuration comes from environment variables set in the workload
//! manifest (`localResources.environment.config`):
//!   IN_TOPIC (default input), OUT_TOPIC (default output),
//!   DLQ_TOPIC (default input.dlq), BATCH_SIZE (default 1).
//! Kafka client and group config comes only from the host-interface binding.
//! Batches larger than one wait for the batch to fill or the stream to end.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kafka-pull-service", generate_all });
    export!(Component);
}

use bindings::cosmonic::kafka::consumer::Consumer;
use bindings::cosmonic::kafka::producer;
use bindings::cosmonic::kafka::types::{
    ConsumedRecord, Error as KafkaError, ErrorCode, ProduceRecord,
};
use bindings::exports::wasi::cli::run::Guest as RunGuest;

struct Component;

/// Read one variable set by `localResources.environment.config`.
fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn batch_size() -> usize {
    env("BATCH_SIZE", "1")
        .parse()
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(1)
        .min(100)
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

async fn commit_stored(consumer: &Consumer) -> Result<(), ()> {
    match consumer.commit(Vec::new()).await {
        Ok(outcomes) => {
            for outcome in outcomes {
                match outcome.error {
                    None | Some(ErrorCode::NoError | ErrorCode::NoOffset) => {}
                    Some(code) => {
                        eprintln!(
                            "offset commit failed for {}-{}: {code:?}",
                            outcome.topic, outcome.partition
                        );
                        return Err(());
                    }
                }
            }
        }
        Err(KafkaError {
            code: ErrorCode::NoOffset,
            ..
        }) => {}
        Err(error) if error.fatal => {
            eprintln!("fatal offset commit error: {}", error.message);
            return Err(());
        }
        Err(error) if error.retriable => {
            eprintln!("deferring offset commit retry: {}", error.message);
        }
        Err(error) => {
            eprintln!("offset commit cannot be retried: {}", error.message);
            return Err(());
        }
    }
    Ok(())
}

async fn flush_outputs(
    consumer: &Consumer,
    out_topic: &str,
    pending: &mut Vec<ProduceRecord>,
) -> Result<(), ()> {
    if pending.is_empty() {
        return Ok(());
    }
    let outcomes = producer::send_batch(out_topic.to_string(), std::mem::take(pending))
        .await
        .map_err(|_| ())?;
    if outcomes.into_iter().any(|outcome| outcome.is_err()) {
        return Err(());
    }
    commit_stored(consumer).await
}

impl RunGuest for Component {
    async fn run() -> Result<(), ()> {
        let in_topic = env("IN_TOPIC", "input");
        let out_topic = env("OUT_TOPIC", "output");
        let dlq_topic = env("DLQ_TOPIC", "input.dlq");
        let batch = batch_size();

        let consumer = Consumer::open().await.map_err(|_| ())?;
        consumer.subscribe(vec![in_topic]).await.map_err(|_| ())?;
        let (mut records, terminal) = consumer.records().await.map_err(|_| ())?;
        let mut pending: Vec<ProduceRecord> = Vec::with_capacity(batch);
        while let Some(rec) = records.next().await {
            match transform(&rec) {
                Ok(out) => pending.push(out),
                Err(reason) => {
                    flush_outputs(&consumer, &out_topic, &mut pending).await?;
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
                    producer::send(dlq_topic.clone(), dead)
                        .await
                        .map_err(|_| ())?;
                    commit_stored(&consumer).await?;
                    continue;
                }
            }
            if pending.len() >= batch {
                flush_outputs(&consumer, &out_topic, &mut pending).await?;
            }
        }
        flush_outputs(&consumer, &out_topic, &mut pending).await?;
        terminal.await.map_err(|_| ())?;
        Ok(())
    }
}
