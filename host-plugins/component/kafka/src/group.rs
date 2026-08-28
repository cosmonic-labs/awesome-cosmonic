//! Consumer-group membership: `JoinGroup`, `SyncGroup`, `Heartbeat`, and
//! `LeaveGroup`.
//!
//! This is what stops two plugins in the same group from reading the same
//! partition. Static assignment — every consumer taking every partition —
//! is correct only while exactly one consumer exists, and nothing enforces
//! that: a second host running this plugin silently doubles every delivery,
//! and the two commit over each other's offsets. Kafka's answer is that
//! consumers *join* a group, a coordinator hands each a disjoint slice, and
//! every commit carries the generation it was made in so a member working
//! from a stale assignment is rejected rather than believed.
//!
//! Sent at v0 throughout. v0 predates flexible versions, so there are no
//! tagged fields or compact strings to encode, and every broker tested still
//! accepts it: Kafka 4.3.1 advertises `JoinGroup(11): 0 to 9`, and Redpanda
//! the same range. The later versions add incremental-rebalance machinery
//! (`KIP-394`'s member-id fencing, static membership) that this plugin does
//! not use.

use std::collections::BTreeMap;

use crate::fetch::{put_str, round_trip, BrokerConn, FetchError, Reader};

/// The generation moved on: this member's assignment is stale.
pub const ERR_ILLEGAL_GENERATION: i16 = 22;
/// The coordinator has forgotten this member — usually a missed heartbeat.
pub const ERR_UNKNOWN_MEMBER: i16 = 25;
/// A rebalance is under way; rejoin to take part in it.
pub const ERR_REBALANCE_IN_PROGRESS: i16 = 27;

/// The three errors that all mean the same thing to a caller: stop trusting
/// the current assignment and rejoin the group.
pub fn is_rejoin_signal(code: i16) -> bool {
    matches!(
        code,
        ERR_ILLEGAL_GENERATION | ERR_UNKNOWN_MEMBER | ERR_REBALANCE_IN_PROGRESS
    )
}

/// Identifies the embedded protocol. `consumer` is what every Kafka consumer
/// client uses, and matching it is what lets this plugin share a group with
/// one rather than merely with copies of itself.
const PROTOCOL_TYPE: &str = "consumer";
/// The assignment strategy this member proposes. `range` is Java's classic
/// default, so a group mixing this plugin and a stock consumer agrees on one
/// strategy instead of failing to.
const PROTOCOL_NAME: &str = "range";

const CLIENT_ID: &str = "wasmcloud-kafka-plugin";

/// A member's place in the group, as the coordinator sees it.
pub struct Membership {
    /// Increments on every rebalance. Sent with each commit, so a commit from
    /// a previous generation is rejected instead of silently applied.
    pub generation: i32,
    /// Assigned by the coordinator on the first join, and reused after.
    pub member_id: String,
}

/// What `JoinGroup` answered.
pub struct JoinResult {
    pub generation: i32,
    pub member_id: String,
    /// Whether this member has to compute the assignment for the whole group.
    /// The coordinator elects one member the leader; assignment is done
    /// client-side, which is why the strategy is a protocol name rather than a
    /// broker setting.
    pub leader: bool,
    /// `(member_id, subscribed_topics)`, non-empty only for the leader.
    pub members: Vec<(String, Vec<String>)>,
}

fn put_bytes(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(&(data.len() as i32).to_be_bytes());
    buf.extend_from_slice(data);
}

fn header(buf: &mut Vec<u8>, api_key: i16, version: i16, correlation: i32) {
    buf.extend_from_slice(&api_key.to_be_bytes());
    buf.extend_from_slice(&version.to_be_bytes());
    buf.extend_from_slice(&correlation.to_be_bytes());
    put_str(buf, CLIENT_ID);
}

fn check_correlation(r: &mut Reader<'_>, expected: i32) -> Result<(), FetchError> {
    let got = r.i32()?;
    if got != expected {
        return Err(FetchError::Protocol(format!(
            "correlation id mismatch: expected {expected}, got {got}"
        )));
    }
    Ok(())
}

/// `ConsumerProtocolSubscription` v0 — what this member wants to read.
fn encode_subscription(topics: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + topics.iter().map(|t| t.len() + 2).sum::<usize>());
    out.extend_from_slice(&0i16.to_be_bytes()); // version
    out.extend_from_slice(&(topics.len() as i32).to_be_bytes());
    for t in topics {
        put_str(&mut out, t);
    }
    out.extend_from_slice(&(-1i32).to_be_bytes()); // user_data: none
    out
}

