//! Kafka capability as a wasmCloud host component plugin.
//!
//! The plugin is instantiated once into a long-lived, host-scoped store and
//! serves every workload that imports its interfaces. That lifetime is the
//! entire point: a Kafka client is stateful — TCP connections to every broker,
//! cached topic metadata, partition leadership, consumer-group membership — and
//! none of it survives a per-request component instance. Holding it here is
//! what lets the workloads stay ephemeral.
//!
//! Transport is the native Kafka wire protocol, spoken directly: every API
//! this plugin uses is encoded in `src/{produce,fetch,meta,group}.rs` over
//! `std::net`, which lowers to `wasi:sockets` on `wasm32-wasip2`. No client
//! library — the one pure-Rust client that builds for this target sends four
//! of these APIs at versions current brokers refuse. No C, no threads, no
//! OpenSSL — see README for why `rdkafka` cannot be used here.
//!
//! Every exported function is `async` for the ABI's sake, not because anything
//! here yields: a plugin's capabilities are installed on a caller's linker as
//! concurrent host functions, and a sync-declared one cannot bind. The bodies
//! are blocking client calls, so each future completes on its first poll.

mod bindings {
    #![allow(unsafe_code)]
    wit_bindgen::generate!({ world: "kafka-plugin", generate_all });
}

mod fetch;
mod group;
mod meta;
mod produce;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use fetch::{BrokerConn, FetchError, FetchedRecord, ERR_NOT_LEADER, ERR_OFFSET_OUT_OF_RANGE};
use group::Membership;
use meta::Cluster;

use bindings::cosmonic::kafka::handler;
use bindings::cosmonic::kafka::types as handler_types;
use bindings::exports::cosmonic::kafka::consumer::Guest as ConsumerGuest;
use bindings::exports::cosmonic::kafka::producer::Guest as ProducerGuest;
use bindings::exports::cosmonic::kafka::types::{KafkaError, KafkaRecord, ProduceAck};
use bindings::exports::wasi::cli::run::Guest as RunGuest;
use bindings::wasi::clocks::monotonic_clock;
use bindings::wasi::config::store;
use bindings::wasi::logging::logging::{log, Level};
use bindings::wasmcloud::host::workload_call as workload;

/// Comma-separated `host:port` list. Required — there is no sensible default
/// for someone else's cluster, so a missing value is a configuration error
/// surfaced on the first call rather than a silent connection to localhost.
const CFG_BROKERS: &str = "bootstrap-servers";
/// Consumer group the plugin joins on behalf of every workload that polls it.
const CFG_GROUP: &str = "consumer-group";
/// Comma-separated topics the consumer subscribes to.
const CFG_TOPICS: &str = "topics";
/// How many replicas must have the record before a produce is acknowledged:
/// `all` (default), `one`, or `none`.
const CFG_ACKS: &str = "producer-acks";
/// Compression applied to produced records: `none` (default), `gzip`, or
/// `snappy`.
const CFG_COMPRESSION: &str = "producer-compression";
/// `on` to run the dispatch loop; anything else leaves the plugin pull-only.
const CFG_TRIGGER: &str = "trigger";
/// Records per dispatched batch.
const CFG_TRIGGER_BATCH: &str = "trigger-batch-size";
/// How many partitions may have a batch in flight at once.
const CFG_TRIGGER_INFLIGHT: &str = "trigger-max-inflight";
/// How many times a batch is redelivered before it is dead-lettered.
const CFG_TRIGGER_MAX_ATTEMPTS: &str = "trigger-max-attempts";
/// Topic that dead-lettered records are written to. Defaults to the source
/// topic with `.dlq` appended.
const CFG_TRIGGER_DLQ: &str = "trigger-dlq-topic";
/// `group` (default) to join the consumer group and be assigned a disjoint
/// slice of the partitions, or `static` to take every partition unconditionally.
const CFG_ASSIGNMENT: &str = "partition-assignment";
/// How long the coordinator waits for a heartbeat before evicting this member
/// and reassigning its partitions. Bounded by the broker's
/// `group.min/max.session.timeout.ms`.
const CFG_SESSION_TIMEOUT: &str = "session-timeout-ms";
/// Java's default, and comfortably above the heartbeat interval derived from it.
const DEFAULT_SESSION_TIMEOUT_MS: i32 = 45_000;

const DEFAULT_GROUP: &str = "wasmcloud-kafka-plugin";

/// Durability of a produce, as the broker defines it.
///
/// Defaults to `All` rather than the more common `One` because `send` returns a
/// `produce-ack` to the caller, and a caller that has been handed an offset has
/// been told the record is safe. Under `One` that is not true: the leader
/// acknowledges before its followers have the record, so a leader failure in
/// that window loses a record the workload was told had landed. Silently
/// under-delivering on an ack this interface already gave is worse than the
/// latency of waiting for the in-sync replicas.
///
/// `All` is only as strong as the topic's `min.insync.replicas`; on a
/// single-broker cluster it is exactly `One`.
fn required_acks() -> Result<i16, KafkaError> {
    match config(CFG_ACKS)?.as_deref().map(str::trim) {
        // -1 is "all in-sync replicas"; 1 is the leader alone; 0 is
        // fire-and-forget, where the broker sends no response at all.
        None | Some("") | Some("all") => Ok(-1),
        Some("one") => Ok(1),
        Some("none") => Ok(0),
        Some(other) => Err(KafkaError::NotConfigured(format!(
            "config key '{CFG_ACKS}' is '{other}'; expected one of all, one, none"
        ))),
    }
}

/// Compression for produced records.
///
/// Defaults to `none` because it is the choice that cannot surprise anyone: a
/// consumer too old to decode the codec fails on read rather than at produce
/// time. Set it for a topic that carries volume — the codecs here are the same
/// ones the fetch path already decodes, so records this plugin writes are
/// records it can read back.
fn compression() -> Result<i16, KafkaError> {
    match config(CFG_COMPRESSION)?.as_deref().map(str::trim) {
        None | Some("") | Some("none") => Ok(0),
        Some("gzip") => Ok(1),
        Some("snappy") => Ok(2),
        Some(other) => Err(KafkaError::NotConfigured(format!(
            "config key '{CFG_COMPRESSION}' is '{other}'; expected one of none, gzip, snappy"
        ))),
    }
}

/// Read one key from the plugin's bind-time config. An absent key is `None`
/// rather than an error, so each caller decides whether it had a default.
fn config(key: &str) -> Result<Option<String>, KafkaError> {
    store::get(key)
        .map_err(|e| KafkaError::NotConfigured(format!("could not read config key '{key}': {e:?}")))
}

