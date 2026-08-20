//! An HTTP workload that publishes to Kafka, exercising the producer path of
//! the `kafka-host-plugin` end to end under `wash dev`.
//!
//! The point of the example is what is *absent*: no Kafka client, no broker
//! address, no connection to keep warm across requests. This component imports
//! `cosmonic:kafka/producer` and the host routes each call across a store
//! boundary into the plugin, which holds the connections. The workload stays
//! ephemeral — `wash dev` tears it down and rebuilds it on every source change
//! — without any of those rebuilds costing a broker handshake.
//!
//! ```console
//! curl -X POST 'localhost:8000/publish?topic=demo' --data 'hello'
//! curl -X POST 'localhost:8000/publish?topic=demo&key=user-1' --data 'hello'
//! ```

mod bindings {
    #![allow(unsafe_code)]
    wit_bindgen::generate!({ world: "publisher", generate_all });
}

use bindings::cosmonic::kafka::types::{
    ConfigEntry, ConsumedRecord, ProduceRecord,
};
use bindings::cosmonic::kafka::producer::Producer;
use bindings::exports::cosmonic::kafka::handler::{Guest as HandlerGuest, HandlerError};
use bindings::exports::wasi::http::handler::Guest;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};
use bindings::wasmcloud::blobstore::blobstore;

/// Read at most this much of a request body. A publish is a single Kafka
/// record, so a body that keeps growing is a mistake rather than a large
/// message, and refusing it beats buffering it into the workload's heap.
const MAX_BODY: usize = 1 << 20;

/// Ask for the batch to be delivered again.
///
/// Everything this handler can fail at — an unreachable object store, a broker
/// that dropped a connection — is worth another attempt.
fn refuse(message: &str) -> HandlerError {
    HandlerError::Transient(Some(message.to_owned()))
}

/// Reject the batch for good, so the provider dead-letters it now instead of
/// redelivering something that will never succeed.
fn reject(message: &str) -> HandlerError {
    HandlerError::Permanent(Some(message.to_owned()))
}

/// Open a producer.
///
/// The config list is empty on purpose: `bootstrap.servers` is an operator
/// concern, and the plugin layers its own over whatever a workload passes. This
/// workload therefore names no broker and holds no credential — which is the
/// property that makes going through the capability worth it rather than
/// opening a socket here.
async fn open_producer() -> Result<Producer, String> {
    let config: Vec<ConfigEntry> = Vec::new();
    Producer::open(config)
        .await
        .map_err(|e| format!("{:?}: {}", e.code, e.message))
}

const USAGE: &str = "POST /publish?topic=<topic>[&key=<key>]   publish the request body\nGET  /consume?max=<n>[&commit]         pull what has accumulated\nPOST /bigwrite?mode=<m>&mb=<n>         stream n MiB up, m = multipart|chunked\nGET  /bigread?mode=<m>&mb=<n>          stream it back, counting bytes\n";

struct Component;