fn decode_subscription(data: &[u8]) -> Result<Vec<String>, FetchError> {
    let mut r = Reader::new(data);
    let _version = r.i16()?;
    let count = r.i32()?;
    let mut topics = Vec::with_capacity(count.max(0) as usize);
    for _ in 0..count.max(0) {
        topics.push(r.string()?);
    }
    Ok(topics)
}

/// `ConsumerProtocolAssignment` v0 — what one member was given.
fn encode_assignment(topics: &BTreeMap<String, Vec<i32>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(&0i16.to_be_bytes()); // version
    out.extend_from_slice(&(topics.len() as i32).to_be_bytes());
    for (topic, partitions) in topics {
        put_str(&mut out, topic);
        out.extend_from_slice(&(partitions.len() as i32).to_be_bytes());
        for p in partitions {
            out.extend_from_slice(&p.to_be_bytes());
        }
    }
    out.extend_from_slice(&(-1i32).to_be_bytes()); // user_data: none
    out
}

fn decode_assignment(data: &[u8]) -> Result<Vec<(String, i32)>, FetchError> {
    if data.is_empty() {
        // A member subscribed to nothing the group covers gets an empty
        // assignment rather than an error.
        return Ok(Vec::new());
    }
    let mut r = Reader::new(data);
    let _version = r.i16()?;
    let topic_count = r.i32()?;
    let mut out = Vec::new();
    for _ in 0..topic_count.max(0) {
        let topic = r.string()?;
        let partition_count = r.i32()?;
        for _ in 0..partition_count.max(0) {
            out.push((topic.clone(), r.i32()?));
        }
    }
    out.sort();
    Ok(out)
}

/// The `range` assignment strategy, as Java's `RangeAssignor` defines it: per
/// topic, lay the partitions out in order and cut them into as many contiguous
/// runs as there are members subscribed to that topic, giving the earlier
/// members the extra partition when the split is uneven.
///
/// Contiguous-per-topic rather than round-robin, so a member reading several
/// co-partitioned topics gets the *same* partition number in each — which is
/// what makes a join across two topics work on one consumer.
///
/// Pure, so it can be tested without a broker. Every member appears in the
/// result, including ones that get nothing.
pub fn range_assign(
    members: &[(String, Vec<String>)],
    partitions_of: &dyn Fn(&str) -> Vec<i32>,
) -> BTreeMap<String, BTreeMap<String, Vec<i32>>> {
    let mut out: BTreeMap<String, BTreeMap<String, Vec<i32>>> = members
        .iter()
        .map(|(id, _)| (id.clone(), BTreeMap::new()))
        .collect();

    let mut all_topics: Vec<&String> = members.iter().flat_map(|(_, ts)| ts).collect();
    all_topics.sort();
    all_topics.dedup();

    for topic in all_topics {
        let mut subscribers: Vec<&String> = members
            .iter()
            .filter(|(_, ts)| ts.contains(topic))
            .map(|(id, _)| id)
            .collect();
        // Sorted by member id: every member runs this same computation on the
        // same inputs only if the order is fixed, and the coordinator does not
        // impose one.
        subscribers.sort();
        if subscribers.is_empty() {
            continue;
        }

        let mut partitions = partitions_of(topic);
        partitions.sort();
        let n = partitions.len();
        let m = subscribers.len();
        let per = n / m;
        let extra = n % m;

        let mut start = 0usize;
        for (i, member) in subscribers.iter().enumerate() {
            let take = per + usize::from(i < extra);
            if take > 0 {
                out.entry((*member).clone())
                    .or_default()
                    .insert(topic.clone(), partitions[start..start + take].to_vec());
            }
            start += take;
        }
    }
    out
}