/// Broker list, parsed once. Held separately from the clients because both the
/// producer and the consumer need it and neither owns it.
///
/// Not `get_or_init`: a config read can fail, and caching a failure as an empty
/// broker list would make every later call report a missing key long after the
/// key was there. Only a usable list is cached.
fn brokers() -> Result<&'static [String], KafkaError> {
    static BROKERS: OnceLock<Vec<String>> = OnceLock::new();
    if let Some(hosts) = BROKERS.get() {
        return Ok(hosts);
    }
    let hosts = split_csv(&config(CFG_BROKERS)?.unwrap_or_default());
    if hosts.is_empty() {
        return Err(KafkaError::NotConfigured(format!(
            "config key '{CFG_BROKERS}' is unset or empty; set it on the plugin to a \
             comma-separated host:port list"
        )));
    }
    Ok(BROKERS.get_or_init(|| hosts))
}

fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A poisoned lock means a previous call trapped mid-operation and the client's
/// internal state is untrustworthy. Recovering the guard and continuing would
/// risk sending on a half-written connection, so surface it instead.
fn poisoned<T>(_: PoisonError<T>) -> KafkaError {
    KafkaError::Connection(
        "kafka client state was poisoned by an earlier failure; restart the plugin".to_owned(),
    )
}

// ---------------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------------

struct Component;

/// Broker connections used for producing, one per broker address, held across
/// calls. Separate from the consumer's connections so a slow poll and a produce
/// do not queue behind each other on one socket.
static PRODUCER_CONNS: Mutex<Option<HashMap<String, BrokerConn>>> = Mutex::new(None);

/// Metadata client, for partition counts and leader addresses.
///
/// `kafka-rust` still does this correctly against a modern broker — it is only
/// its `Produce` and `Fetch` that are too old — so the parts that work are
/// still used.
static PRODUCER_META: Mutex<Option<Cluster>> = Mutex::new(None);

/// How long the broker may take to satisfy `acks` before it gives up.
const PRODUCE_TIMEOUT: Duration = Duration::from_secs(5);

/// Which partition a record goes to: the key's murmur2 hash when there is one,
/// so the same key keeps its ordering, and round-robin otherwise.
/// A record waiting to be encoded: `(key, value)`.
type PendingRecord = (Option<Vec<u8>>, Vec<u8>);

fn choose_partition(cluster: &Cluster, topic: &str, key: Option<&[u8]>) -> Result<i32, KafkaError> {
    let ids = cluster.available_partitions(topic);
    if ids.is_empty() {
        return Err(KafkaError::UnknownTopic(format!(
            "topic '{topic}' has no available partitions"
        )));
    }
    match key {
        Some(k) => {
            let idx = produce::partition_for_key(k, ids.len() as i32) as usize;
            Ok(ids[idx % ids.len()])
        }
        None => {
            static NEXT: Mutex<usize> = Mutex::new(0);
            let mut next = NEXT.lock().map_err(poisoned)?;
            let id = ids[*next % ids.len()];
            *next = next.wrapping_add(1);
            Ok(id)
        }
    }
}

/// Get or open a connection to one broker.
fn connect<'a>(
    conns: &'a mut HashMap<String, BrokerConn>,
    addr: &str,
) -> Result<&'a mut BrokerConn, KafkaError> {
    if !conns.contains_key(addr) {
        let conn = BrokerConn::connect(addr).map_err(|e| fetch_error(e, addr))?;
        conns.insert(addr.to_owned(), conn);
    }
    conns
        .get_mut(addr)
        .ok_or_else(|| KafkaError::Connection(format!("connection to {addr} vanished")))
}

fn map_produce_error(e: produce::ProduceError, addr: &str) -> KafkaError {
    match e {
        produce::ProduceError::Io(io) => KafkaError::Connection(format!("{addr}: {io}")),
        produce::ProduceError::Broker(3) => {
            KafkaError::UnknownTopic("broker reports unknown topic or partition".to_owned())
        }
        produce::ProduceError::Broker(7) => KafkaError::TimedOut,
        produce::ProduceError::Broker(code) => {
            KafkaError::Protocol(format!("broker returned error code {code}"))
        }
        produce::ProduceError::Protocol(msg) => KafkaError::Protocol(msg),
    }
}

/// Publish `records` to one topic, splitting them across partitions the way
/// their keys dictate and sending one `Produce` request per partition.
fn send_records(
    topic: &str,
    records: &[(Option<Vec<u8>>, Vec<u8>)],
) -> Result<Vec<ProduceAck>, KafkaError> {
    send_records_with_headers(topic, records, &[])
}

/// As [`send_records`], with headers stamped on every record. Used for
/// dead-lettering, where the provenance matters as much as the payload.
fn send_records_with_headers(
    topic: &str,
    records: &[(Option<Vec<u8>>, Vec<u8>)],
    headers: &[(&str, Vec<u8>)],
) -> Result<Vec<ProduceAck>, KafkaError> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let acks = required_acks()?;
    let codec = compression()?;
    // Via `std` rather than a new world import: on wasm32-wasip2 this lowers to
    // the `wasi:clocks/wall-clock` already in the plugin's base WASI set.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut conns_guard = PRODUCER_CONNS.lock().map_err(poisoned)?;
    let conns = conns_guard.get_or_insert_with(HashMap::new);
    let seed = brokers()?
        .first()
        .cloned()
        .ok_or_else(|| KafkaError::NotConfigured("no brokers configured".to_owned()))?;

    let mut meta_guard = PRODUCER_META.lock().map_err(poisoned)?;
    if meta_guard.is_none()
        || !meta_guard
            .as_ref()
            .is_some_and(|c| c.topics.contains_key(topic))
    {
        // Refreshed when the topic is unknown, which is usually a topic created
        // since the last look rather than a real absence.
        let cluster = connect(conns, &seed)?
            .metadata()
            .map_err(|e| fetch_error(e, &seed))?;
        *meta_guard = Some(cluster);
    }
    let cluster = meta_guard
        .as_ref()
        .ok_or_else(|| KafkaError::Connection("cluster metadata vanished".to_owned()))?;

    // Group by partition so each one is a single request, which is also what
    // makes a batch cheaper than N sends.
    let mut by_partition: BTreeMap<i32, Vec<PendingRecord>> = BTreeMap::new();
    for (key, value) in records {
        let partition = choose_partition(cluster, topic, key.as_deref())?;
        by_partition
            .entry(partition)
            .or_default()
            .push((key.clone(), value.clone()));
    }

    let mut acks_out = Vec::with_capacity(records.len());
    for (partition, group) in by_partition {
        let addr = cluster
            .leader_addr(topic, partition)
            .ok_or_else(|| {
                KafkaError::Connection(format!("no leader known for {topic}/{partition}"))
            })?
            .to_owned();
        let conn = connect(conns, &addr)?;

        let batch = produce::encode_batch(&group, now_ms, codec, headers);
        let (correlation, stream) = conn.next_request();
        let result = produce::produce(
            stream,
            correlation,
            topic,
            partition,
            acks,
            PRODUCE_TIMEOUT,
            &batch,
        );
        let at = match result {
            Ok(at) => at,
            Err(e) => {
                // An I/O failure leaves the connection mid-response and so
                // unusable for the next request.
                if matches!(e, produce::ProduceError::Io(_)) {
                    conns.remove(&addr);
                }
                return Err(map_produce_error(e, &addr));
            }
        };

        // The broker reports the base offset; the rest follow it in order.
        for i in 0..group.len() {
            acks_out.push(ProduceAck {
                partition: at.partition,
                offset: if at.offset < 0 {
                    -1
                } else {
                    at.offset + i as i64
                },
            });
        }
    }
    Ok(acks_out)
}

