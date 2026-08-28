//! A `Fetch` v4 client, because `kafka-rust` 0.10's is v0 and no current broker
//! accepts that.
//!
//! Everything else `kafka-rust` does still works against a modern broker —
//! metadata, offset lookup, group coordination, producing — so this replaces one
//! API rather than the client. Asking a broker directly is what pins it down:
//!
//! ```text
//! Produce   (key 0)  v0..v7      <- kafka-rust's v0 accepted, so produce works
//! Fetch     (key 1)  v4..v13     <- kafka-rust's v0 refused, so consume does not
//! ```
//!
//! v4 is the floor, so v4 is what this sends. That choice also fixes the record
//! format: a v4 response carries v2 `RecordBatch`es, which `kafka-rust` cannot
//! parse at all (it accepts only magic byte 0, "kafka 0.8 and 0.9"). Decoding
//! them is most of the code below — zigzag varints for every length and delta,
//! and a batch header whose timestamps mean per-record timestamps finally
//! survive to the caller.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Kafka's `Fetch` API key.
const API_KEY_FETCH: i16 = 1;
/// The lowest version any current broker accepts, and all this needs: newer
/// versions add features (incremental fetch sessions, topic ids) that a single
/// polling consumer does not use.
const API_VERSION_FETCH: i16 = 4;

/// A record as it came off the wire.
#[derive(Debug)]
pub struct FetchedRecord {
    pub partition: i32,
    pub offset: i64,
    pub key: Option<Vec<u8>>,
    pub value: Vec<u8>,
    pub timestamp_ms: i64,
}

#[derive(Debug)]
pub enum FetchError {
    /// The connection failed or was closed mid-response. The caller should drop
    /// the connection: a partially-read response leaves it desynchronised.
    Io(std::io::Error),
    /// The broker answered with an error code for this partition.
    Broker(i16),
    /// The broker's answer did not decode.
    Protocol(String),
}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        FetchError::Io(e)
    }
}

/// Kafka error code for a fetch offset outside the partition's retained range,
/// which a caller resolves by resetting rather than retrying.
pub const ERR_OFFSET_OUT_OF_RANGE: i16 = 1;
/// Kafka error code for a stale leader; the caller must reload metadata.
pub const ERR_NOT_LEADER: i16 = 6;

/// One long-lived connection to one broker.
///
/// Held across calls on purpose: re-handshaking TCP per poll is exactly the
/// per-request cost this plugin exists to keep off the workloads.
pub struct BrokerConn {
    stream: TcpStream,
    correlation: i32,
}

impl BrokerConn {
    pub fn connect(addr: &str) -> Result<Self, FetchError> {
        let stream = TcpStream::connect(addr)?;
        // Nagle would coalesce a request that is already one write.
        let _ = stream.set_nodelay(true);
        Ok(Self {
            stream,
            correlation: 0,
        })
    }

    /// Claim the next correlation id and the underlying stream, so another API
    /// can be sent on this same connection.
    ///
    /// One connection per broker serves every request this plugin makes to it;
    /// the correlation counter lives here so two APIs sharing a socket cannot
    /// collide on it.
    pub fn next_request(&mut self) -> (i32, &mut TcpStream) {
        self.correlation = self.correlation.wrapping_add(1);
        (self.correlation, &mut self.stream)
    }