impl BrokerConn {
    /// Join the group, or rejoin it after a rebalance.
    ///
    /// Blocks until the rebalance completes — the coordinator holds every
    /// member's request open until the last one arrives or the rebalance
    /// timeout expires, which is how it gets them all onto one generation.
    /// First join sends an empty `member_id` and is told what it is.
    pub fn join_group(
        &mut self,
        group: &str,
        member_id: &str,
        topics: &[String],
        session_timeout_ms: i32,
    ) -> Result<JoinResult, FetchError> {
        let (correlation, stream) = self.next_request();
        let subscription = encode_subscription(topics);

        let mut req = Vec::with_capacity(128 + group.len() + subscription.len());
        header(&mut req, 11, 0, correlation); // JoinGroup v0
        put_str(&mut req, group);
        req.extend_from_slice(&session_timeout_ms.to_be_bytes());
        put_str(&mut req, member_id);
        put_str(&mut req, PROTOCOL_TYPE);
        req.extend_from_slice(&1i32.to_be_bytes()); // one candidate protocol
        put_str(&mut req, PROTOCOL_NAME);
        put_bytes(&mut req, &subscription);

        let resp = round_trip(stream, &req)?;
        let mut r = Reader::new(&resp);
        check_correlation(&mut r, correlation)?;

        let error = r.i16()?;
        if error != 0 {
            return Err(FetchError::Broker(error));
        }
        let generation = r.i32()?;
        let _protocol = r.string()?;
        let leader_id = r.string()?;
        let member_id = r.string()?;
        let leader = leader_id == member_id;

        let member_count = r.i32()?;
        let mut members = Vec::with_capacity(member_count.max(0) as usize);
        for _ in 0..member_count.max(0) {
            let id = r.string()?;
            let meta_len = r.i32()?;
            let meta = if meta_len < 0 {
                &[][..]
            } else {
                r.bytes(meta_len as usize)?
            };
            members.push((id, decode_subscription(meta)?));
        }

        Ok(JoinResult {
            generation,
            member_id,
            leader,
            members,
        })
    }

    /// Collect this member's assignment. The leader passes the assignment it
    /// computed for everyone; a follower passes an empty list and is simply
    /// told its own.
    pub fn sync_group(
        &mut self,
        group: &str,
        generation: i32,
        member_id: &str,
        assignments: &BTreeMap<String, BTreeMap<String, Vec<i32>>>,
    ) -> Result<Vec<(String, i32)>, FetchError> {
        let (correlation, stream) = self.next_request();
        let mut req = Vec::with_capacity(128 + group.len());
        header(&mut req, 14, 0, correlation); // SyncGroup v0
        put_str(&mut req, group);
        req.extend_from_slice(&generation.to_be_bytes());
        put_str(&mut req, member_id);
        req.extend_from_slice(&(assignments.len() as i32).to_be_bytes());
        for (member, topics) in assignments {
            put_str(&mut req, member);
            put_bytes(&mut req, &encode_assignment(topics));
        }

        let resp = round_trip(stream, &req)?;
        let mut r = Reader::new(&resp);
        check_correlation(&mut r, correlation)?;

        let error = r.i16()?;
        if error != 0 {
            return Err(FetchError::Broker(error));
        }
        let len = r.i32()?;
        let raw = if len < 0 {
            &[][..]
        } else {
            r.bytes(len as usize)?
        };
        decode_assignment(raw)
    }

    /// Tell the coordinator this member is still alive.
    ///
    /// The answer is the rebalance signal: a member that stops hearing `0` here
    /// has lost its partitions and must rejoin before reading any more.
    pub fn heartbeat(
        &mut self,
        group: &str,
        generation: i32,
        member_id: &str,
    ) -> Result<(), FetchError> {
        let (correlation, stream) = self.next_request();
        let mut req = Vec::with_capacity(64 + group.len());
        header(&mut req, 12, 0, correlation); // Heartbeat v0
        put_str(&mut req, group);
        req.extend_from_slice(&generation.to_be_bytes());
        put_str(&mut req, member_id);

        let resp = round_trip(stream, &req)?;
        let mut r = Reader::new(&resp);
        check_correlation(&mut r, correlation)?;
        match r.i16()? {
            0 => Ok(()),
            other => Err(FetchError::Broker(other)),
        }
    }