impl ProducerGuest for Component {
    /// `async` because the host installs a plugin's capabilities on a caller's
    /// linker as concurrent host functions — not because anything here yields.
    /// The body is a blocking client call, so this future completes on its
    /// first poll and holds the plugin instance for the round trip.
    async fn send(
        topic: String,
        key: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<ProduceAck, KafkaError> {
        send_records(&topic, &[(key, value)])?
            .into_iter()
            .next()
            .ok_or_else(|| KafkaError::Protocol("broker acknowledged no partitions".to_owned()))
    }

    /// One request per partition rather than per record, which is the whole
    /// point of a batch: a keyed batch spanning three partitions costs three
    /// round trips, not one per record.
    async fn send_batch(
        topic: String,
        records: Vec<(Option<Vec<u8>>, Vec<u8>)>,
    ) -> Result<Vec<ProduceAck>, KafkaError> {
        send_records(&topic, &records)
    }
}

// ---------------------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------------------

/// Longest a `poll` may hold the plugin instance waiting for a first record.
/// The client blocks, so an unbounded wait would park the plugin — and it is a
/// singleton serving every workload — on one caller's empty topic.
const MAX_POLL_WAIT: Duration = Duration::from_secs(10);

/// Per-partition response cap. Large enough that a poll is one round trip for a
/// normal batch, small enough that one call cannot buffer a whole partition.
const FETCH_MAX_BYTES: i32 = 1 << 20;

/// The consumer, assembled from `kafka-rust` for everything that still works
/// against a modern broker — metadata, group offset fetch and commit — plus
/// [`fetch`] for the one API that does not.
///
/// Partitions are assigned statically: every partition of every configured
/// topic, claimed by this plugin. No `JoinGroup`/`SyncGroup` membership, which
/// costs nothing here because the plugin is a host-scoped singleton and so is
/// already the only consumer. The group name is still used, for offset storage,
/// so offsets survive a restart and are visible to `kafka-consumer-groups`.
struct KafkaConsumer {
    /// Cluster snapshot: partition leaders and broker addresses.
    cluster: Cluster,
    /// One connection per broker, held across polls. Re-handshaking TCP per
    /// call is exactly the per-request cost this plugin exists to absorb.
    conns: HashMap<String, BrokerConn>,
    group: String,
    /// Subscribed topics, kept for rejoining: a rebalance re-sends them.
    topics: Vec<String>,
    /// Group membership, or `None` under static assignment. Holds the
    /// generation every commit is fenced by.
    membership: Option<Membership>,
    session_timeout_ms: i32,
    /// `monotonic_clock::now()` at the last heartbeat, in nanoseconds.
    last_heartbeat: u64,
    /// `(topic, partition)` in a stable order, walked round-robin so a busy
    /// partition cannot starve the others.
    ///
    /// Under group membership this is the coordinator's answer and holds only
    /// the partitions *this* member owns; under static assignment it is every
    /// partition of every subscribed topic.
    assignment: Vec<(String, i32)>,
    next_partition: usize,
    /// Next offset to fetch, per partition.
    positions: BTreeMap<(String, i32), i64>,
    /// Consecutive failures per partition, keyed by the batch's first offset so
    /// the count resets as soon as the cursor moves on.
    attempts: BTreeMap<(String, i32), (i64, u32)>,
    /// The broker coordinating this consumer group, resolved on first commit.
    coordinator: Option<String>,
    /// Next offset to commit, per partition — advanced only for records that
    /// were actually handed to a caller.
    pending: BTreeMap<(String, i32), i64>,
}

static CONSUMER: Mutex<Option<KafkaConsumer>> = Mutex::new(None);

impl KafkaConsumer {
    fn create() -> Result<Self, KafkaError> {
        let topics = split_csv(&config(CFG_TOPICS)?.unwrap_or_default());
        if topics.is_empty() {
            return Err(KafkaError::NotConfigured(format!(
                "config key '{CFG_TOPICS}' is unset or empty; set it on the plugin to a \
                 comma-separated topic list"
            )));
        }
        let group = config(CFG_GROUP)?.unwrap_or_else(|| DEFAULT_GROUP.to_owned());

        let seed = brokers()?
            .first()
            .cloned()
            .ok_or_else(|| KafkaError::NotConfigured("no brokers configured".to_owned()))?;
        let mut conns: HashMap<String, BrokerConn> = HashMap::new();
        let cluster = connect(&mut conns, &seed)?
            .metadata()
            .map_err(|e| fetch_error(e, &seed))?;

        // Every subscribed topic must exist before anything is assigned:
        // "the group gave me no partitions" and "the topic is misspelled" are
        // indistinguishable otherwise.
        for topic in &topics {
            if cluster.available_partitions(topic).is_empty() {
                return Err(KafkaError::UnknownTopic(format!(
                    "broker knows no topic '{topic}', or none of its partitions have a leader"
                )));
            }
        }

        let mut consumer = Self {
            cluster,
            conns,
            group,
            topics: topics.clone(),
            membership: None,
            session_timeout_ms: session_timeout_ms()?,
            last_heartbeat: 0,
            assignment: Vec::new(),
            next_partition: 0,
            positions: BTreeMap::new(),
            pending: BTreeMap::new(),
            attempts: BTreeMap::new(),
            coordinator: None,
        };

        if group_membership_enabled()? {
            consumer.join()?;
        } else {
            consumer.assign_every_partition();
            consumer.load_positions()?;
        }
        Ok(consumer)
    }

    /// Take every partition of every subscribed topic.
    ///
    /// Correct only while this is the sole consumer in its group, which nothing
    /// checks — see [`Self::join`] for the mechanism that does.
    fn assign_every_partition(&mut self) {
        let mut assignment = Vec::new();
        for topic in &self.topics {
            for id in self.cluster.available_partitions(topic) {
                assignment.push((topic.clone(), id));
            }
        }
        assignment.sort();
        self.assignment = assignment;
    }