    /// The earliest (`timestamp` -2) or latest (-1) offset of one partition.
    ///
    /// `ListOffsets` v1 rather than v0, because Kafka 4.x refuses v0 — the
    /// third API where `kafka-rust`'s hardcoded v0 is rejected, after `Fetch`
    /// and `Produce`. v1 also drops the `max_num_offsets` field and returns one
    /// offset with its timestamp.
    pub fn list_offset(
        &mut self,
        topic: &str,
        partition: i32,
        timestamp: i64,
    ) -> Result<i64, FetchError> {
        self.correlation = self.correlation.wrapping_add(1);
        let correlation = self.correlation;

        let mut req = Vec::with_capacity(64 + topic.len());
        req.extend_from_slice(&2i16.to_be_bytes()); // ListOffsets
        req.extend_from_slice(&1i16.to_be_bytes()); // v1
        req.extend_from_slice(&correlation.to_be_bytes());
        put_str(&mut req, "wasmcloud-kafka-plugin");
        req.extend_from_slice(&(-1i32).to_be_bytes()); // replica_id: not a broker
        req.extend_from_slice(&1i32.to_be_bytes()); // one topic
        put_str(&mut req, topic);
        req.extend_from_slice(&1i32.to_be_bytes()); // one partition
        req.extend_from_slice(&partition.to_be_bytes());
        req.extend_from_slice(&timestamp.to_be_bytes());

        self.stream.write_all(&(req.len() as i32).to_be_bytes())?;
        self.stream.write_all(&req)?;
        self.stream.flush()?;

        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = i32::from_be_bytes(len_buf);
        if len <= 0 {
            return Err(FetchError::Protocol(format!(
                "broker announced a {len}-byte response"
            )));
        }
        let mut resp = vec![0u8; len as usize];
        self.stream.read_exact(&mut resp)?;

        let mut r = Reader::new(&resp);
        let got = r.i32()?;
        if got != correlation {
            return Err(FetchError::Protocol(format!(
                "correlation id mismatch: expected {correlation}, got {got}"
            )));
        }
        expect_single_partition(&mut r)?;
        let _partition = r.i32()?;
        let error = r.i16()?;
        let _timestamp = r.i64()?;
        let offset = r.i64()?;
        if error != 0 {
            return Err(FetchError::Broker(error));
        }
        Ok(offset)
    }

