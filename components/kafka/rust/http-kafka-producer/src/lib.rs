//! HTTP → Kafka producer.
//!
//! Routes:
//! - `POST /produce?topic=T&key=K&value=V` — one record via `send`; body `partition:offset`.
//! - `POST /produce-batch?topic=T&count=N&size=S` — N records via `send-batch`;
//!   body is one line per record (`ok` or the error code).
//!
//! The host owns and reuses the producer configured by the workload's Kafka
//! binding. The component can use only the binding's brokers and topic grant.

mod bindings {
    use super::Component;
    wit_bindgen::generate!({ world: "http-kafka-producer", generate_all });
    export!(Component);
}

use std::collections::BTreeMap;

use bindings::cosmonic::kafka::producer;
use bindings::cosmonic::kafka::types::ProduceRecord;
use bindings::exports::wasi::http::handler;
use bindings::wasi::http::types::{Headers, Method};
use bindings::{wit_future, wit_stream};

struct Component;

const MAX_BATCH_RECORDS: usize = 10_000;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;

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
    let record = ProduceRecord {
        partition: None,
        key: Some(key.as_bytes().to_vec()),
        value: Some(value.as_bytes().to_vec()),
        headers: Vec::new(),
        timestamp: None,
    };
    match producer::send(topic.to_string(), record).await {
        Ok(ack) => resp(200, &format!("{}:{}", ack.partition, ack.offset)),
        Err(e) => resp(500, &format!("send failed: {:?}", e.code)),
    }
}

async fn produce_batch(query: &str) -> handler::Response {
    let p = parse(query);
    let Some(topic) = p.get("topic") else {
        return resp(400, "topic required");
    };
    let count = match p.get("count") {
        None => 100,
        Some(value) => match value.parse::<usize>() {
            Ok(count @ 1..=MAX_BATCH_RECORDS) => count,
            _ => return resp(400, "count must be between 1 and 10000"),
        },
    };
    let size = match p.get("size") {
        None => 64,
        Some(value) => match value.parse::<usize>() {
            Ok(size) if size <= MAX_RECORD_BYTES => size,
            _ => return resp(400, "size must be between 0 and 1048576"),
        },
    };
    if count.saturating_mul(size) > MAX_BATCH_BYTES {
        return resp(400, "batch values must total at most 16777216 bytes");
    }
    let records: Vec<ProduceRecord> = (0..count)
        .map(|i| ProduceRecord {
            partition: None,
            key: Some(format!("k{i}").into_bytes()),
            value: Some(vec![b'x'; size]),
            headers: Vec::new(),
            timestamp: None,
        })
        .collect();
    match producer::send_batch(topic.to_string(), records).await {
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