    /// Join (or rejoin) the consumer group and adopt the assignment it hands
    /// back.
    ///
    /// The coordinator divides the partitions among the members, so a second
    /// plugin in the same group takes over some of them rather than
    /// duplicating all of them. Assignment is computed *client-side* by whoever
    /// the coordinator elects leader — this is why the strategy travels as a
    /// protocol name, and why [`group::range_assign`] has to agree with the
    /// stock `RangeAssignor` byte for byte to share a group with one.
    ///
    /// Everything derived from the previous assignment is dropped: partitions
    /// this member no longer owns must not be committed for, and ones it has
    /// just been given start from the group's committed offset, not from
    /// whatever this member last read.
    fn join(&mut self) -> Result<(), KafkaError> {
        let addr = self.coordinator_addr()?;
        let group = self.group.clone();
        let topics = self.topics.clone();
        let member_id = self
            .membership
            .as_ref()
            .map(|m| m.member_id.clone())
            .unwrap_or_default();
        let session_timeout = self.session_timeout_ms;

        let joined = {
            let conn = self.conn_for(&addr)?;
            conn.join_group(&group, &member_id, &topics, session_timeout)
                .map_err(|e| fetch_error(e, &addr))?
        };

        // Only the leader computes assignments; a follower syncs an empty map
        // and is told its own share.
        let assignments = if joined.leader {
            // Refresh first: the leader assigns for *every* member, including
            // ones subscribed to topics this member does not read and so may
            // never have seen. A topic missing from a stale snapshot has no
            // partitions to hand out, and would be silently left unconsumed for
            // the whole generation. A rebalance is also exactly when the
            // cluster is most likely to have changed.
            self.reload_metadata()?;
            let cluster = &self.cluster;
            group::range_assign(&joined.members, &|topic| {
                cluster.available_partitions(topic)
            })
        } else {
            BTreeMap::new()
        };

        let assignment = {
            let conn = self.conn_for(&addr)?;
            conn.sync_group(&group, joined.generation, &joined.member_id, &assignments)
                .map_err(|e| fetch_error(e, &addr))?
        };

        log(
            Level::Info,
            "kafka-plugin",
            &format!(
                "joined group '{}' as {} in generation {}{}, holding {} partition(s)",
                group,
                joined.member_id,
                joined.generation,
                if joined.leader { " (leader)" } else { "" },
                assignment.len(),
            ),
        );

        self.assignment = assignment;
        self.membership = Some(Membership {
            generation: joined.generation,
            member_id: joined.member_id,
        });
        self.next_partition = 0;
        self.positions.clear();
        self.pending.clear();
        self.attempts.clear();
        self.last_heartbeat = monotonic_clock::now();
        self.load_positions()
    }

    /// Heartbeat if one is due, and rejoin if the answer says the group moved
    /// on without this member.
    ///
    /// A third of the session timeout is the usual interval: it tolerates two
    /// lost heartbeats before eviction. This is called from the poll paths, so
    /// the pacing is only as good as how often something polls — a handler that
    /// occupies the plugin for longer than the session timeout will be evicted
    /// and its partitions reassigned, which is the trade for not having a
    /// background thread to heartbeat from.
    fn heartbeat_if_due(&mut self) -> Result<(), KafkaError> {
        let Some(membership) = self.membership.as_ref() else {
            return Ok(());
        };
        let interval_ns = (self.session_timeout_ms as u64 / 3) * 1_000_000;
        let now = monotonic_clock::now();
        if now.saturating_sub(self.last_heartbeat) < interval_ns {
            return Ok(());
        }

        let (generation, member_id) = (membership.generation, membership.member_id.clone());
        let group = self.group.clone();
        let addr = self.coordinator_addr()?;
        let result = {
            let conn = self.conn_for(&addr)?;
            conn.heartbeat(&group, generation, &member_id)
        };
        self.last_heartbeat = now;

        match result {
            Ok(()) => Ok(()),
            Err(FetchError::Broker(code)) if group::is_rejoin_signal(code) => {
                log(
                    Level::Info,
                    "kafka-plugin",
                    &format!("group '{group}' is rebalancing (code {code}); rejoining"),
                );
                self.join()
            }
            Err(e) => Err(fetch_error(e, &addr)),
        }
    }

    /// The broker coordinating this group, resolved once and cached.
    ///
    /// In general a different broker from any partition leader, and both the
    /// group APIs and offset commits must go to it.
    fn coordinator_addr(&mut self) -> Result<String, KafkaError> {
        if let Some(addr) = &self.coordinator {
            return Ok(addr.clone());
        }
        let seed = brokers()?
            .first()
            .cloned()
            .ok_or_else(|| KafkaError::NotConfigured("no brokers configured".to_owned()))?;
        let group = self.group.clone();
        let found = {
            let conn = self.conn_for(&seed)?;
            conn.find_coordinator(&group)
                .map_err(|e| fetch_error(e, &seed))?
        };
        self.coordinator = Some(found.clone());
        Ok(found)
    }

    /// Leave the group deliberately, so it rebalances now rather than after the
    /// session timeout expires.
    fn leave(&mut self) {
        let Some(membership) = self.membership.take() else {
            return;
        };
        let Ok(addr) = self.coordinator_addr() else {
            return;
        };
        let group = self.group.clone();
        if let Ok(conn) = self.conn_for(&addr) {
            // Best effort: the session timeout is the backstop, so a failure
            // here costs a slower rebalance and nothing else.
            let _ = conn.leave_group(&group, &membership.member_id);
        }
    }

    /// Start each partition where the group left off, falling back to the
    /// earliest retained offset. A committed offset is the *next* one to read,
    /// which is what `positions` holds, so the two need no conversion.
    fn load_positions(&mut self) -> Result<(), KafkaError> {
        // Grouped by topic because `OffsetFetch` takes one topic and a list of
        // partitions, and the assignment is a flat list of pairs.
        let mut by_topic: BTreeMap<String, Vec<i32>> = BTreeMap::new();
        for (topic, partition) in &self.assignment {
            by_topic.entry(topic.clone()).or_default().push(*partition);
        }

        for (topic, partitions) in by_topic {
            let topic = topic.as_str();
            let seed = self.leader_for(topic, partitions[0])?;
            let group = self.group.clone();
            let committed = {
                let conn = self.conn_for(&seed)?;
                conn.offset_fetch(&group, topic, &partitions)
                    .map_err(|e| fetch_error(e, &seed))?
            };

            for (partition, offset) in committed {
                // A group with no commit yet reports -1.
                // A group with no commit yet reports -1, so fall back to the
                // oldest retained record. `ListOffsets` v1 via our own client:
                // `kafka-rust` sends v0, which Kafka 4.x refuses.
                let start = if offset >= 0 {
                    offset
                } else {
                    self.earliest_offset(topic, partition)?
                };
                self.positions.insert((topic.to_owned(), partition), start);
            }
        }
        Ok(())
    }

    /// The oldest offset still retained in a partition.
    fn earliest_offset(&mut self, topic: &str, partition: i32) -> Result<i64, KafkaError> {
        let addr = self.leader_for(topic, partition)?;
        let conn = self.conn_for(&addr)?;
        conn.list_offset(topic, partition, -2)
            .map_err(|e| fetch_error(e, &addr))
    }