    /// Commit one partition's offset for a consumer group.
    ///
    /// `OffsetCommit` v2, because Kafka 4.x refuses v1 — the fourth API where
    /// `kafka-rust`'s version is too old. v2 carries the group generation and
    /// member id, which a non-member commit sends as `-1` and `""`: the
    /// "simple consumer" form, and the right one here because this plugin
    /// assigns partitions statically rather than joining the group.
    ///
    /// `retention_time` of `-1` means the broker's own default.
    pub fn commit_offset(
        &mut self,
        group: &str,
        generation: i32,
        member_id: &str,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> Result<(), FetchError> {
        self.correlation = self.correlation.wrapping_add(1);
        let correlation = self.correlation;

        let mut req = Vec::with_capacity(96 + topic.len() + group.len());
        req.extend_from_slice(&8i16.to_be_bytes()); // OffsetCommit
        req.extend_from_slice(&2i16.to_be_bytes()); // v2
        req.extend_from_slice(&correlation.to_be_bytes());
        put_str(&mut req, "wasmcloud-kafka-plugin");
        put_str(&mut req, group);
        // A real generation and member id when this consumer joined the group,
        // and the "simple consumer" sentinels (-1, "") when it did not. With
        // them the coordinator fences the commit: a member whose assignment has
        // been taken away is answered ILLEGAL_GENERATION instead of being
        // allowed to move another member's offsets.
        req.extend_from_slice(&generation.to_be_bytes());
        put_str(&mut req, member_id);
        req.extend_from_slice(&(-1i64).to_be_bytes()); // retention: broker default
        req.extend_from_slice(&1i32.to_be_bytes()); // one topic
        put_str(&mut req, topic);
        req.extend_from_slice(&1i32.to_be_bytes()); // one partition
        req.extend_from_slice(&partition.to_be_bytes());
        req.extend_from_slice(&offset.to_be_bytes());
        put_str(&mut req, ""); // metadata

        self.stream.write_all(&(req.len() as i32).to_be_bytes())?;
        self.stream.write_all(&req)?;
        self.stream.flush()?;

        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = i32::from_be_bytes(len_buf);
        if len <= 0 {
            return Err(FetchError::Protocol(format!(
                "broker announced a {len}-byte response"
            )));
        }
        let mut resp = vec![0u8; len as usize];
        self.stream.read_exact(&mut resp)?;

        let mut r = Reader::new(&resp);
        let got = r.i32()?;
        if got != correlation {
            return Err(FetchError::Protocol(format!(
                "correlation id mismatch: expected {correlation}, got {got}"
            )));
        }
        expect_single_partition(&mut r)?;
        let _partition = r.i32()?;
        match r.i16()? {
            0 => Ok(()),
            error => Err(FetchError::Broker(error)),
        }
    }

    /// The broker coordinating a consumer group, as `host:port`.
    ///
    /// An offset commit has to go to the coordinator, not to the partition's
    /// leader — a distinction that does not matter on a single-broker cluster
    /// and matters immediately on any real one.
    pub fn find_coordinator(&mut self, group: &str) -> Result<String, FetchError> {
        self.correlation = self.correlation.wrapping_add(1);
        let correlation = self.correlation;

        let mut req = Vec::with_capacity(64 + group.len());
        req.extend_from_slice(&10i16.to_be_bytes()); // FindCoordinator
        req.extend_from_slice(&0i16.to_be_bytes()); // v0: group coordinator
        req.extend_from_slice(&correlation.to_be_bytes());
        put_str(&mut req, "wasmcloud-kafka-plugin");
        put_str(&mut req, group);

        self.stream.write_all(&(req.len() as i32).to_be_bytes())?;
        self.stream.write_all(&req)?;
        self.stream.flush()?;

        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = i32::from_be_bytes(len_buf);
        let mut resp = vec![0u8; len.max(0) as usize];
        self.stream.read_exact(&mut resp)?;

        let mut r = Reader::new(&resp);
        let _correlation = r.i32()?;
        let error = r.i16()?;
        if error != 0 {
            return Err(FetchError::Broker(error));
        }
        let _node_id = r.i32()?;
        let host = r.string()?;
        let port = r.i32()?;
        Ok(format!("{host}:{port}"))
    }

    /// Fetch from one partition, waiting up to `max_wait` for `min_bytes`.
    ///
    /// Blocking: this occupies the plugin instance for the whole wait, so the
    /// caller is responsible for keeping `max_wait` short.
    pub fn fetch(
        &mut self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_wait: Duration,
        max_bytes: i32,
    ) -> Result<Vec<FetchedRecord>, FetchError> {
        self.correlation = self.correlation.wrapping_add(1);
        let correlation = self.correlation;

        let mut req = Vec::with_capacity(64 + topic.len());
        req.extend_from_slice(&API_KEY_FETCH.to_be_bytes());
        req.extend_from_slice(&API_VERSION_FETCH.to_be_bytes());
        req.extend_from_slice(&correlation.to_be_bytes());
        put_str(&mut req, "wasmcloud-kafka-plugin");

        req.extend_from_slice(&(-1i32).to_be_bytes()); // replica_id: not a broker
        let wait_ms = i32::try_from(max_wait.as_millis()).unwrap_or(i32::MAX);
        req.extend_from_slice(&wait_ms.to_be_bytes());
        req.extend_from_slice(&1i32.to_be_bytes()); // min_bytes: return as soon as anything is there
        req.extend_from_slice(&max_bytes.to_be_bytes()); // (v3+) whole-response cap
        req.push(0); // (v4+) isolation_level: read uncommitted
        req.extend_from_slice(&1i32.to_be_bytes()); // one topic
        put_str(&mut req, topic);
        req.extend_from_slice(&1i32.to_be_bytes()); // one partition
        req.extend_from_slice(&partition.to_be_bytes());
        req.extend_from_slice(&offset.to_be_bytes());
        req.extend_from_slice(&max_bytes.to_be_bytes());

        self.stream.write_all(&(req.len() as i32).to_be_bytes())?;
        self.stream.write_all(&req)?;
        self.stream.flush()?;

        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = i32::from_be_bytes(len_buf);
        if len <= 0 {
            return Err(FetchError::Protocol(format!(
                "broker announced a {len}-byte response"
            )));
        }
        let mut resp = vec![0u8; len as usize];
        self.stream.read_exact(&mut resp)?;

        decode_response(&resp, correlation, partition, offset)
    }
}

/// Read the "one topic, one partition" preamble every single-partition
/// response shares, and fail if the broker answered a different shape.
///
/// Written as a check rather than as nested loops: these requests each name
/// exactly one topic and one partition, so a loop implies a generality that is
/// not there and hides the fact that anything else is a protocol error.
fn expect_single_partition(r: &mut Reader<'_>) -> Result<(), FetchError> {
    let topics = r.i32()?;
    if topics != 1 {
        return Err(FetchError::Protocol(format!(
            "asked about one topic, got {topics} in the response"
        )));
    }
    let _topic = r.string()?;
    let partitions = r.i32()?;
    if partitions != 1 {
        return Err(FetchError::Protocol(format!(
            "asked about one partition, got {partitions} in the response"
        )));
    }
    Ok(())
}

pub(crate) fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as i16).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// Decode a v4 Fetch response for the single topic/partition it was asked about.
fn decode_response(
    resp: &[u8],
    correlation: i32,
    want: i32,
    min_offset: i64,
) -> Result<Vec<FetchedRecord>, FetchError> {
    let mut r = Reader::new(resp);
    let got = r.i32()?;
    if got != correlation {
        // The connection is now desynchronised; the caller drops it.
        return Err(FetchError::Protocol(format!(
            "correlation id mismatch: expected {correlation}, got {got}"
        )));
    }
    let _throttle_time_ms = r.i32()?;

    expect_single_partition(&mut r)?;
    let partition = r.i32()?;
    let error = r.i16()?;
    // Read to advance the cursor to the record set; the plugin has no use for
    // the lag it implies until it grows a polling loop.
    let _high_watermark = r.i64()?;
    let _last_stable_offset = r.i64()?; // v4+
    let aborted = r.i32()?; // v4+, -1 when null
    for _ in 0..aborted.max(0) {
        let _producer_id = r.i64()?;
        let _first_offset = r.i64()?;
    }
    let set_len = r.i32()?;
    if error != 0 {
        return Err(FetchError::Broker(error));
    }
    if partition != want {
        return Err(FetchError::Protocol(format!(
            "broker answered for partition {partition}, not {want}"
        )));
    }
    if set_len > 0 {
        decode_batches(r.bytes(set_len as usize)?, partition, min_offset)
    } else {
        Ok(Vec::new())
    }
}

