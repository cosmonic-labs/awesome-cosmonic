//! `Metadata` and `OffsetFetch`, the last two APIs this plugin needed a client
//! library for.
//!
//! With these, `kafka-rust` is gone. It was doing only these two by the end —
//! every other API it offers is sent at a version some current broker refuses,
//! so `Produce`, `Fetch`, `ListOffsets`, and `OffsetCommit` were already
//! implemented here. Keeping a dependency for two small requests, when that
//! dependency's obsolescence caused every protocol bug in this project, was not
//! a good trade.
//!
//! Both are sent at versions every broker tested accepts: `Metadata` v0 (floor
//! v0 on Redpanda and Kafka 4.x alike) and `OffsetFetch` v1 (floor v1 on both —
//! v0 stored offsets in ZooKeeper and is long gone).

use std::collections::HashMap;

use crate::fetch::{BrokerConn, FetchError, Reader};

/// One partition's identity and where its leader is.
pub struct PartitionMeta {
    pub id: i32,
    /// Broker node id of the leader, or `-1` when the partition has none right
    /// now — mid-election, and not writable until it does.
    pub leader: i32,
}

/// A snapshot of the cluster: which brokers exist and which leads what.
///
/// A snapshot rather than a live view, so it goes stale. The fetch and produce
/// paths both re-request it when a broker answers `NOT_LEADER`, which is the
/// signal that leadership moved.
pub struct Cluster {
    /// Node id to `host:port`.
    pub brokers: HashMap<i32, String>,
    pub topics: HashMap<String, Vec<PartitionMeta>>,
}

impl Cluster {
    /// The address of the broker leading `partition`, if it has one.
    pub fn leader_addr(&self, topic: &str, partition: i32) -> Option<&str> {
        let leader = self
            .topics
            .get(topic)?
            .iter()
            .find(|p| p.id == partition)?
            .leader;
        self.brokers.get(&leader).map(String::as_str)
    }

    /// Partition ids of `topic` that currently have a leader. A partition
    /// without one cannot be produced to or fetched from, so offering it would
    /// only produce an error later.
    pub fn available_partitions(&self, topic: &str) -> Vec<i32> {
        self.topics
            .get(topic)
            .map(|ps| ps.iter().filter(|p| p.leader >= 0).map(|p| p.id).collect())
            .unwrap_or_default()
    }
}

impl BrokerConn {
    /// Fetch cluster metadata for every topic the broker knows.
    pub fn metadata(&mut self) -> Result<Cluster, FetchError> {
        let (correlation, stream) = self.next_request();
        let mut req = Vec::with_capacity(64);
        req.extend_from_slice(&3i16.to_be_bytes()); // Metadata
        req.extend_from_slice(&0i16.to_be_bytes()); // v0
        req.extend_from_slice(&correlation.to_be_bytes());
        crate::fetch::put_str(&mut req, "wasmcloud-kafka-plugin");
        // An empty topic array means "all topics" in v0.
        req.extend_from_slice(&0i32.to_be_bytes());

        let resp = crate::fetch::round_trip(stream, &req)?;
        let mut r = Reader::new(&resp);
        let got = r.i32()?;
        if got != correlation {
            return Err(FetchError::Protocol(format!(
                "correlation id mismatch: expected {correlation}, got {got}"
            )));
        }

        let mut brokers = HashMap::new();
        let broker_count = r.i32()?;
        for _ in 0..broker_count {
            let node_id = r.i32()?;
            let host = r.string()?;
            let port = r.i32()?;
            brokers.insert(node_id, format!("{host}:{port}"));
        }

        let mut topics = HashMap::new();
        let topic_count = r.i32()?;
        for _ in 0..topic_count {
            let error = r.i16()?;
            let name = r.string()?;
            let partition_count = r.i32()?;
            let mut partitions = Vec::with_capacity(partition_count.max(0) as usize);
            for _ in 0..partition_count {
                let _p_error = r.i16()?;
                let id = r.i32()?;
                let leader = r.i32()?;
                // Replicas and ISR: read to advance, not used — this plugin
                // never talks to a follower.
                let replicas = r.i32()?;
                for _ in 0..replicas.max(0) {
                    let _ = r.i32()?;
                }
                let isr = r.i32()?;
                for _ in 0..isr.max(0) {
                    let _ = r.i32()?;
                }
                partitions.push(PartitionMeta { id, leader });
            }
            // A topic-level error (e.g. it does not exist) is reported by
            // omitting it, so a later lookup fails with a clear "unknown topic"
            // rather than an opaque protocol code.
            if error == 0 {
                topics.insert(name, partitions);
            }
        }

        Ok(Cluster { brokers, topics })
    }

    /// The committed offsets of a consumer group for one topic.
    ///
    /// A partition the group has never committed reports `-1`, which the caller
    /// treats as "start from the beginning of what is retained".
    pub fn offset_fetch(
        &mut self,
        group: &str,
        topic: &str,
        partitions: &[i32],
    ) -> Result<Vec<(i32, i64)>, FetchError> {
        let (correlation, stream) = self.next_request();
        let mut req = Vec::with_capacity(64 + topic.len() + group.len());
        req.extend_from_slice(&9i16.to_be_bytes()); // OffsetFetch
        req.extend_from_slice(&1i16.to_be_bytes()); // v1: offsets in Kafka, not ZooKeeper
        req.extend_from_slice(&correlation.to_be_bytes());
        crate::fetch::put_str(&mut req, "wasmcloud-kafka-plugin");
        crate::fetch::put_str(&mut req, group);
        req.extend_from_slice(&1i32.to_be_bytes()); // one topic
        crate::fetch::put_str(&mut req, topic);
        req.extend_from_slice(&(partitions.len() as i32).to_be_bytes());
        for p in partitions {
            req.extend_from_slice(&p.to_be_bytes());
        }

        let resp = crate::fetch::round_trip(stream, &req)?;
        let mut r = Reader::new(&resp);
        let got = r.i32()?;
        if got != correlation {
            return Err(FetchError::Protocol(format!(
                "correlation id mismatch: expected {correlation}, got {got}"
            )));
        }

        let mut out = Vec::new();
        let topic_count = r.i32()?;
        for _ in 0..topic_count {
            let _name = r.string()?;
            let partition_count = r.i32()?;
            for _ in 0..partition_count {
                let partition = r.i32()?;
                let offset = r.i64()?;
                let _metadata = r.string()?;
                let error = r.i16()?;
                if error != 0 {
                    return Err(FetchError::Broker(error));
                }
                out.push((partition, offset));
            }
        }
        Ok(out)
    }
}