    /// The address of the broker currently leading `partition`.
    /// The address of the broker currently leading `partition`, refreshing the
    /// cluster snapshot once if this one has no leader for it.
    ///
    /// The refresh is what makes this recoverable. A partition mid-election has
    /// no leader, and without a reload nothing would ever correct that — the
    /// `NOT_LEADER` path only fires when a broker *answers*, and there is no
    /// broker to send to. Deliberately **not** a `connection` error afterwards
    /// either: that variant means the transport failed, and
    /// [`with_consumer`] tears the whole consumer down when it sees one. A
    /// partition briefly without a leader is normal cluster behaviour, and
    /// answering it by leaving the group would turn a local election into a
    /// group-wide rebalance.
    fn leader_for(&mut self, topic: &str, partition: i32) -> Result<String, KafkaError> {
        if let Some(addr) = self.cluster.leader_addr(topic, partition) {
            return Ok(addr.to_owned());
        }
        self.reload_metadata()?;
        self.cluster
            .leader_addr(topic, partition)
            .map(str::to_owned)
            .ok_or_else(|| {
                KafkaError::Protocol(format!(
                    "no leader for {topic}/{partition}: the partition is mid-election or offline"
                ))
            })
    }

    /// Re-fetch the cluster snapshot from a bootstrap broker.
    fn reload_metadata(&mut self) -> Result<(), KafkaError> {
        let seed = brokers()?
            .first()
            .cloned()
            .ok_or_else(|| KafkaError::NotConfigured("no brokers configured".to_owned()))?;
        self.cluster = {
            let conn = connect(&mut self.conns, &seed)?;
            conn.metadata().map_err(|e| fetch_error(e, &seed))?
        };
        Ok(())
    }

    fn conn_for(&mut self, addr: &str) -> Result<&mut BrokerConn, KafkaError> {
        connect(&mut self.conns, addr)
    }

    fn poll(
        &mut self,
        max_records: usize,
        timeout: Duration,
    ) -> Result<Vec<KafkaRecord>, KafkaError> {
        // Before reading, not after: a heartbeat that discovers a rebalance
        // replaces the assignment this poll would otherwise have read from.
        self.heartbeat_if_due()?;
        let mut out = Vec::new();
        if self.assignment.is_empty() || max_records == 0 {
            return Ok(out);
        }

        // One pass over the assignment, resuming where the last poll stopped so
        // partitions take turns being served first.
        for step in 0..self.assignment.len() {
            if out.len() >= max_records {
                break;
            }
            let idx = (self.next_partition + step) % self.assignment.len();
            let (topic, partition) = self.assignment[idx].clone();

            // Only the first fetch of a poll waits: once anything has been
            // collected, the caller gets it now rather than after every other
            // partition has also been given the full timeout.
            let wait = if out.is_empty() {
                timeout
            } else {
                Duration::ZERO
            };

            let outcome = self.fetch_partition(&topic, partition, wait)?;
            let key = (topic.clone(), partition);
            for rec in outcome {
                if out.len() >= max_records {
                    break;
                }
                // Advance only past records actually handed over, so a caller
                // never has an offset committed for something it did not see.
                self.positions.insert(key.clone(), rec.offset + 1);
                self.pending.insert(key.clone(), rec.offset + 1);
                out.push(KafkaRecord {
                    topic: topic.clone(),
                    partition: rec.partition,
                    offset: rec.offset,
                    key: rec.key,
                    value: rec.value,
                    timestamp_ms: Some(rec.timestamp_ms),
                });
            }
        }

        self.next_partition = (self.next_partition + 1) % self.assignment.len();
        Ok(out)
    }

    /// Poll every assigned partition once, keeping each partition's records
    /// separate. Partitions with nothing new are left out.
    ///
    /// Separate, because a partition is the unit that can be worked on
    /// independently: Kafka orders records within one and commits a single
    /// high-water mark per one. Merging them (as [`Self::poll`] does for the
    /// pull interface) is fine for a caller that commits everything at once,
    /// but it is exactly the wrong shape for dispatching batches concurrently —
    /// there would be no way to commit the batch that succeeded without also
    /// committing the one that did not.
    fn poll_partitions(
        &mut self,
        per_partition: usize,
        timeout: Duration,
    ) -> Vec<(String, i32, Vec<KafkaRecord>)> {
        let mut batches = Vec::new();
        if per_partition == 0 {
            return batches;
        }
        // A failed heartbeat is not fatal to the pass: the partitions this
        // member still owns are worth reading, and a genuine eviction shows up
        // again on the next pass and on the next commit.
        if let Err(e) = self.heartbeat_if_due() {
            log(
                Level::Warn,
                "kafka-plugin",
                &format!("heartbeat failed: {e:?}"),
            );
        }

        for idx in 0..self.assignment.len() {
            let (topic, partition) = self.assignment[idx].clone();
            // Only the first fetch waits. Giving every partition the full
            // timeout would make an idle pass cost `partitions * timeout`.
            let wait = if batches.is_empty() {
                timeout
            } else {
                Duration::ZERO
            };

            // A partition that fails is skipped, not fatal: its records stay
            // uncommitted and the other partitions still get their turn. A
            // broker problem affecting all of them shows up as an empty pass.
            let Ok(fetched) = self.fetch_partition(&topic, partition, wait) else {
                continue;
            };
            if fetched.is_empty() {
                continue;
            }

            let key = (topic.clone(), partition);
            let mut records = Vec::with_capacity(fetched.len().min(per_partition));
            for rec in fetched.into_iter().take(per_partition) {
                self.positions.insert(key.clone(), rec.offset + 1);
                self.pending.insert(key.clone(), rec.offset + 1);
                records.push(KafkaRecord {
                    topic: topic.clone(),
                    partition: rec.partition,
                    offset: rec.offset,
                    key: rec.key,
                    value: rec.value,
                    timestamp_ms: Some(rec.timestamp_ms),
                });
            }
            batches.push((topic, partition, records));
        }
        batches
    }

    /// Commit one partition's progress, leaving every other partition's alone.
    ///
    /// This is what makes concurrent dispatch safe. Each partition's committed
    /// offset advances only when that partition's batch was handled, so a
    /// failure in one cannot carry another's records past the point they were
    /// actually processed.
    fn commit_partition(&mut self, topic: &str, partition: i32) -> Result<Option<i64>, KafkaError> {
        let key = (topic.to_owned(), partition);
        let Some(offset) = self.pending.remove(&key) else {
            return Ok(None);
        };
        self.commit_one(topic, partition, offset)?;
        Ok(Some(offset))
    }

    /// Commit one offset to the group's coordinator, fenced by the generation
    /// it was read in.
    ///
    /// `OffsetCommit` v2, which Kafka 4.x still accepts. The commit carries
    /// this member's generation and id, so if the group rebalanced while the
    /// batch was in flight the coordinator rejects it — the partition now
    /// belongs to someone else, and letting this commit through would move
    /// *their* offset past records they never saw. That rejection is a rejoin
    /// signal, not a failure: the records stay uncommitted and are redelivered
    /// to whoever owns the partition now.
    fn commit_one(&mut self, topic: &str, partition: i32, offset: i64) -> Result<(), KafkaError> {
        let addr = self.coordinator_addr()?;
        let group = self.group.clone();
        let (generation, member_id) = match &self.membership {
            Some(m) => (m.generation, m.member_id.clone()),
            // "Simple consumer": no membership, so nothing to fence against.
            None => (-1, String::new()),
        };

        let result = {
            let conn = self.conn_for(&addr)?;
            conn.commit_offset(&group, generation, &member_id, topic, partition, offset)
        };
        match result {
            Ok(()) => Ok(()),
            Err(FetchError::Broker(code)) if group::is_rejoin_signal(code) => {
                log(
                    Level::Warn,
                    "kafka-plugin",
                    &format!(
                        "commit of {topic}/{partition}@{offset} was fenced (code {code}): this \
                         member no longer owns the partition. Rejoining; the records will be \
                         redelivered to whoever does."
                    ),
                );
                self.join()?;
                Err(KafkaError::Protocol(format!(
                    "commit fenced by a rebalance (code {code})"
                )))
            }
            Err(e) => Err(fetch_error(e, &addr)),
        }
    }

