//! Event gateway: HTTP in, Kafka out. A wasip3 component.
//!
//! The producing half of the reference architecture. It accepts clickstream
//! events over HTTP, fills in what the caller omitted, and produces them to a
//! Kafka topic through the HTTP Proxy. From there ClickHouse takes over and
//! there is no more application code in the path - see
//! `infra/clickhouse/init/01-ingest-pipeline.sql`.
//!
//! Being wasip3 means the entrypoint is `wasi:http/handler@0.3.0`: one
//! `async fn handle` that returns a response, with bodies as native
//! `stream<u8>`. No `wasi:io`, no `response-outparam`, and the outbound
//! produce call is a plain `.await`.
//!
//! Routes:
//!   POST /events                  produce one or many events
//!   POST /events/simulate?count=N synthesize and produce N events
//!   GET  /healthz                 liveness, plus the resolved config
//!   GET  /                        a short human-readable index

use anyhow::Result;
use serde_json::json;

mod bindings {
    wit_bindgen::generate!({
        world: "event-gateway",
        path: "wit",
        generate_all,
    });
}

mod config;
mod event;
mod http;
mod producer;
mod rng;

use bindings::exports::wasi::http::handler::Guest;
use bindings::wasi::http::types::{ErrorCode, Method, Request, Response};

use config::config;
use event::{EventPayload, synthesize, validate};
use http::{CORS, json, read_body, respond};
use rng::Rng;

const DEFAULT_SIMULATE_COUNT: usize = 500;

struct Component;

impl Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let method = request.get_method();
        let target = request.get_path_with_query().unwrap_or_default();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target, String::new()),
        };

        Ok(match (&method, path.as_str()) {
            // Browsers preflight a cross-origin POST with a JSON content type,
            // and the dashboard on another port drives this endpoint.
            (Method::Options, _) => respond(204, &CORS, Vec::new()),

            (Method::Post, "/events") => finish(handle_events(request).await),
            (Method::Post, "/events/simulate") => finish(handle_simulate(&query).await),
            (Method::Get, "/healthz") => json(200, &health()),
            (Method::Get, "/") => index(),

            _ => json(
                404,
                &json!({"error": "not found", "routes": [
                    "POST /events", "POST /events/simulate?count=N", "GET /healthz"
                ]}),
            ),
        })
    }
}

/// Accepts a single event, an array, or `{"events": [...]}`.
async fn handle_events(request: Request) -> Result<(u16, serde_json::Value)> {
    let body = read_body(request).await;

    let payload: EventPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return Ok((400, json!({"error": format!("invalid JSON body: {e}")}))),
    };

    let inputs = payload.into_vec();
    if inputs.is_empty() {
        return Ok((400, json!({"error": "no events in request"})));
    }
    let cfg = config();
    if inputs.len() > cfg.max_batch {
        return Ok((
            413,
            json!({"error": format!("batch of {} exceeds MAX_BATCH of {}", inputs.len(), cfg.max_batch)}),
        ));
    }

    for (i, input) in inputs.iter().enumerate() {
        if let Err(message) = validate(input) {
            return Ok((400, json!({"error": message, "index": i})));
        }
    }

    let mut rng = Rng::from_entropy();
    let now_ms = now_millis();
    let events: Vec<_> = inputs
        .into_iter()
        .map(|i| i.into_event(&mut rng, now_ms))
        .collect();

    let result = producer::produce(&events).await?;
    Ok((202, serde_json::to_value(result)?))
}

/// Synthesizes a batch of plausible traffic. This is what makes the example
/// self-driving: one call and the whole pipeline has something to chew on.
async fn handle_simulate(query: &str) -> Result<(u16, serde_json::Value)> {
    let cfg = config();
    let count = query_param(query, "count")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SIMULATE_COUNT)
        .clamp(1, cfg.max_batch);

    let mut rng = Rng::from_entropy();
    let now_ms = now_millis();
    let events: Vec<_> = (0..count)
        .map(|i| synthesize(&mut rng, now_ms, i))
        .collect();

    let result = producer::produce(&events).await?;
    Ok((202, serde_json::to_value(result)?))
}

fn health() -> serde_json::Value {
    let cfg = config();
    json!({
        "status": "ok",
        "produce_url": cfg.produce_url(),
        "topic": cfg.topic,
        "max_batch": cfg.max_batch,
        "wasi_http": "0.3.0",
    })
}

fn index() -> Response {
    let cfg = config();
    let body = format!(
        "event-gateway (wasip3)\n\n\
         Produces clickstream events to Kafka topic '{}' via {}.\n\n\
         POST /events                   one event, an array, or {{\"events\": [...]}}\n\
         POST /events/simulate?count=N  synthesize N events (max {})\n\
         GET  /healthz                  resolved configuration\n\n\
         Rows land in ClickHouse analytics.events within kafka_flush_interval_ms.\n\
         The dashboard is served by insights-api.\n",
        cfg.topic,
        cfg.produce_url(),
        cfg.max_batch,
    );
    let mut headers = vec![("content-type", "text/plain; charset=utf-8")];
    headers.extend_from_slice(&CORS);
    respond(200, &headers, body.into_bytes())
}

/// Renders a handler result as JSON, turning an unexpected error into a 502.
/// Producing failures are upstream failures, so 502 is the honest code.
fn finish(outcome: Result<(u16, serde_json::Value)>) -> Response {
    match outcome {
        Ok((status, value)) => json(status, &value),
        Err(e) => {
            eprintln!("request failed: {e:?}");
            json(502, &json!({"error": format!("{e:#}")}))
        }
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// Wall clock via the standard library, which reaches the host through
/// wasi-libc. Keeps `wasi:clocks` out of the world.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

bindings::export!(Component with_types_in bindings);
