//! Exactly-once consume → transform → produce (read-process-write).
//!
//! Per batch: `transaction::begin` → produce outputs → `send-offsets` (the
//! input positions, one past the last processed record) → `commit`. Output
//! records and input offsets commit atomically; a crash anywhere replays the
//! batch and downstream `read_committed` readers never see the aborted half.
//!
//! Requirements:
//! - `transactional.id` in the workload's kafka `hostInterfaces[].config`
//!   (host-pinned; implies `enable.idempotence`). One stable ID per logical
//!   pipeline — it is what fences a restarted instance.
//! - Consumer `enable.auto.commit=false` (offsets travel in the transaction).
//! - `transaction.group.id` matches `consumer.group.id`.
//! - Downstream consumers set `isolation.level=read_committed`.
//!
//! A failed batch is aborted and the service restarts from committed offsets.
//! The host retires a native client after a fatal error.
//!
//! Environment (via `localResources.environment.config`): IN_TOPIC, OUT_TOPIC,
//! BATCH_SIZE (default 1).
//! Batches larger than one wait for the batch to fill or the stream to end.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kafka-transactional", generate_all });
    export!(Component);
}

use std::collections::BTreeMap;

use bindings::cosmonic::kafka::consumer::Consumer;
use bindings::cosmonic::kafka::types::{ConsumedRecord, PartitionOffset, ProduceRecord};
use bindings::exports::wasi::cli::run::Guest as RunGuest;
use bindings::transaction::{self, Transaction};

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

/// Your transform — pure function of the input record.
fn transform(rec: &ConsumedRecord) -> ProduceRecord {
    ProduceRecord {
        partition: None,
        key: rec.key.clone(),
        value: rec.value.clone(),
        headers: rec.headers.clone(),
        timestamp: None,
    }
}

/// Offsets to commit for a processed batch: one past the last record seen on
/// each input partition, carrying the leader epoch through for fencing.
fn commit_positions(batch: &[ConsumedRecord]) -> Vec<PartitionOffset> {
    let mut latest: BTreeMap<(String, i32), &ConsumedRecord> = BTreeMap::new();
    for rec in batch {
        let entry = latest
            .entry((rec.topic.clone(), rec.partition))
            .or_insert(rec);
        if rec.offset > entry.offset {
            *entry = rec;
        }
    }
    latest
        .into_values()
        .map(|rec| PartitionOffset {
            topic: rec.topic.clone(),
            partition: rec.partition,
            offset: rec.offset + 1,
            leader_epoch: rec.leader_epoch,
            metadata: None,
        })
        .collect()
}

impl RunGuest for Component {
    async fn run() -> Result<(), ()> {
        let in_topic = env("IN_TOPIC", "input");
        let out_topic = env("OUT_TOPIC", "output");
        let batch_size = batch_size();

        let consumer = Consumer::open().await.map_err(|_| ())?;
        consumer.subscribe(vec![in_topic]).await.map_err(|_| ())?;
        let (mut records, terminal) = consumer.records().await.map_err(|_| ())?;
        let mut batch: Vec<ConsumedRecord> = Vec::with_capacity(batch_size);
        while let Some(rec) = records.next().await {
            batch.push(rec);
            if batch.len() >= batch_size {
                process_batch(&out_topic, &batch).await?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            process_batch(&out_topic, &batch).await?;
        }
        terminal.await.map_err(|_| ())?;
        Ok(())
    }
}

async fn process_batch(out_topic: &str, batch: &[ConsumedRecord]) -> Result<(), ()> {
    let txn: Transaction = transaction::begin().await.map_err(|_| ())?;
    let outputs: Vec<ProduceRecord> = batch.iter().map(transform).collect();
    let outputs_ok = txn
        .send_batch(out_topic.to_string(), outputs)
        .await
        .is_ok_and(|outcomes| outcomes.into_iter().all(|outcome| outcome.is_ok()));
    if !outputs_ok || txn.send_offsets(commit_positions(batch)).await.is_err() {
        let _ = txn.abort().await;
        return Err(());
    }
    if txn.commit().await.is_err() {
        let _ = txn.abort().await;
        return Err(());
    }
    Ok(())
}