    /// Record a failed delivery and report how many consecutive times *this*
    /// batch has now failed.
    ///
    /// Keyed by first offset, so a batch that fails once and then succeeds
    /// leaves no residue, and a *different* batch failing later starts from
    /// one rather than inheriting a stale count.
    fn note_failure(&mut self, topic: &str, partition: i32, first_offset: i64) -> u32 {
        let entry = self
            .attempts
            .entry((topic.to_owned(), partition))
            .or_insert((first_offset, 0));
        if entry.0 != first_offset {
            *entry = (first_offset, 0);
        }
        entry.1 += 1;
        entry.1
    }

    fn clear_failures(&mut self, topic: &str, partition: i32) {
        self.attempts.remove(&(topic.to_owned(), partition));
    }

    /// Forget a partition's uncommitted progress and rewind to what the group
    /// last committed, so a failed batch is redelivered rather than skipped.
    ///
    /// Without the rewind, `positions` would still point past the failed batch
    /// and the next poll would fetch what comes *after* it — losing exactly the
    /// records the failure was meant to protect.
    fn rewind_partition(&mut self, topic: &str, partition: i32, first_offset: i64) {
        let key = (topic.to_owned(), partition);
        self.pending.remove(&key);
        self.positions.insert(key, first_offset);
    }

    /// Fetch one partition, handling the two broker errors that are recoverable
    /// in place: a lost leader (metadata is stale) and an offset that has aged
    /// out of the partition (reset to the earliest retained).
    fn fetch_partition(
        &mut self,
        topic: &str,
        partition: i32,
        wait: Duration,
    ) -> Result<Vec<FetchedRecord>, KafkaError> {
        for attempt in 0..2 {
            let offset = *self
                .positions
                .get(&(topic.to_owned(), partition))
                .unwrap_or(&0);
            let addr = self.leader_for(topic, partition)?;

            let result = {
                let conn = self.conn_for(&addr)?;
                conn.fetch(topic, partition, offset, wait, FETCH_MAX_BYTES)
            };

            match result {
                Ok(records) => return Ok(records),
                Err(FetchError::Broker(code)) if code == ERR_NOT_LEADER && attempt == 0 => {
                    // Leadership moved; the cached address is wrong.
                    self.conns.remove(&addr);
                    self.reload_metadata()?;
                }
                Err(FetchError::Broker(code))
                    if code == ERR_OFFSET_OUT_OF_RANGE && attempt == 0 =>
                {
                    // Retention passed this offset by. Resume at the oldest
                    // record still held rather than failing every later poll.
                    let earliest = self.earliest_offset(topic, partition)?;
                    self.positions
                        .insert((topic.to_owned(), partition), earliest);
                }
                Err(e) => {
                    // An I/O failure leaves the connection mid-response and so
                    // unusable for the next request.
                    if matches!(e, FetchError::Io(_)) {
                        self.conns.remove(&addr);
                    }
                    return Err(fetch_error(e, &addr));
                }
            }
        }
        Ok(Vec::new())
    }

    fn commit(&mut self) -> Result<(), KafkaError> {
        // Take the pending set first: a failed commit leaves it taken, and the
        // records replay from the last committed offset, which is the
        // at-least-once behaviour a caller can reason about.
        let pending = std::mem::take(&mut self.pending);
        for ((topic, partition), offset) in pending {
            self.commit_one(&topic, partition, offset)?;
        }
        Ok(())
    }
}

/// Map a fetch-layer failure onto the interface's error variants, keeping the
/// distinction a caller acts on: unreachable broker, unknown topic, or a
/// protocol problem it can only report.
fn fetch_error(e: FetchError, addr: &str) -> KafkaError {
    match e {
        FetchError::Io(io) => KafkaError::Connection(format!("{addr}: {io}")),
        FetchError::Broker(code) => match code {
            ERR_OFFSET_OUT_OF_RANGE => {
                KafkaError::Protocol("fetch offset is outside the partition's range".to_owned())
            }
            3 => KafkaError::UnknownTopic("broker reports unknown topic or partition".to_owned()),
            other => KafkaError::Protocol(format!("broker returned error code {other}")),
        },
        FetchError::Protocol(msg) => KafkaError::Protocol(msg),
    }
}

/// Run `f` against the shared consumer, building it on first use.
///
/// A connection error discards the consumer entirely rather than leaving it in
/// place. Its broker connections are held across calls by design, and a broken
/// one stays broken — without this, a single dropped TCP connection would fail
/// every subsequent call for the life of the plugin. Leaving the group first
/// turns the reconnect into an immediate rebalance instead of one that waits
/// out the session timeout with this member's partitions unread.
fn with_consumer<T>(
    f: impl FnOnce(&mut KafkaConsumer) -> Result<T, KafkaError>,
) -> Result<T, KafkaError> {
    let mut guard = CONSUMER.lock().map_err(poisoned)?;
    if guard.is_none() {
        *guard = Some(KafkaConsumer::create()?);
    }
    let consumer = guard.as_mut().ok_or_else(|| {
        KafkaError::Connection("consumer disappeared after initialization".to_owned())
    })?;

    let result = f(consumer);
    if let Err(KafkaError::Connection(reason)) = &result {
        log(
            Level::Warn,
            LOG_CONTEXT,
            &format!("dropping consumer state after a connection error: {reason}"),
        );
        consumer.leave();
        *guard = None;
    }
    result
}

impl ConsumerGuest for Component {
    async fn poll(max_records: u32, timeout_ms: u32) -> Result<Vec<KafkaRecord>, KafkaError> {
        let timeout = Duration::from_millis(u64::from(timeout_ms)).min(MAX_POLL_WAIT);
        with_consumer(|consumer| consumer.poll(max_records as usize, timeout))
    }