/// Smallest possible v2 batch header, used to tell "another batch follows" from
/// "the broker truncated the last one at `max_bytes`".
const BATCH_HEADER_LEN: usize = 61;

/// Java's snappy library frames its output, and Kafka inherited that framing
/// from the days its clients were all Java. A stream starting with this is a
/// sequence of length-prefixed blocks rather than one raw snappy block.
const XERIAL_MAGIC: &[u8] = &[0x82, b'S', b'N', b'A', b'P', b'P', b'Y', 0x00];

/// Inflate a batch's records section.
///
/// Codec numbers are the low three bits of the batch attributes, and every
/// decoder here is pure Rust: a C-backed one would not link for
/// `wasm32-wasip2`, the same constraint that rules out `rdkafka` entirely.
fn decompress(codec: i16, data: &[u8]) -> Result<Vec<u8>, FetchError> {
    let inflate = |what: &str, r: Result<Vec<u8>, String>| {
        r.map_err(|e| FetchError::Protocol(format!("{what} decompression failed: {e}")))
    };
    match codec {
        1 => inflate("gzip", {
            use std::io::Read as _;
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(data)
                .read_to_end(&mut out)
                .map(|_| out)
                .map_err(|e| e.to_string())
        }),
        2 => inflate("snappy", snappy(data)),
        3 => inflate("lz4", {
            use std::io::Read as _;
            let mut out = Vec::new();
            lz4_flex::frame::FrameDecoder::new(data)
                .read_to_end(&mut out)
                .map(|_| out)
                .map_err(|e| e.to_string())
        }),
        4 => inflate("zstd", {
            use std::io::Read as _;
            let mut out = Vec::new();
            ruzstd::StreamingDecoder::new(data)
                .map_err(|e| e.to_string())
                .and_then(|mut d| {
                    d.read_to_end(&mut out)
                        .map(|_| out)
                        .map_err(|e| e.to_string())
                })
        }),
        other => Err(FetchError::Protocol(format!(
            "record batch uses unknown compression codec {other}"
        ))),
    }
}

