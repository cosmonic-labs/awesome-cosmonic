//! A `Produce` v3 client, because `kafka-rust` 0.10's is v0 and Kafka 4.x
//! removed it.
//!
//! The same shape of problem as [`crate::fetch`], with a twist worth recording:
//! Kafka 4.3.1's `ApiVersions` response *advertises* `Produce v0..v13`, and then
//! rejects a v0 request with
//! `UnsupportedVersionException: unsupported version 0` — closing the socket
//! rather than answering, which is why the client only ever sees
//! `UnexpectedEof`. The advertised floor cannot be trusted for this API; v3 is
//! the real one (Produce v0-v2 went in Kafka 4.0).
//!
//! v3 is also where the request carries a v2 `RecordBatch` instead of the old
//! message set — the same format the fetch path already decodes, so producing
//! and consuming now agree on one record format.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Kafka's `Produce` API key.
const API_KEY_PRODUCE: i16 = 0;
/// The lowest version Kafka 4.x accepts, and the first that carries v2 record
/// batches. Newer versions add transactional and idempotence fields this does
/// not use.
const API_VERSION_PRODUCE: i16 = 3;

/// Where a produced record landed.
pub struct ProducedAt {
    pub partition: i32,
    pub offset: i64,
}

#[derive(Debug)]
pub enum ProduceError {
    Io(std::io::Error),
    /// The broker answered with an error code for this partition.
    Broker(i16),
    Protocol(String),
}

impl From<std::io::Error> for ProduceError {
    fn from(e: std::io::Error) -> Self {
        ProduceError::Io(e)
    }
}

/// CRC-32C (Castagnoli), which is what a v2 record batch's `crc` field is —
/// *not* the CRC-32 the older message format used. A wrong checksum is rejected
/// as a corrupt record, which reads like a network fault rather than an
/// encoding bug.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    !crc
}

fn put_varint(buf: &mut Vec<u8>, value: i64) {
    let mut raw = ((value << 1) ^ (value >> 63)) as u64;
    loop {
        let mut byte = (raw & 0x7f) as u8;
        raw >>= 7;
        if raw != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if raw == 0 {
            break;
        }
    }
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as i16).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// Encode records as one v2 `RecordBatch`.
///
/// Timestamps are the batch's `first_timestamp` plus a per-record delta, and
/// offsets are deltas from a base of 0 — the broker assigns the real ones and
/// reports the base back.
/// `headers` is applied to every record in the batch. Batch-level rather than
/// per-record because the only caller that needs headers — dead-lettering —
/// moves one partition's batch at a time, and every record in it shares the
/// same provenance.
pub fn encode_batch(
    records: &[(Option<Vec<u8>>, Vec<u8>)],
    timestamp_ms: i64,
    codec: i16,
    headers: &[(&str, Vec<u8>)],
) -> Vec<u8> {
    let mut body = Vec::new();
    // Everything from `attributes` on is what the CRC covers.
    // The codec lives in the low three bits of `attributes`; the rest of the
    // header stays plain so a reader can find the codec before decompressing.
    body.extend_from_slice(&(codec & 0x07).to_be_bytes());
    body.extend_from_slice(&((records.len() as i32) - 1).to_be_bytes()); // last offset delta
    body.extend_from_slice(&timestamp_ms.to_be_bytes()); // first timestamp
    body.extend_from_slice(&timestamp_ms.to_be_bytes()); // max timestamp
    body.extend_from_slice(&(-1i64).to_be_bytes()); // producer id: not idempotent
    body.extend_from_slice(&(-1i16).to_be_bytes()); // producer epoch
    body.extend_from_slice(&(-1i32).to_be_bytes()); // base sequence
    body.extend_from_slice(&(records.len() as i32).to_be_bytes());

    let mut records_section = Vec::new();
    for (i, (key, value)) in records.iter().enumerate() {
        let mut rec = Vec::new();
        rec.push(0u8); // record attributes: unused, must be zero
        put_varint(&mut rec, 0); // timestamp delta
        put_varint(&mut rec, i as i64); // offset delta
        match key {
            Some(k) => {
                put_varint(&mut rec, k.len() as i64);
                rec.extend_from_slice(k);
            }
            // -1 is an absent key, which is distinct from a zero-length one.
            None => put_varint(&mut rec, -1),
        }
        put_varint(&mut rec, value.len() as i64);
        rec.extend_from_slice(value);
        put_varint(&mut rec, headers.len() as i64);
        for (name, value) in headers {
            put_varint(&mut rec, name.len() as i64);
            rec.extend_from_slice(name.as_bytes());
            put_varint(&mut rec, value.len() as i64);
            rec.extend_from_slice(value);
        }

        put_varint(&mut records_section, rec.len() as i64);
        records_section.extend_from_slice(&rec);
    }

    // Only the records are compressed, never the header.
    let records_section = match codec & 0x07 {
        0 => records_section,
        1 => {
            use std::io::Write as _;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let _ = enc.write_all(&records_section);
            enc.finish().unwrap_or_default()
        }
        2 => snap::raw::Encoder::new()
            .compress_vec(&records_section)
            .unwrap_or_default(),
        _ => records_section,
    };
    body.extend_from_slice(&records_section);

    // The header before the CRC: base offset, length, leader epoch, magic. The
    // CRC is computed over everything after it, so it is written last.
    let mut batch = Vec::with_capacity(body.len() + 64);
    batch.extend_from_slice(&0i64.to_be_bytes()); // base offset; broker assigns
    let length_at = batch.len();
    batch.extend_from_slice(&0i32.to_be_bytes()); // batch length, filled below
    batch.extend_from_slice(&(-1i32).to_be_bytes()); // partition leader epoch
    batch.push(2); // magic
    let crc_at = batch.len();
    batch.extend_from_slice(&0u32.to_be_bytes()); // crc, filled below
    batch.extend_from_slice(&body);

    let crc = crc32c(&batch[crc_at + 4..]);
    batch[crc_at..crc_at + 4].copy_from_slice(&crc.to_be_bytes());
    // Length counts everything after the length field itself.
    let len = (batch.len() - length_at - 4) as i32;
    batch[length_at..length_at + 4].copy_from_slice(&len.to_be_bytes());
    batch
}