impl Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request
            .get_path_with_query()
            .unwrap_or_else(|| "/".to_string());
        let (route, query) = match path.split_once('?') {
            Some((r, q)) => (r, q),
            None => (path.as_str(), ""),
        };

        // Pull whatever has accumulated. The plugin holds the offsets, so this
        // workload can be torn down and rebuilt between polls without losing
        // its place — which is the point of the capability living there.
        if route == "/consume" {
            // Pulling means holding a `consumer` resource and reading
            // `records()`, which this provider does not implement — and which
            // this workload could not use anyway: it is per-request, and a
            // stream needs an owner that outlives the request. Records arrive
            // through the `handler` export below instead.
            return Ok(respond(
                501,
                "this workload is push-shaped: the plugin calls its \
                 cosmonic:kafka/handler export with each batch. See /publish.\n",
            ));
        }

        // Two events, one per upload strategy, so they can be compared on the
        // same payload: `?mode=multipart` and `?mode=chunked` write to
        // different containers, which is what the plugin keys its strategy off.
        //
        // The bytes are generated into the stream rather than built first, so
        // the workload's own memory stays bounded too — otherwise this would
        // just move the 4 GB problem one component to the left.
        if route == "/bigwrite" {
            let mode = query_get(query, "mode").unwrap_or_else(|| "multipart".to_string());
            let mb: u64 = query_get(query, "mb")
                .and_then(|m| m.parse().ok())
                .unwrap_or(16);
            let container_name = format!("bench-{mode}");

            let container = match blobstore::get_container(container_name.clone()).await {
                Ok(c) => c,
                Err(e) => {
                    return Ok(respond(
                        502,
                        &format!("no container {container_name}: {e:?}\n"),
                    ))
                }
            };

            let (mut tx, rx) = bindings::wit_stream::new();
            wit_bindgen::spawn_local(async move {
                // 1 MiB at a time, so peak here is one block regardless of `mb`.
                let block: Vec<u8> = (0..1024 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
                for _ in 0..mb {
                    // `write_all` hands back whatever it could not send, which
                    // is how a dropped reader is reported. Ignoring that and
                    // writing again traps the whole component — so a failure on
                    // the plugin side ends the generator instead.
                    if !tx.write_all(block.clone()).await.is_empty() {
                        break;
                    }
                }
                drop(tx);
            });

            let key = format!("{mb}mb.bin");
            return Ok(match container.write_data(key.clone(), rx).await {
                Ok(()) => respond(
                    200,
                    &format!("wrote {mb} MiB to {container_name}/{key} via {mode}\n"),
                ),
                Err(e) => respond(502, &format!("write failed ({mode}): {e:?}\n")),
            });
        }

        // Read it back without ever holding it: count the bytes as they stream.
        if route == "/bigread" {
            let mode = query_get(query, "mode").unwrap_or_else(|| "multipart".to_string());
            let mb: u64 = query_get(query, "mb")
                .and_then(|m| m.parse().ok())
                .unwrap_or(16);
            let container = match blobstore::get_container(format!("bench-{mode}")).await {
                Ok(c) => c,
                Err(e) => return Ok(respond(502, &format!("{e:?}\n"))),
            };
            let mut stream = match container.get_data(format!("{mb}mb.bin"), 0, u64::MAX).await {
                Ok(s) => s,
                Err(e) => return Ok(respond(502, &format!("read failed: {e:?}\n"))),
            };
            let mut total: u64 = 0;
            loop {
                let (status, chunk) = stream.read(Vec::with_capacity(256 * 1024)).await;
                total += chunk.len() as u64;
                if matches!(status, wit_bindgen::StreamResult::Dropped) {
                    break;
                }
            }
            return Ok(respond(
                200,
                &format!("read {total} bytes from bench-{mode}\n"),
            ));
        }

        // Counts every key in a container. Worth a route of its own because a
        // container larger than one `ListObjectsV2` page is exactly where an
        // unpaginated listing looks like it worked and silently under-reports.
        if route == "/count" {
            let name = query_get(query, "container").unwrap_or_default();
            let container = match blobstore::get_container(name.clone()).await {
                Ok(c) => c,
                Err(e) => return Ok(respond(502, &format!("{e:?}\n"))),
            };
            let mut names = match container.list_objects().await {
                Ok(s) => s,
                Err(e) => return Ok(respond(502, &format!("list failed: {e:?}\n"))),
            };
            let mut count = 0usize;
            loop {
                let (status, batch) = names.read(Vec::with_capacity(256)).await;
                count += batch.len();
                if matches!(status, wit_bindgen::StreamResult::Dropped) {
                    break;
                }
            }
            return Ok(respond(200, &format!("{count} objects in {name}\n")));
        }

        if route != "/publish" {
            return Ok(respond(404, USAGE));
        }

        let Some(topic) = query_get(query, "topic") else {
            return Ok(respond(400, "missing required query parameter 'topic'\n"));
        };
        // An explicitly empty `key=` is a caller asking for an empty key, which
        // Kafka cannot represent distinctly from no key at all, so it is
        // treated as absent — the same collapse the plugin documents on read.
        let key = query_get(query, "key").filter(|k| !k.is_empty());

        let body = match read_body(request).await {
            Ok(body) => body,
            Err(e) => return Ok(respond(400, &format!("{e}\n"))),
        };
        let len = body.len();

        // The whole example: one call, no client to build, no address to know.
        // `send` is awaited because a plugin's capabilities are installed on
        // this component's linker as concurrent host functions — which is also
        // why this handler is `wasi:http/handler@0.3.0` and not p2's
        // `incoming-handler`: a sync-lifted export has nowhere to await from.
        let producer = match open_producer().await {
            Ok(p) => p,
            Err(e) => return Ok(respond(502, &format!("open failed: {e}\n"))),
        };
        let record = ProduceRecord {
            partition: None,
            key: key.map(String::into_bytes),
            value: Some(body),
            headers: Vec::new(),
            timestamp: None,
        };
        match producer.send(topic.clone(), record).await {
            Ok(ack) => Ok(respond(
                200,
                &format!(
                    "published {len} bytes to {topic} partition {} offset {}\n",
                    ack.partition, ack.offset
                ),
            )),
            // The plugin's error variants survive the store boundary intact, so
            // a misconfigured broker list reads differently here than a topic
            // the cluster refuses to create.
            Err(e) => Ok(respond(502, &format!("publish failed: {e:?}\n"))),
        }
    }
}

/// Where a handled record is written back, so a batch the plugin pushed can be
/// observed after the fact. A workload instance is ephemeral — it does not
/// survive to answer an HTTP request about what it saw — so the evidence has to
/// go somewhere durable, and Kafka is right there.
const PROCESSED_SUFFIX: &str = ".processed";

/// Container the handler archives batches into. Hardcoded because this is an
/// example; a real workload would take it from `wasi:config`.
const ARCHIVE_CONTAINER: &str = "kafka-archive";