/// Snappy, in either shape Kafka produces: the Java-style framed stream, or a
/// single raw block from a client that does not bother with the framing.
fn snappy(data: &[u8]) -> Result<Vec<u8>, String> {
    if !data.starts_with(XERIAL_MAGIC) {
        return snap::raw::Decoder::new()
            .decompress_vec(data)
            .map_err(|e| e.to_string());
    }

    // magic, then version and compatible-version, then length-prefixed blocks.
    let mut out = Vec::new();
    let mut i = XERIAL_MAGIC.len() + 8;
    let mut decoder = snap::raw::Decoder::new();
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        let end = i.checked_add(len).ok_or("block length overflowed")?;
        if end > data.len() {
            return Err("framed snappy block runs past the end".to_owned());
        }
        out.extend_from_slice(
            &decoder
                .decompress_vec(&data[i..end])
                .map_err(|e| e.to_string())?,
        );
        i = end;
    }
    Ok(out)
}

/// Walk the v2 `RecordBatch`es in one partition's record set, returning the
/// records at or after `min_offset`.
///
/// Dropping the earlier ones is required, not tidiness. A fetch names an offset
/// but the broker answers in whole batches, so asking for offset 8 returns the
/// entire batch containing it — which may start at 0. A client that returns
/// those leading records hands back data the caller already had, and if it then
/// resumes from "last record + 1" it lands right back where it started: the same
/// batch, forever. It only shows up once records are written in batches; a
/// producer sending one at a time makes every batch start exactly where the
/// fetch asked.
///
/// A record set may also end mid-batch: the broker fills up to `max_bytes` and
/// cuts, expecting the client to notice and ask again from where it got to.
/// Treating that tail as a batch is what produces a spurious decode failure, so
/// anything that does not fit is left for the next fetch.
fn decode_batches(
    set: &[u8],
    partition: i32,
    min_offset: i64,
) -> Result<Vec<FetchedRecord>, FetchError> {
    let mut out = Vec::new();
    let mut b = Reader::new(set);

    while b.remaining() >= BATCH_HEADER_LEN {
        let base_offset = b.i64()?;
        let batch_length = b.i32()?;
        let body_start = b.pos();
        let Some(body_end) = body_start.checked_add(batch_length.max(0) as usize) else {
            break;
        };
        if body_end > set.len() {
            // Truncated tail: stop cleanly and let the caller re-fetch.
            break;
        }

        let _partition_leader_epoch = b.i32()?;
        let magic = b.i8()?;
        if magic != 2 {
            return Err(FetchError::Protocol(format!(
                "record batch magic byte {magic}, expected 2"
            )));
        }
        let _crc = b.i32()?; // CRC32C over the batch body; not verified here
        let attributes = b.i16()?;
        let _last_offset_delta = b.i32()?;
        let base_timestamp = b.i64()?;
        let _max_timestamp = b.i64()?;
        let _producer_id = b.i64()?;
        let _producer_epoch = b.i16()?;
        let _base_sequence = b.i32()?;
        let count = b.i32()?;

        let codec = attributes & 0x07;
        let is_control = attributes & 0x20 != 0;

        if is_control {
            // Transaction markers, not user records.
            b.seek(body_end);
            continue;
        }

        // Only the records section is compressed; the header just read is
        // always plain, which is how the codec is discoverable at all.
        let decompressed;
        let mut records = if codec == 0 {
            Reader::new(b.bytes(body_end - b.pos())?)
        } else {
            decompressed = decompress(codec, b.bytes(body_end - b.pos())?)?;
            Reader::new(&decompressed)
        };

        for _ in 0..count {
            let _record_len = records.varint()?;
            let _attributes = records.i8()?;
            let timestamp_delta = records.varint()?;
            let offset_delta = records.varint()?;

            let key_len = records.varint()?;
            let key = if key_len < 0 {
                None
            } else {
                Some(records.bytes(key_len as usize)?.to_vec())
            };
            let value_len = records.varint()?;
            let value = if value_len < 0 {
                Vec::new()
            } else {
                records.bytes(value_len as usize)?.to_vec()
            };
            // Headers are not surfaced by `cosmonic:kafka/types` yet, but they
            // still have to be walked to reach the next record.
            let header_count = records.varint()?;
            for _ in 0..header_count.max(0) {
                let k = records.varint()?;
                let _ = records.bytes(k.max(0) as usize)?;
                let v = records.varint()?;
                let _ = records.bytes(v.max(0) as usize)?;
            }

            let offset = base_offset + offset_delta;
            if offset < min_offset {
                // Already delivered; the broker just could not send half a batch.
                continue;
            }
            out.push(FetchedRecord {
                partition,
                offset,
                key,
                value,
                timestamp_ms: base_timestamp + timestamp_delta,
            });
        }

        // Trust the batch length over the record walk, so one odd batch cannot
        // drag the cursor out of alignment for the rest of the set.
        b.seek(body_end);
    }

    Ok(out)
}