/// Send one `Produce` v3 request for a single topic-partition.
pub fn produce(
    stream: &mut TcpStream,
    correlation: i32,
    topic: &str,
    partition: i32,
    acks: i16,
    timeout: Duration,
    batch: &[u8],
) -> Result<ProducedAt, ProduceError> {
    let mut req = Vec::with_capacity(batch.len() + 128);
    req.extend_from_slice(&API_KEY_PRODUCE.to_be_bytes());
    req.extend_from_slice(&API_VERSION_PRODUCE.to_be_bytes());
    req.extend_from_slice(&correlation.to_be_bytes());
    put_str(&mut req, "wasmcloud-kafka-plugin");

    req.extend_from_slice(&(-1i16).to_be_bytes()); // transactional id: none
    req.extend_from_slice(&acks.to_be_bytes());
    req.extend_from_slice(&(timeout.as_millis() as i32).to_be_bytes());
    req.extend_from_slice(&1i32.to_be_bytes()); // one topic
    put_str(&mut req, topic);
    req.extend_from_slice(&1i32.to_be_bytes()); // one partition
    req.extend_from_slice(&partition.to_be_bytes());
    req.extend_from_slice(&(batch.len() as i32).to_be_bytes());
    req.extend_from_slice(batch);

    stream.write_all(&(req.len() as i32).to_be_bytes())?;
    stream.write_all(&req)?;
    stream.flush()?;

    // acks=0 means the broker sends nothing back, so there is no offset to
    // report and nothing to wait for.
    if acks == 0 {
        return Ok(ProducedAt {
            partition,
            offset: -1,
        });
    }

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = i32::from_be_bytes(len_buf);
    if len <= 0 {
        return Err(ProduceError::Protocol(format!(
            "broker announced a {len}-byte response"
        )));
    }
    let mut resp = vec![0u8; len as usize];
    stream.read_exact(&mut resp)?;
    decode_response(&resp, correlation)
}

fn decode_response(resp: &[u8], correlation: i32) -> Result<ProducedAt, ProduceError> {
    let at = |i: usize, n: usize| -> Result<&[u8], ProduceError> {
        resp.get(i..i + n)
            .ok_or_else(|| ProduceError::Protocol("response truncated".to_owned()))
    };
    let got = i32::from_be_bytes(at(0, 4)?.try_into().unwrap());
    if got != correlation {
        return Err(ProduceError::Protocol(format!(
            "correlation id mismatch: expected {correlation}, got {got}"
        )));
    }
    let mut i = 4;
    let topics = i32::from_be_bytes(at(i, 4)?.try_into().unwrap());
    i += 4;
    // One topic and one partition per request, so exactly one of each comes
    // back. A different shape is the broker answering a question that was not
    // asked, which is worth reporting rather than iterating over.
    if topics != 1 {
        return Err(ProduceError::Protocol(format!(
            "produced to one topic, got {topics} in the response"
        )));
    }
    let name_len = i16::from_be_bytes(at(i, 2)?.try_into().unwrap()) as usize;
    i += 2 + name_len;
    let partitions = i32::from_be_bytes(at(i, 4)?.try_into().unwrap());
    i += 4;
    if partitions != 1 {
        return Err(ProduceError::Protocol(format!(
            "produced to one partition, got {partitions} in the response"
        )));
    }

    let partition = i32::from_be_bytes(at(i, 4)?.try_into().unwrap());
    i += 4;
    let error = i16::from_be_bytes(at(i, 2)?.try_into().unwrap());
    i += 2;
    let offset = i64::from_be_bytes(at(i, 8)?.try_into().unwrap());
    i += 8;
    let _log_append_time = i64::from_be_bytes(at(i, 8)?.try_into().unwrap());
    if error != 0 {
        return Err(ProduceError::Broker(error));
    }
    Ok(ProducedAt { partition, offset })
}