impl HandlerGuest for Component {
    /// Called by the plugin's trigger loop, once per batch.
    ///
    /// Returning `ok` is what lets the plugin commit, so this must not report
    /// success for work it did not finish — hence the `?` on every publish
    /// rather than a best-effort loop. A failure here means the whole batch is
    /// redelivered, which is why the transform must be idempotent: writing the
    /// same record to the same output topic twice is the expected worst case.
    async fn handle(records: Vec<ConsumedRecord>) -> Result<Option<i64>, HandlerError> {
        let Some(first) = records.first() else {
            return Ok(None);
        };

        // A record whose value starts with `poison` always fails, so the
        // plugin's bounded-retry and dead-letter path can be exercised without
        // waiting for a real bug to produce one.
        if records
            .iter()
            .any(|r| r.value.as_deref().unwrap_or_default().starts_with(b"poison")) {
            return Err(reject("poison record: this handler will never succeed"));
        }

        // One object per batch, keyed by where the batch came from, so a rerun
        // of the same batch overwrites rather than accumulating. That is what
        // makes this idempotent under the at-least-once redelivery the plugin
        // promises: the offsets are in the key, so replaying a batch after a
        // crash writes the same object again instead of a second copy.
        let archive_key = format!(
            "{}/partition-{}/{:012}.txt",
            first.topic, first.partition, first.offset
        );
        let archive: String = records
            .iter()
            .map(|r| {
                format!(
                    "{} {} {}\n",
                    r.offset,
                    r.key
                        .as_deref()
                        .map(|k| String::from_utf8_lossy(k).into_owned())
                        .unwrap_or_else(|| "-".to_string()),
                    String::from_utf8_lossy(r.value.as_deref().unwrap_or_default())
                )
            })
            .collect();

        // Standard `wasmcloud:blobstore`, so these lines are the same whether
        // the host serves it from S3, the filesystem, or NATS — and carry no
        // endpoint or credential either way.
        let container = blobstore::get_container(ARCHIVE_CONTAINER.to_string())
            .await
            .map_err(|e| refuse(&format!("archive container unavailable: {e:?}")))?;

        // The body crosses as a `stream<u8>`, which is what makes this
        // interface usable across a plugin boundary at all.
        let (mut tx, rx) = bindings::wit_stream::new();
        wit_bindgen::spawn_local(async move {
            tx.write_all(archive.into_bytes()).await;
            drop(tx);
        });
        container
            .write_data(archive_key, rx)
            .await
            .map_err(|e| refuse(&format!("archive failed: {e:?}")))?;

        for record in records {
            let topic = format!("{}{PROCESSED_SUFFIX}", record.topic);
            let value = format!(
                "handled offset {}: {}",
                record.offset,
                String::from_utf8_lossy(record.value.as_deref().unwrap_or_default())
            );
            let producer = open_producer().await.map_err(|e| refuse(&e))?;
            producer
                .send(
                    topic,
                    ProduceRecord {
                        partition: None,
                        key: record.key,
                        value: Some(value.into_bytes()),
                        headers: Vec::new(),
                        timestamp: None,
                    },
                )
                .await
                .map_err(|e| refuse(&format!("{e:?}")))?;
        }
        // This handler either archives the whole batch or fails it, so there
        // is no partial progress to report.
        Ok(None)
    }
}

fn query_get(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

/// Enough percent-decoding for a topic or key typed into `curl`. Not a general
/// URL decoder: an invalid escape is left as written rather than rejected,
/// because the broker's own topic-name rules are the check that matters.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn read_body(request: Request) -> Result<Vec<u8>, String> {
    // `res` is how a guest reports a problem back upstream mid-body. Nothing
    // here fails that way, so dropping the writer resolves it to the `Ok(())`
    // default.
    let (res_tx, res_rx) = bindings::wit_future::new(|| Ok(()));
    let (mut body, _trailers) = Request::consume_body(request, res_rx);

    let mut buf = Vec::new();
    loop {
        let (status, chunk) = body.read(Vec::with_capacity(64 * 1024)).await;
        if buf.len() + chunk.len() > MAX_BODY {
            return Err(format!("request body exceeds {MAX_BODY} bytes"));
        }
        buf.extend_from_slice(&chunk);
        if matches!(status, wit_bindgen::StreamResult::Dropped) {
            break;
        }
    }
    drop(res_tx);
    Ok(buf)
}

fn respond(status: u16, body: &str) -> Response {
    let headers = Fields::new();
    let _ = headers.set("content-type", &[b"text/plain".to_vec()]);

    let bytes = body.as_bytes().to_vec();
    let (mut tx, rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| Ok(None));
    wit_bindgen::spawn_local(async move {
        tx.write_all(bytes).await;
        drop(tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });

    let (response, _result) = Response::new(headers, Some(rx), trailers_rx);
    let _ = response.set_status_code(status);
    response
}

mod export {
    #![allow(unsafe_code)]
    use super::{bindings, Component};
    bindings::export!(Component with_types_in bindings);
}
