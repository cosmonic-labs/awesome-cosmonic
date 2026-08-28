//! Producing to Kafka over the HTTP Proxy, via `wasi:http/client@0.3.0`.
//!
//! The Kafka wire protocol needs a long-lived TCP connection, cluster
//! metadata, and a partitioner. A request-scoped component has none of those,
//! and giving it one means a messaging capability provider. The HTTP Proxy
//! (pandaproxy in Redpanda, the REST Proxy in Confluent) removes the problem:
//! producing becomes one POST, and the only host capability needed is outbound
//! HTTP, already scoped by `workload.allowedHosts`.
//!
//! The tradeoff is real and worth naming: per-request HTTP costs more than a
//! batched native producer, and the proxy is another hop to run. For a
//! browser-facing collector, where events arrive as HTTP anyway, it is the
//! natural shape.
//!
//! `client::send` is an `async func` in WIT, so this is a plain `.await` with
//! no poll loop and no `wasi:io` import.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::bindings;
use crate::bindings::wasi::http::client;
use crate::bindings::wasi::http::types::{Fields, Method, Request, Scheme};
use crate::config::config;
use crate::event::Event;

/// Kafka REST Proxy v2, JSON embedded format. Redpanda and Confluent both
/// speak this.
const CONTENT_TYPE: &str = "application/vnd.kafka.json.v2+json";

#[derive(Serialize)]
struct ProxyRecord<'a> {
    /// Partition key. Keying by session keeps one session's events on one
    /// partition and therefore in order, which matters the moment anyone
    /// wants funnel or sessionization queries downstream.
    key: &'a str,
    value: &'a Event,
}

#[derive(Serialize)]
struct ProxyPayload<'a> {
    records: Vec<ProxyRecord<'a>>,
}

/// Where a batch landed. Surfaced in the response so a caller can see the
/// partition spread without opening a Kafka client.
#[derive(Debug, Serialize)]
pub(crate) struct ProduceResult {
    pub(crate) produced: usize,
    pub(crate) topic: String,
    pub(crate) offsets: Value,
}

pub(crate) async fn produce(events: &[Event]) -> Result<ProduceResult> {
    let cfg = config();
    if events.is_empty() {
        return Ok(ProduceResult {
            produced: 0,
            topic: cfg.topic.clone(),
            offsets: Value::Array(vec![]),
        });
    }

    let payload = ProxyPayload {
        records: events
            .iter()
            .map(|e| ProxyRecord {
                key: &e.session_id,
                value: e,
            })
            .collect(),
    };
    let body = serde_json::to_vec(&payload).context("failed to serialize produce payload")?;

    let (status, response_body) = post_json(
        &cfg.proxy_authority,
        &format!("/topics/{}", cfg.topic),
        CONTENT_TYPE,
        body,
    )
    .await?;

    if !(200..300).contains(&status) {
        // The proxy reports per-record failures in the body, so include it
        // verbatim rather than just the status.
        bail!(
            "kafka proxy returned {status}: {}",
            String::from_utf8_lossy(&response_body)
        );
    }

    // `{"offsets":[{"partition":1,"offset":37}, ...]}`. A 2xx with an `error`
    // set on an entry still means that record did not land.
    let parsed: Value = serde_json::from_slice(&response_body).unwrap_or(Value::Null);
    let offsets = parsed
        .get("offsets")
        .cloned()
        .unwrap_or(Value::Array(vec![]));

    if let Some(err) = offsets
        .as_array()
        .and_then(|entries| entries.iter().find(|e| !e["error"].is_null()))
    {
        bail!("kafka proxy rejected a record: {err}");
    }

    Ok(ProduceResult {
        produced: events.len(),
        topic: cfg.topic.clone(),
        offsets,
    })
}

/// One outbound POST. Returns the status and the collected response body.
async fn post_json(
    authority: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>)> {
    let headers = Fields::new();
    let _ = headers.append("content-type", content_type.as_bytes());

    // The request body is a stream fed by its own task, so the request head
    // can go out before the payload is fully serialized onto the wire.
    let (mut body_tx, body_rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| Ok(None));
    wit_bindgen::spawn_local(async move {
        body_tx.write_all(body).await;
        drop(body_tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });

    let (request, _sent) = Request::new(headers, Some(body_rx), trailers_rx, None);
    let _ = request.set_method(&Method::Post);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some(authority));
    let _ = request.set_path_with_query(Some(path));

    let response = client::send(request)
        .await
        .map_err(|e| anyhow::anyhow!("request to {authority}{path} failed: {e:?}"))?;

    let status = response.get_status_code();
    let (result_tx, result_rx) = bindings::wit_future::new(|| Ok(()));
    let (contents, _trailers) =
        crate::bindings::wasi::http::types::Response::consume_body(response, result_rx);
    let bytes = contents.collect().await;
    drop(result_tx);

    Ok((status, bytes))
}