/// A bounds-checked cursor. Every read returns an error rather than panicking,
/// because the bytes come from the network and a malformed frame must not take
/// the plugin's store down with it — a trap here restarts the plugin for every
/// workload it serves.
pub(crate) struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    fn pos(&self) -> usize {
        self.i
    }

    fn seek(&mut self, i: usize) {
        self.i = i.min(self.b.len());
    }

    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.i)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], FetchError> {
        let end = self
            .i
            .checked_add(n)
            .ok_or_else(|| FetchError::Protocol("length overflowed while decoding".to_owned()))?;
        if end > self.b.len() {
            return Err(FetchError::Protocol(format!(
                "response truncated: wanted {n} bytes at {}, {} remain",
                self.i,
                self.remaining()
            )));
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], FetchError> {
        self.take(n)
    }

    pub(crate) fn i8(&mut self) -> Result<i8, FetchError> {
        Ok(self.take(1)?[0] as i8)
    }

    pub(crate) fn i16(&mut self) -> Result<i16, FetchError> {
        let s = self.take(2)?;
        Ok(i16::from_be_bytes([s[0], s[1]]))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, FetchError> {
        let s = self.take(4)?;
        Ok(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, FetchError> {
        let s = self.take(8)?;
        Ok(i64::from_be_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    pub(crate) fn string(&mut self) -> Result<String, FetchError> {
        let n = self.i16()?;
        if n < 0 {
            return Ok(String::new());
        }
        Ok(String::from_utf8_lossy(self.take(n as usize)?).into_owned())
    }

    /// Zigzag varint, the encoding v2 record batches use for every length and
    /// delta — and the reason a v0-era parser cannot even walk one.
    fn varint(&mut self) -> Result<i64, FetchError> {
        let mut raw: u64 = 0;
        let mut shift = 0;
        loop {
            if shift > 63 {
                return Err(FetchError::Protocol(
                    "varint longer than 64 bits".to_owned(),
                ));
            }
            let byte = self.take(1)?[0];
            raw |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
    }
}

/// Send a framed request and read the framed response.
///
/// Every API here shares this: a four-byte length, the request, then the same
/// for the answer. Factored out so a new request type is only its own encoding.
pub(crate) fn round_trip(stream: &mut TcpStream, req: &[u8]) -> Result<Vec<u8>, FetchError> {
    stream.write_all(&(req.len() as i32).to_be_bytes())?;
    stream.write_all(req)?;
    stream.flush()?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = i32::from_be_bytes(len_buf);
    if len <= 0 {
        return Err(FetchError::Protocol(format!(
            "broker announced a {len}-byte response"
        )));
    }
    let mut resp = vec![0u8; len as usize];
    stream.read_exact(&mut resp)?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one uncompressed v2 batch holding `records` as (key, value).
    fn batch(base_offset: i64, base_ts: i64, records: &[(Option<&[u8]>, &[u8])]) -> Vec<u8> {
        fn varint(buf: &mut Vec<u8>, v: i64) {
            let mut raw = ((v << 1) ^ (v >> 63)) as u64;
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

        let mut body = Vec::new();
        body.extend_from_slice(&0i32.to_be_bytes()); // partition_leader_epoch
        body.push(2); // magic
        body.extend_from_slice(&0i32.to_be_bytes()); // crc (unverified)
        body.extend_from_slice(&0i16.to_be_bytes()); // attributes: no compression
        body.extend_from_slice(&((records.len() as i32) - 1).to_be_bytes());
        body.extend_from_slice(&base_ts.to_be_bytes());
        body.extend_from_slice(&base_ts.to_be_bytes());
        body.extend_from_slice(&(-1i64).to_be_bytes()); // producer_id
        body.extend_from_slice(&(-1i16).to_be_bytes()); // producer_epoch
        body.extend_from_slice(&(-1i32).to_be_bytes()); // base_sequence
        body.extend_from_slice(&(records.len() as i32).to_be_bytes());

        for (i, (key, value)) in records.iter().enumerate() {
            let mut rec = Vec::new();
            rec.push(0u8); // attributes
            varint(&mut rec, i as i64); // timestamp delta
            varint(&mut rec, i as i64); // offset delta
            match key {
                Some(k) => {
                    varint(&mut rec, k.len() as i64);
                    rec.extend_from_slice(k);
                }
                None => varint(&mut rec, -1),
            }
            varint(&mut rec, value.len() as i64);
            rec.extend_from_slice(value);
            varint(&mut rec, 0); // no headers

            varint(&mut body, rec.len() as i64);
            body.extend_from_slice(&rec);
        }

        let mut out = Vec::new();
        out.extend_from_slice(&base_offset.to_be_bytes());
        out.extend_from_slice(&(body.len() as i32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn decodes_keyed_and_unkeyed_records_with_timestamps() {
        let set = batch(
            100,
            1_700_000_000_000,
            &[(Some(b"k1"), b"v1"), (None, b"v2")],
        );
        let out = decode_batches(&set, 3, 0).expect("batch should decode");

        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].offset, 100,
            "first record takes the batch base offset"
        );
        assert_eq!(out[0].key.as_deref(), Some(&b"k1"[..]));
        assert_eq!(out[0].value, b"v1");
        assert_eq!(out[0].timestamp_ms, 1_700_000_000_000);
        assert_eq!(out[0].partition, 3);

        assert_eq!(out[1].offset, 101, "offset deltas advance from the base");
        assert_eq!(out[1].key, None, "a -1 key length is an absent key");
        assert_eq!(
            out[1].timestamp_ms, 1_700_000_000_001,
            "timestamps are a base plus a per-record delta"
        );
    }

    /// A fetch names an offset; the broker answers in whole batches. Asking for
    /// offset 3 of a batch based at 0 returns all five records, and returning
    /// the first three would redeliver data the caller already had — and, if it
    /// then resumed from "last + 1", would re-request this same batch forever.
    ///
    /// The infinite loop this guards against is invisible to a test that writes
    /// one record per batch, because then every batch starts exactly where the
    /// fetch asked. It took a topic written by `rpk` to expose it.
    #[test]
    fn records_before_the_requested_offset_are_dropped() {
        let set = batch(
            0,
            1_000,
            &[
                (None, b"zero"),
                (None, b"one"),
                (None, b"two"),
                (None, b"three"),
                (None, b"four"),
            ],
        );

        let out = decode_batches(&set, 0, 3).expect("batch should decode");
        let offsets: Vec<i64> = out.iter().map(|r| r.offset).collect();
        assert_eq!(
            offsets,
            vec![3, 4],
            "only records at or after the requested offset are returned"
        );
        assert_eq!(out[0].value, b"three", "and they keep their own payloads");
    }

    /// Two batches in one record set, which is what any non-trivial fetch
    /// returns — the cursor has to land exactly on the second batch's header.
    #[test]
    fn decodes_several_batches_in_one_record_set() {
        let mut set = batch(0, 1_000, &[(None, b"a")]);
        set.extend_from_slice(&batch(1, 2_000, &[(None, b"b"), (None, b"c")]));

        let out = decode_batches(&set, 0, 0).expect("both batches should decode");
        let offsets: Vec<i64> = out.iter().map(|r| r.offset).collect();
        assert_eq!(offsets, vec![0, 1, 2]);
    }

    /// The broker fills a record set up to `max_bytes` and cuts mid-batch. That
    /// tail is not corruption and must not be reported as such: the records
    /// before it are still good, and the caller re-fetches from where it got to.
    #[test]
    fn a_truncated_trailing_batch_is_ignored_not_an_error() {
        let mut set = batch(0, 1_000, &[(None, b"complete")]);
        let mut partial = batch(1, 2_000, &[(None, b"cut off here")]);
        partial.truncate(partial.len() - 6);
        set.extend_from_slice(&partial);

        let out = decode_batches(&set, 0, 0).expect("a truncated tail is not an error");
        assert_eq!(
            out.len(),
            1,
            "only the whole batch is returned; the cut one waits for the next fetch"
        );
        assert_eq!(out[0].value, b"complete");
    }

    /// An unknown codec must be named rather than walked as if it were plain
    /// bytes, which decodes to nonsense.
    #[test]
    fn an_unknown_codec_is_refused_by_name() {
        let mut set = batch(0, 1_000, &[(None, b"payload")]);
        // attributes sits after base_offset(8) + length(4) + epoch(4) + magic(1)
        // + crc(4); 7 is not a codec Kafka defines.
        set[21] = 0x00;
        set[22] = 0x07;

        let err = decode_batches(&set, 0, 0).expect_err("an unknown codec should be refused");
        match err {
            FetchError::Protocol(msg) => {
                assert!(
                    msg.contains("codec 7"),
                    "error should name the codec: {msg}"
                )
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    /// Snappy arrives in two shapes. Redpanda sends one raw block, but Kafka's
    /// Java client frames its output — magic, versions, then length-prefixed
    /// blocks — and a broker relays whatever the producer wrote. Reading a
    /// framed stream as a raw block fails outright, so both paths matter and
    /// only one of them is reachable from a Redpanda-backed test.
    #[test]
    fn snappy_decodes_both_raw_and_java_framed() {
        let payload = b"records section, pretend this is a record batch body".repeat(4);

        let raw = snap::raw::Encoder::new()
            .compress_vec(&payload)
            .expect("compress");
        assert_eq!(
            snappy(&raw).expect("raw snappy should decode"),
            payload,
            "a bare snappy block is the shape Redpanda sends"
        );

        // Two blocks, so the framed path has to loop rather than read one.
        let (a, b) = payload.split_at(payload.len() / 2);
        let mut framed = Vec::from(XERIAL_MAGIC);
        framed.extend_from_slice(&1i32.to_be_bytes()); // version
        framed.extend_from_slice(&1i32.to_be_bytes()); // compatible version
        for chunk in [a, b] {
            let block = snap::raw::Encoder::new()
                .compress_vec(chunk)
                .expect("compress");
            framed.extend_from_slice(&(block.len() as u32).to_be_bytes());
            framed.extend_from_slice(&block);
        }
        assert_eq!(
            snappy(&framed).expect("framed snappy should decode"),
            payload,
            "and the framed shape has to reassemble every block"
        );
    }

    /// A control batch carries transaction markers, not user records, and
    /// delivering those to a workload would be a bug.
    #[test]
    fn a_control_batch_yields_no_records() {
        let mut set = batch(0, 1_000, &[(None, b"marker")]);
        set[22] = 0x20; // control bit

        let out = decode_batches(&set, 0, 0).expect("a control batch should decode");
        assert!(out.is_empty(), "control records are not user records");
    }

    /// A batch claiming more records than it carries must surface as an error,
    /// never a panic or a silent short read: these bytes come off the network
    /// into a store shared by every workload the plugin serves, so a panic here
    /// restarts the plugin for all of them.
    #[test]
    fn a_batch_claiming_more_records_than_it_holds_errors() {
        let mut set = batch(0, 1_000, &[(None, b"only one")]);
        // `records_count` is the last i32 of the 61-byte header.
        set[57..61].copy_from_slice(&1000i32.to_be_bytes());

        let err = decode_batches(&set, 0, 0).expect_err("an overrun must be reported");
        match err {
            FetchError::Protocol(msg) => {
                assert!(
                    msg.contains("truncated"),
                    "error should say what ran out: {msg}"
                )
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }
}