    async fn commit() -> Result<(), KafkaError> {
        with_consumer(KafkaConsumer::commit)
    }
}

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

/// Interface a workload must export to be dispatched to. Matched by prefix so
/// the version suffix the host reports does not have to be hardcoded here — the
/// exact spelling comes back from `callable`, which is what `target.open`
/// wants.
const HANDLER_INTERFACE: &str = "cosmonic:kafka/handler";

/// How long a poll parks at the broker when there is nothing to read. This is
/// the loop's idle cost and its only pacing: the broker holds the request open
/// until records arrive or this elapses, so an empty topic costs one in-flight
/// request rather than a spin.
const TRIGGER_POLL_WAIT_MS: u32 = 1_000;

/// Records handed to a workload per call, unless `trigger-batch-size` says
/// otherwise. Small enough that a failed batch redelivers little, large enough
/// that the cross-store call is not the dominant cost.
const DEFAULT_TRIGGER_BATCH: u32 = 32;

/// How long the loop suspends between passes. Short enough that a burst is
/// picked up promptly, long enough that an idle topic costs one suspended timer
/// rather than a spin.
const IDLE_BACKOFF_NS: u64 = 200_000_000;

/// How many partitions may have a batch in flight at once, unless
/// `trigger-max-inflight` says otherwise. Each one is a live workload instance
/// holding a store, so this is a memory ceiling as much as a throughput knob.
const DEFAULT_TRIGGER_INFLIGHT: usize = 8;

/// Redeliveries of one batch before it is dead-lettered, unless
/// `trigger-max-attempts` says otherwise.
///
/// Finite on purpose. Redelivering forever is the failure mode that looks
/// safest and is not: a batch whose handler can never succeed — a malformed
/// record, a bug on a code path only it reaches — blocks its partition
/// permanently, and everything behind it stops. Better to move it aside and
/// keep the partition flowing.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Context string on every log line the trigger emits, so its output can be
/// picked out of a host serving many plugins.
const LOG_CONTEXT: &str = "cosmonic-kafka-trigger";

impl RunGuest for Component {
    /// The trigger loop.
    ///
    /// Each pass polls every partition, then hands their batches to the
    /// workload *concurrently* — one in-flight call per partition, capped by
    /// `trigger-max-inflight`. The host runs each on its own instance, so a
    /// topic's partition count is what a burst scales across, and the instances
    /// retire when it drains.
    ///
    /// Per-partition is not an arbitrary unit. Kafka orders records within a
    /// partition and commits one high-water mark per partition, so partitions
    /// are precisely the pieces that can be worked on independently and
    /// committed independently. Splitting a single partition's records across
    /// concurrent calls would both break that ordering and make "which offset
    /// is safe to commit" unanswerable. More parallelism means more partitions
    /// — the same answer Kafka gives every other consumer.
    ///
    /// Errors never end the loop. A trigger that exits on a broker blip is
    /// worse than one that retries: the plugin is a host-scoped singleton, and
    /// returning here would stop dispatching for every workload it serves until
    /// the whole plugin was restarted.
    async fn run() -> Result<(), ()> {
        if !trigger_enabled() {
            // Returning is the documented way to have no background work; the
            // capability exports keep serving either way.
            return Ok(());
        }
        let batch_size = trigger_batch_size();
        let max_inflight = trigger_max_inflight();

        loop {
            // Every path suspends somewhere, because this store also serves
            // every workload's capability calls and a loop that never yields
            // starves them. But *how* it suspends depends on whether there was
            // work: backing off after a productive pass would cap throughput at
            // one batch per backoff, which is the whole burst arriving slowly.
            //
            // Nothing to dispatch to yet: leave the records where they are —
            // uncommitted and in the log — rather than polling them into a
            // buffer this singleton would drop on restart.
            let Some((target_id, interface)) = first_handler_workload() else {
                monotonic_clock::wait_for(IDLE_BACKOFF_NS).await;
                continue;
            };

            // Keep going whatever happened. A permanent failure — an
            // unreadable topic, a handler that always rejects — otherwise looks
            // exactly like an idle one, so it is logged rather than swallowed;
            // never returned, because this loop is the only dispatcher for
            // every workload the plugin serves.
            match dispatch_pass(&target_id, &interface, batch_size, max_inflight).await {
                // Caught up. Park on a timer so an idle topic is cheap; the
                // poll's own broker-side wait already absorbs most of the idle.
                Ok(0) => monotonic_clock::wait_for(IDLE_BACKOFF_NS).await,
                // There was work, so there is probably more. Yield rather than
                // sleep: the capability calls sharing this store get their turn,
                // and the next batch starts immediately instead of a backoff
                // later.
                Ok(n) => {
                    log(
                        Level::Info,
                        LOG_CONTEXT,
                        &format!("dispatched and committed {n} records to '{target_id}'"),
                    );
                    wit_bindgen::yield_async().await;
                }
                Err(e) => {
                    log(
                        Level::Error,
                        LOG_CONTEXT,
                        &format!("dispatch failed, records will be redelivered: {e:?}"),
                    );
                    // Pace retries; a broker that is down stays down for a while.
                    monotonic_clock::wait_for(IDLE_BACKOFF_NS).await;
                }
            }
        }
    }
}

/// Poll every partition and dispatch their batches concurrently, committing
/// each partition on its own success. Returns how many records were committed.
async fn dispatch_pass(
    target_id: &str,
    interface: &str,
    batch_size: u32,
    max_inflight: usize,
) -> Result<usize, KafkaError> {
    let batches = with_consumer(|consumer| {
        Ok(consumer.poll_partitions(
            batch_size as usize,
            Duration::from_millis(u64::from(TRIGGER_POLL_WAIT_MS)),
        ))
    })?;
    if batches.is_empty() {
        return Ok(0);
    }

    // `callable` was a snapshot, and the workload can have stopped since. A
    // `none` here is that race, not an error worth escalating — the records
    // stay uncommitted and go to whoever is up on the next pass.
    // A handle is per workload *and* per interface, and exists only if that
    // workload really serves that interface — so holding one is proof the call
    // can be routed. `none` is the race `callable` warns about: a snapshot, and
    // the workload can stop between reading it and acting on it.
    let Some(_target) = workload::Target::open(target_id, interface) else {
        return Err(KafkaError::Connection(format!(
            "workload '{target_id}' stopped before the batch could be delivered"
        )));
    };

    let mut committed = 0;
    // Windowed rather than all-at-once: a topic with a hundred partitions would
    // otherwise put a hundred workload instances in flight, each holding a
    // store. The target handle stays open across the whole pass, so every call
    // in every window is routed to the same workload — the concurrency is
    // across that workload's *instances*, which is what the host spins up.
    for window in batches.chunks(max_inflight) {
        let calls = window.iter().map(|(topic, partition, records)| {
            // Each call owns its arguments so the futures can be polled
            // together without borrowing the batch list.
            let topic = topic.clone();
            let partition = *partition;
            let count = records.len();
            let first_offset = records.first().map_or(0, |r| r.offset);
            // The record type is generated twice — once for the interface this
            // plugin exports, once for the one it imports — so the same shape
            // has to be restated to cross from one to the other.
            let outbound: Vec<handler_types::KafkaRecord> = records
                .iter()
                .map(|r| handler_types::KafkaRecord {
                    topic: r.topic.clone(),
                    partition: r.partition,
                    offset: r.offset,
                    key: r.key.clone(),
                    value: r.value.clone(),
                    timestamp_ms: r.timestamp_ms,
                })
                .collect();

            async move {
                let _ = (&topic, partition);
                let result = handler::handle(outbound).await;
                (count, first_offset, result)
            }
        });

        // `join_all` polls them together on this one task, and each `handle` is
        // an async-lowered call the host serves on its own instance — so the
        // batches really do overlap rather than taking turns.
        // Zipped against the window so a failed batch still has its records to
        // hand: `join_all` preserves order, so the nth result is the nth batch.
        for ((topic, partition, records), (count, first_offset, result)) in
            window.iter().zip(futures::future::join_all(calls).await)
        {
            let (topic, partition) = (topic.clone(), *partition);
            match result {
                Ok(()) => {
                    // Only this partition's offset moves, and only because this
                    // partition's batch was handled.
                    let at =
                        with_consumer(|consumer| consumer.commit_partition(&topic, partition))?;
                    log(
                        Level::Debug,
                        LOG_CONTEXT,
                        &format!("committed {topic}/{partition} at offset {at:?}"),
                    );
                    committed += count;
                }
                Err(e) => {
                    let attempts = with_consumer(|consumer| {
                        Ok(consumer.note_failure(&topic, partition, first_offset))
                    })?;
                    let max = trigger_max_attempts();

                    if attempts < max {
                        log(
                            Level::Warn,
                            LOG_CONTEXT,
                            &format!(
                                "handler rejected {count} records from {topic}/{partition} \
                                 (attempt {attempts}/{max}), rewinding to offset \
                                 {first_offset}: {e}"
                            ),
                        );
                        // Put the cursor back to the start of the failed batch
                        // so it is redelivered. Its neighbours are unaffected —
                        // that isolation is the whole reason to commit per
                        // partition.
                        with_consumer(|consumer| {
                            consumer.rewind_partition(&topic, partition, first_offset);
                            Ok(())
                        })?;
                        continue;
                    }

                    // Out of attempts. Move the batch aside and commit past it:
                    // holding a partition hostage to one bad batch stops every
                    // record behind it, which is a worse failure than parking
                    // these somewhere durable and going on.
                    let dlq = dlq_topic(&topic);
                    match dead_letter(&dlq, &topic, partition, records, &e) {
                        Ok(()) => {
                            log(
                                Level::Error,
                                LOG_CONTEXT,
                                &format!(
                                    "handler rejected {count} records from {topic}/{partition} \
                                     {max} times; moved to '{dlq}' and advancing past offset \
                                     {first_offset}: {e}"
                                ),
                            );
                            with_consumer(|consumer| consumer.commit_partition(&topic, partition))?;
                            with_consumer(|consumer| {
                                consumer.clear_failures(&topic, partition);
                                Ok(())
                            })?;
                        }
                        Err(dlq_err) => {
                            // The DLQ is the safety net; if it is unreachable
                            // the only safe thing left is to keep retrying,
                            // because advancing now would drop the records.
                            log(
                                Level::Error,
                                LOG_CONTEXT,
                                &format!(
                                    "handler rejected {topic}/{partition} and dead-lettering to \
                                     '{dlq}' failed ({dlq_err:?}); holding the partition rather \
                                     than losing {count} records"
                                ),
                            );
                            with_consumer(|consumer| {
                                consumer.rewind_partition(&topic, partition, first_offset);
                                Ok(())
                            })?;
                        }
                    }
                }
            }
        }
    }
    Ok(committed)
}

/// Write a batch that could not be handled to the dead-letter topic, tagged
/// with where it came from and why.
///
/// Provenance goes in record headers rather than the value, so a consumer of
/// the DLQ sees the original payload byte for byte — which is the point of
/// keeping it.
fn dead_letter(
    dlq: &str,
    source_topic: &str,
    partition: i32,
    records: &[KafkaRecord],
    reason: &str,
) -> Result<(), KafkaError> {
    let payload: Vec<(Option<Vec<u8>>, Vec<u8>)> = records
        .iter()
        .map(|r| (r.key.clone(), r.value.clone()))
        .collect();
    let headers = vec![
        ("dlq-source-topic", source_topic.as_bytes().to_vec()),
        ("dlq-source-partition", partition.to_string().into_bytes()),
        (
            "dlq-source-offset",
            records
                .first()
                .map(|r| r.offset.to_string())
                .unwrap_or_default()
                .into_bytes(),
        ),
        ("dlq-reason", reason.as_bytes().to_vec()),
    ];
    send_records_with_headers(dlq, &payload, &headers).map(|_| ())
}

/// The first running workload that exports the handler interface.
///
/// First, not balanced across *workloads*: a target handle scopes a whole task,
/// so sending different batches to different workloads would need a task each.
/// Concurrency here is across instances of the one workload, which is what the
/// host spins up per in-flight call and what a burst actually needs.
fn first_handler_workload() -> Option<(String, String)> {
    workload::callable()
        .into_iter()
        .find_map(|(id, interfaces)| {
            let interface = interfaces
                .into_iter()
                .find(|i| i.starts_with(HANDLER_INTERFACE))?;
            Some((id, interface))
        })
}

fn trigger_enabled() -> bool {
    matches!(
        config(CFG_TRIGGER).ok().flatten().as_deref().map(str::trim),
        Some("on") | Some("true") | Some("enabled")
    )
}

/// Whether to join the consumer group rather than statically taking every
/// partition. Defaults to joining: static assignment silently double-reads as
/// soon as a second consumer exists, which is not a safe default even though it
/// is the simpler one.
fn group_membership_enabled() -> Result<bool, KafkaError> {
    match config(CFG_ASSIGNMENT)?.as_deref().map(str::trim) {
        None | Some("") | Some("group") => Ok(true),
        Some("static") => Ok(false),
        Some(other) => Err(KafkaError::NotConfigured(format!(
            "config key '{CFG_ASSIGNMENT}' is '{other}'; expected 'group' or 'static'"
        ))),
    }
}

/// How long the coordinator waits before evicting this member. The broker
/// clamps it to its own configured range and answers `INVALID_SESSION_TIMEOUT`
/// rather than silently adjusting, so a rejected value is reported as it is.
fn session_timeout_ms() -> Result<i32, KafkaError> {
    let Some(raw) = config(CFG_SESSION_TIMEOUT)? else {
        return Ok(DEFAULT_SESSION_TIMEOUT_MS);
    };
    raw.trim().parse::<i32>().map_err(|_| {
        KafkaError::NotConfigured(format!(
            "config key '{CFG_SESSION_TIMEOUT}' is '{raw}'; expected a whole number of \
             milliseconds"
        ))
    })
}

fn trigger_max_attempts() -> u32 {
    config(CFG_TRIGGER_MAX_ATTEMPTS)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

fn dlq_topic(source: &str) -> String {
    config(CFG_TRIGGER_DLQ)
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("{source}.dlq"))
}

fn trigger_max_inflight() -> usize {
    config(CFG_TRIGGER_INFLIGHT)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TRIGGER_INFLIGHT)
}

fn trigger_batch_size() -> u32 {
    config(CFG_TRIGGER_BATCH)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TRIGGER_BATCH)
}

mod export {
    #![allow(unsafe_code)]
    use super::{bindings, Component};
    bindings::export!(Component with_types_in bindings);
}
