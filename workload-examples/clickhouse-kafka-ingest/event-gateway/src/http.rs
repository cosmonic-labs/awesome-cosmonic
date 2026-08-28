//! `wasi:http@0.3.0` plumbing.
//!
//! The shape difference from p2 is worth understanding, because it is the
//! reason to move: a p3 body is a native component-model `stream<u8>`, not a
//! `wasi:io` pollable wrapped in a resource. Handlers are `async fn` that
//! return a `response` directly instead of writing into a `response-outparam`,
//! and there is no `wasi:io` import anywhere in this component.
//!
//! A body is therefore produced by a task: `Response::new` takes the read end
//! of a stream, and a `spawn_local` task owns the write end. The response
//! header goes out as soon as the handler returns, while the body is still
//! being written.

use crate::bindings;
use crate::bindings::wasi::http::types::{Fields, Request, Response};

/// Read an entire request body into memory.
///
/// Fine here because these payloads are small JSON documents bounded by
/// `MAX_BATCH`. A large or unbounded upload should be consumed incrementally
/// from the stream rather than collected.
pub(crate) async fn read_body(request: Request) -> Vec<u8> {
    // The future lets the guest report a read failure upstream. This component
    // always accepts the body, so dropping the writer resolves it to the
    // `Ok(())` default.
    let (result_tx, result_rx) = bindings::wit_future::new(|| Ok(()));
    let (contents, _trailers) = Request::consume_body(request, result_rx);
    let bytes = contents.collect().await;
    drop(result_tx);
    bytes
}

/// Build a response whose body is written by a spawned task.
pub(crate) fn respond(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> Response {
    let fields = Fields::new();
    for &(name, value) in headers {
        let _ = fields.append(name, value.as_bytes());
    }

    let (mut body_tx, body_rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| Ok(None));
    wit_bindgen::spawn_local(async move {
        if !body.is_empty() {
            body_tx.write_all(body).await;
        }
        drop(body_tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });

    let (response, _result) = Response::new(fields, Some(body_rx), trailers_rx);
    let _ = response.set_status_code(status);
    response
}

/// Permissive CORS, so the dashboard served by insights-api on another port
/// can drive the traffic generator. A real collector would pin the origin.
pub(crate) const CORS: [(&str, &str); 3] = [
    ("access-control-allow-origin", "*"),
    ("access-control-allow-methods", "GET, POST, OPTIONS"),
    ("access-control-allow-headers", "content-type"),
];

/// JSON response with CORS headers attached.
pub(crate) fn json(status: u16, value: &serde_json::Value) -> Response {
    let mut headers = vec![("content-type", "application/json")];
    headers.extend_from_slice(&CORS);
    respond(status, &headers, value.to_string().into_bytes())
}
