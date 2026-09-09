//! HTTP → Kafka producer.
//!
//! Routes:
//! - `POST /produce?topic=T&key=K&value=V` — one record via `send`; body `partition:offset`.
//! - `POST /produce-batch?topic=T&count=N&size=S` — N records via `send-batch`;
//!   body is one line per record (`ok` or the error code).
//!
//! Performance notes (measured on wasmCloud 2.8 / cosmonic:kafka 0.3.0):
//! - `Producer::open` builds a full Kafka client (DNS + TCP + metadata,
//!   ~100 ms). One open per request caps a single instance near ~10 req/s;
//!   `send-batch` amortizes it across the whole batch (30k+ records/s).
//! - Keep `maxConcurrency` at its default (1) for this shape: combining
//!   `poolSize > 1` with `maxConcurrency > 1` on a component that opens a
//!   client per call measured 14x SLOWER than either knob alone. Scale with
//!   `poolSize`, replicas, or batching instead.
//! - The operator pins `bootstrap.servers` (and anything else set in the
//!   workload's `hostInterfaces[].config`) — a guest cannot override it.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "http-kafka-producer", generate_all });
    export!(Component);
}

use std::collections::BTreeMap;

use bindings::cosmonic::kafka::producer::Producer;
use bindings::cosmonic::kafka::types::ProduceRecord;
use bindings::exports::wasi::http::handler;
use bindings::wasi::http::types::{Headers, Method};
use bindings::{wit_future, wit_stream};

struct Component;

impl handler::Guest for Component {
    async fn handle(req: handler::Request) -> Result<handler::Response, handler::ErrorCode> {
        let Some(pq) = req.get_path_with_query() else {
            return Ok(resp(400, "no path"));
        };
        let (path, query) = pq.split_once('?').unwrap_or((pq.as_str(), ""));
        Ok(match (req.get_method(), path) {
            (Method::Post, "/produce") => produce(query).await,
            (Method::Post, "/produce-batch") => produce_batch(query).await,
            _ => resp(404, "no such route"),
        })
    }
}

async fn produce(query: &str) -> handler::Response {
    let p = parse(query);
    let (Some(topic), Some(key), Some(value)) = (p.get("topic"), p.get("key"), p.get("value"))
    else {
        return resp(400, "topic, key and value required");
    };
    // The host merges the workload's kafka config over this empty list, so
    // `bootstrap.servers` etc. come from the manifest, not the code.
    let producer = match Producer::open(Vec::new()).await {
        Ok(p) => p,
        Err(e) => return resp(500, &format!("open failed: {:?}", e.code)),
    };
    let record = ProduceRecord {
        partition: None,
        key: Some(key.as_bytes().to_vec()),
        value: Some(value.as_bytes().to_vec()),
        headers: Vec::new(),
        timestamp: None,
    };
    match producer.send(topic.to_string(), record).await {
        Ok(ack) => resp(200, &format!("{}:{}", ack.partition, ack.offset)),
        Err(e) => resp(500, &format!("send failed: {:?}", e.code)),
    }
}

async fn produce_batch(query: &str) -> handler::Response {
    let p = parse(query);
    let Some(topic) = p.get("topic") else {
        return resp(400, "topic required");
    };
    let count: usize = p.get("count").and_then(|c| c.parse().ok()).unwrap_or(100);
    let size: usize = p.get("size").and_then(|s| s.parse().ok()).unwrap_or(64);
    let producer = match Producer::open(Vec::new()).await {
        Ok(p) => p,
        Err(e) => return resp(500, &format!("open failed: {:?}", e.code)),
    };
    let records: Vec<ProduceRecord> = (0..count)
        .map(|i| ProduceRecord {
            partition: None,
            key: Some(format!("k{i}").into_bytes()),
            value: Some(vec![b'x'; size]),
            headers: Vec::new(),
            timestamp: None,
        })
        .collect();
    match producer.send_batch(topic.to_string(), records).await {
        Ok(outcomes) => {
            let lines: Vec<String> = outcomes
                .into_iter()
                .map(|o| match o {
                    Ok(_) => "ok".into(),
                    Err(e) => format!("{:?}", e.code),
                })
                .collect();
            resp(200, &lines.join("\n"))
        }
        Err(e) => resp(500, &format!("send-batch failed: {:?}", e.code)),
    }
}

fn parse(query: &str) -> BTreeMap<String, String> {
    form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn resp(status: u16, body: &str) -> handler::Response {
    let (trailers_tx, trailers_rx) = wit_future::new(|| unreachable!());
    let (mut body_tx, body_rx) = wit_stream::new();
    let bytes = body.as_bytes().to_vec();
    wit_bindgen::spawn_local(async move {
        body_tx.write_all(bytes).await;
        let _ = trailers_tx.write(Ok(None)).await;
    });
    let (response, _fut) = handler::Response::new(Headers::new(), Some(body_rx), trailers_rx);
    let _ = response.set_status_code(status);
    response
}