/// Kafka's partitioner for a keyed record: murmur2 of the key, masked positive,
/// modulo the partition count.
///
/// Reproduced rather than invented, because the point of a key is that the same
/// key lands on the same partition as every other client's — a different hash
/// would silently break ordering for anyone else reading the topic.
pub fn partition_for_key(key: &[u8], partitions: i32) -> i32 {
    (murmur2(key) & 0x7fff_ffff) % partitions.max(1)
}

fn murmur2(data: &[u8]) -> i32 {
    const SEED: u32 = 0x9747_b28c;
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;

    let len = data.len();
    let mut h: u32 = SEED ^ (len as u32);
    let chunks = len / 4;

    for i in 0..chunks {
        let o = i * 4;
        let mut k = u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M);
        h ^= k;
    }

    let tail = chunks * 4;
    match len - tail {
        3 => {
            h ^= u32::from(data[tail + 2]) << 16;
            h ^= u32::from(data[tail + 1]) << 8;
            h ^= u32::from(data[tail]);
            h = h.wrapping_mul(M);
        }
        2 => {
            h ^= u32::from(data[tail + 1]) << 8;
            h ^= u32::from(data[tail]);
            h = h.wrapping_mul(M);
        }
        1 => {
            h ^= u32::from(data[tail]);
            h = h.wrapping_mul(M);
        }
        _ => {}
    }

    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked against an independent bitwise implementation of the Castagnoli
    /// polynomial, not a constant recalled from memory. A v2 batch uses CRC-32C
    /// where the old format used CRC-32, and getting that wrong is reported by
    /// the broker as a corrupt record.
    #[test]
    fn crc32c_matches_the_reference_polynomial() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"hello world"), 0xC994_65AA);
    }

    /// The batch has to be self-describing: the length field counts everything
    /// after itself, and the CRC covers everything after the CRC field. Both
    /// are written after the fact, so both are easy to get off by four bytes.
    #[test]
    fn a_batch_declares_its_own_length_and_checksum() {
        let batch = encode_batch(&[(None, b"hello".to_vec())], 1_700_000_000_000, 0, &[]);

        let declared = i32::from_be_bytes(batch[8..12].try_into().unwrap()) as usize;
        assert_eq!(
            declared,
            batch.len() - 12,
            "batch length counts every byte after the length field"
        );
        assert_eq!(batch[16], 2, "magic byte identifies a v2 record batch");

        let declared_crc = u32::from_be_bytes(batch[17..21].try_into().unwrap());
        assert_eq!(
            declared_crc,
            crc32c(&batch[21..]),
            "the CRC covers everything after itself"
        );
    }

    /// An absent key and an empty key are different records, and the difference
    /// is a `-1` length rather than a `0` — collapsing them changes which
    /// partition a record lands on.
    #[test]
    fn an_absent_key_encodes_differently_from_an_empty_one() {
        let absent = encode_batch(&[(None, b"v".to_vec())], 0, 0, &[]);
        let empty = encode_batch(&[(Some(Vec::new()), b"v".to_vec())], 0, 0, &[]);
        assert_ne!(absent, empty);
    }

    #[test]
    fn a_batch_carries_every_record() {
        let batch = encode_batch(
            &[
                (Some(b"k1".to_vec()), b"v1".to_vec()),
                (None, b"v2".to_vec()),
                (Some(b"k3".to_vec()), b"v3".to_vec()),
            ],
            42,
            0,
            &[],
        );
        let count = i32::from_be_bytes(batch[57..61].try_into().unwrap());
        assert_eq!(count, 3);
        let last_delta = i32::from_be_bytes(batch[23..27].try_into().unwrap());
        assert_eq!(last_delta, 2, "last offset delta is count - 1");
    }

    /// A dead-lettered record has to say where it came from, and headers are
    /// where that belongs — mangling the value would change the payload a
    /// consumer of the DLQ is trying to inspect.
    #[test]
    fn headers_are_written_on_every_record() {
        let plain = encode_batch(&[(None, b"v".to_vec()), (None, b"w".to_vec())], 0, 0, &[]);
        let tagged = encode_batch(
            &[(None, b"v".to_vec()), (None, b"w".to_vec())],
            0,
            0,
            &[("source-topic", b"orders".to_vec())],
        );
        assert!(tagged.len() > plain.len(), "headers add bytes");
        let count = tagged
            .windows(b"source-topic".len())
            .filter(|w| *w == b"source-topic")
            .count();
        assert_eq!(count, 2, "one copy per record, not one per batch");
    }

    /// The same key must land on the same partition every time, and stay within
    /// range. This is Kafka's own partitioner, so a record keyed here reaches
    /// the partition any other client would send it to.
    #[test]
    fn keyed_partitioning_is_stable_and_in_range() {
        for key in [&b"user-1"[..], b"user-2", b"", b"a-much-longer-key-value"] {
            let p = partition_for_key(key, 6);
            assert!((0..6).contains(&p), "{p} out of range for key {key:?}");
            assert_eq!(p, partition_for_key(key, 6), "must be deterministic");
        }
        assert_ne!(
            partition_for_key(b"user-1", 6),
            partition_for_key(b"user-4", 6),
            "distinct keys should generally spread"
        );
    }
}
