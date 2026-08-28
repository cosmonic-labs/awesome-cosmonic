//! `wasi:http@0.3.0` response plumbing.
//!
//! A p3 body is a native component-model `stream<u8>`, not a `wasi:io`
//! pollable, so a response is built by handing `Response::new` the read end of
//! a stream and letting a `spawn_local` task own the write end. The header
//! goes out as soon as the handler returns, while the body is still streaming.

use crate::bindings;
use crate::bindings::wasi::http::types::{Fields, Response};

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

pub(crate) fn json(status: u16, value: &serde_json::Value) -> Response {
    respond(
        status,
        &[
            ("content-type", "application/json"),
            ("cache-control", "no-store"),
        ],
        value.to_string().into_bytes(),
    )
}
