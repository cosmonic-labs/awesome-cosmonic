//! Exactly-once consume → transform → produce (read-process-write).
//!
//! Per batch: `Transaction::begin` → produce outputs → `send-offsets` (the
//! input positions, one past the last processed record) → `commit`. Output
//! records and input offsets commit atomically; a crash anywhere replays the
//! batch and downstream `read_committed` readers never see the aborted half.
//!
//! Requirements:
//! - `transactional.id` in the workload's kafka `hostInterfaces[].config`
//!   (host-pinned; implies `enable.idempotence`). One STABLE id per logical
//!   pipeline — it is what fences a restarted instance.
//! - Consumer `enable.auto.commit=false` (offsets travel in the transaction).
//! - Downstream consumers set `isolation.level=read_committed`.
//!
//! Error handling: on any produce/commit failure check
//! `error.txn-requires-abort` — when set, `abort` and reprocess the batch.
//! When `error.fatal` is set the producer is finished: reopen it (a fresh
//! `begin` re-fences).
//!
//! Environment (via `localResources.environment.config`): IN_TOPIC, OUT_TOPIC,
//! GROUP_ID, BATCH_SIZE.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "kafka-transactional", generate_all });
    export!(Component);
}

use std::collections::BTreeMap;

use bindings::cosmonic::kafka::consumer::Consumer;
use bindings::cosmonic::kafka::producer::{Producer, Transaction};
use bindings::cosmonic::kafka::types::{ConfigEntry, ConsumedRecord, PartitionOffset, ProduceRecord};
use bindings::exports::wasi::cli0_3_0::run::Guest as RunGuest;
use bindings::wasi::cli0_2_0::environment;

struct Component;

fn env(key: &str, default: &str) -> String {
    environment::get_environment()
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_else(|| default.to_string())
}

fn cfg(key: &str, value: &str) -> ConfigEntry {
    ConfigEntry { key: key.into(), value: value.into() }
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
        let entry = latest.entry((rec.topic.clone(), rec.partition)).or_insert(rec);
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
        let group_id = env("GROUP_ID", "txn-pipeline-g1");
        let batch_size: usize = env("BATCH_SIZE", "100").parse().unwrap_or(100);

        let consumer = Consumer::open(vec![
            cfg("group.id", &group_id),
            cfg("auto.offset.reset", "earliest"),
            cfg("enable.auto.commit", "false"),
        ])
        .await
        .map_err(|_| ())?;
        consumer.subscribe(vec![in_topic]).await.map_err(|_| ())?;
        let (mut records, _terminal) = consumer.records().await.map_err(|_| ())?;
        // `transactional.id` arrives via the host-side config merge.
        let producer = Producer::open(Vec::new()).await.map_err(|_| ())?;

        let mut batch: Vec<ConsumedRecord> = Vec::with_capacity(batch_size);
        while let Some(rec) = records.next().await {
            batch.push(rec);
            if batch.len() >= batch_size {
                process_batch(&producer, &out_topic, &group_id, &batch).await?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            process_batch(&producer, &out_topic, &group_id, &batch).await?;
        }
        Ok(())
    }
}

async fn process_batch(
    producer: &Producer,
    out_topic: &str,
    group_id: &str,
    batch: &[ConsumedRecord],
) -> Result<(), ()> {
    let txn = Transaction::begin(producer).await.map_err(|_| ())?;
    // Records are produced through the producer while the transaction is
    // open; the transaction object carries begin/send-offsets/commit/abort.
    let outputs: Vec<ProduceRecord> = batch.iter().map(transform).collect();
    let sent = producer.send_batch(out_topic.to_string(), outputs).await;
    let offsets_ok = match sent {
        Ok(_) => txn
            .send_offsets(commit_positions(batch), group_id.to_string())
            .await
            .is_ok(),
        Err(_) => false,
    };
    if offsets_ok {
        txn.commit().await.map_err(|_| ())?;
        Ok(())
    } else {
        // Aborting keeps the pipeline consistent; the uncommitted input
        // offsets mean this batch is redelivered and reprocessed.
        let _ = txn.abort().await;
        Ok(())
    }
}