    /// Leave deliberately, so the group rebalances now rather than after the
    /// session timeout expires.
    pub fn leave_group(&mut self, group: &str, member_id: &str) -> Result<(), FetchError> {
        let (correlation, stream) = self.next_request();
        let mut req = Vec::with_capacity(64 + group.len());
        header(&mut req, 13, 0, correlation); // LeaveGroup v0
        put_str(&mut req, group);
        put_str(&mut req, member_id);

        let resp = round_trip(stream, &req)?;
        let mut r = Reader::new(&resp);
        check_correlation(&mut r, correlation)?;
        match r.i16()? {
            0 => Ok(()),
            other => Err(FetchError::Broker(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(ids: &[&str], topics: &[&str]) -> Vec<(String, Vec<String>)> {
        ids.iter()
            .map(|id| {
                (
                    (*id).to_owned(),
                    topics.iter().map(|t| (*t).to_owned()).collect(),
                )
            })
            .collect()
    }

    /// The property that makes group membership worth having: no partition is
    /// assigned twice, and none is dropped. Checked over an awkward split
    /// (7 across 3) rather than an even one.
    #[test]
    fn every_partition_is_assigned_exactly_once() {
        let ms = members(&["a", "b", "c"], &["orders"]);
        let assigned = range_assign(&ms, &|_| (0..7).collect());

        let mut seen: Vec<i32> = assigned
            .values()
            .flat_map(|topics| topics.values().flatten().copied())
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            (0..7).collect::<Vec<_>>(),
            "no gaps and no duplicates"
        );

        // 7 across 3 is 3/2/2, the extra going to the first member.
        let counts: Vec<usize> = assigned
            .values()
            .map(|t| t.values().map(Vec::len).sum())
            .collect();
        assert_eq!(counts, vec![3, 2, 2]);
    }

    /// Each member's run is contiguous, which is what `range` means and what a
    /// co-partitioned join across topics depends on.
    #[test]
    fn ranges_are_contiguous_and_aligned_across_topics() {
        let ms = members(&["a", "b"], &["left", "right"]);
        let assigned = range_assign(&ms, &|_| (0..4).collect());

        assert_eq!(assigned["a"]["left"], vec![0, 1]);
        assert_eq!(assigned["a"]["right"], vec![0, 1]);
        assert_eq!(assigned["b"]["left"], vec![2, 3]);
        assert_eq!(
            assigned["b"]["right"],
            vec![2, 3],
            "the same member gets the same partition number in each topic"
        );
    }

    /// More members than partitions is the case that decides whether a second
    /// plugin double-reads: the surplus members must get *nothing*, not a copy.
    #[test]
    fn surplus_members_get_no_partitions() {
        let ms = members(&["a", "b", "c"], &["single"]);
        let assigned = range_assign(&ms, &|_| vec![0]);

        let total: usize = assigned
            .values()
            .map(|t| t.values().map(Vec::len).sum::<usize>())
            .sum();
        assert_eq!(total, 1, "one partition, assigned once");
        assert_eq!(assigned["a"]["single"], vec![0]);
        assert!(
            assigned["b"].is_empty(),
            "b idles rather than double-reading"
        );
        assert!(assigned["c"].is_empty());
    }

    /// Members subscribed to different topics divide only what they asked for.
    #[test]
    fn assignment_respects_differing_subscriptions() {
        let ms = vec![
            (
                "a".to_owned(),
                vec!["shared".to_owned(), "only-a".to_owned()],
            ),
            ("b".to_owned(), vec!["shared".to_owned()]),
        ];
        let assigned = range_assign(&ms, &|_| (0..2).collect());

        assert_eq!(assigned["a"]["shared"], vec![0]);
        assert_eq!(assigned["b"]["shared"], vec![1]);
        assert_eq!(
            assigned["a"]["only-a"],
            vec![0, 1],
            "a topic only one member wants is not split"
        );
        assert!(!assigned["b"].contains_key("only-a"));
    }

    /// The wire encodings, round-tripped: a subscription this plugin sends is
    /// one it can also read back, which is exactly what the group leader has to
    /// do with every other member's.
    #[test]
    fn subscription_and_assignment_round_trip() {
        let topics = vec!["orders".to_owned(), "events".to_owned()];
        assert_eq!(
            decode_subscription(&encode_subscription(&topics)).unwrap(),
            topics
        );

        let mut assignment = BTreeMap::new();
        assignment.insert("orders".to_owned(), vec![0, 2]);
        assignment.insert("events".to_owned(), vec![1]);
        assert_eq!(
            decode_assignment(&encode_assignment(&assignment)).unwrap(),
            vec![
                ("events".to_owned(), 1),
                ("orders".to_owned(), 0),
                ("orders".to_owned(), 2)
            ]
        );

        assert!(
            decode_assignment(&[]).unwrap().is_empty(),
            "an empty assignment is a member with nothing to do, not an error"
        );
    }
}
