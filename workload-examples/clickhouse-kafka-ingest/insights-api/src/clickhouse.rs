//! Talking to ClickHouse over its HTTP interface, via `wasi:http/client@0.3.0`.
//!
//! ClickHouse accepts a query as the POST body and returns whatever format the
//! query asks for. `FORMAT JSON` gives `{"meta": [...], "data": [...],
//! "rows": N, "statistics": {...}}`, which is all this component needs - so
//! there is no driver, no connection pool, and no native protocol. Outbound
//! HTTP is the only capability involved.
//!
//! `client::send` is an `async func` in WIT, so a query is a plain `.await`:
//! no poll loop, no `wasi:io` streams, no manual readiness handling.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::bindings;
use crate::bindings::wasi::http::client;
use crate::bindings::wasi::http::types::{Fields, Method, Request, Response, Scheme};
use crate::config::config;

/// Runs a query and returns the `data` array from ClickHouse's JSON envelope.
///
/// Every caller passes a `&'static str`. Nothing from the request is
/// concatenated into SQL anywhere in this component, which is the only reason
/// it is safe to send the query as an opaque body.
pub(crate) async fn query(sql: &'static str) -> Result<Vec<Value>> {
    let cfg = config();

    let headers = Fields::new();
    let _ = headers.append("x-clickhouse-user", cfg.user.as_bytes());
    let _ = headers.append("x-clickhouse-key", cfg.password.as_bytes());
    let _ = headers.append("x-clickhouse-database", cfg.database.as_bytes());

    // The query goes out as a streamed body written by its own task, so the
    // request head is on the wire before the body is fully queued.
    let (mut body_tx, body_rx) = bindings::wit_stream::new();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| Ok(None));
    wit_bindgen::spawn_local(async move {
        body_tx.write_all(sql.as_bytes().to_vec()).await;
        drop(body_tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });

    let (request, _sent) = Request::new(headers, Some(body_rx), trailers_rx, None);
    let _ = request.set_method(&Method::Post);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some(&cfg.authority));
    let _ = request.set_path_with_query(Some("/"));

    let response = client::send(request)
        .await
        .map_err(|e| anyhow::anyhow!("ClickHouse request to {} failed: {e:?}", cfg.authority))?;

    let status = response.get_status_code();
    let (result_tx, result_rx) = bindings::wit_future::new(|| Ok(()));
    let (contents, _trailers) = Response::consume_body(response, result_rx);
    let body = contents.collect().await;
    drop(result_tx);

    if !(200..300).contains(&status) {
        // ClickHouse puts a genuinely useful error in the body, including the
        // failing position in the query. Do not swallow it.
        bail!(
            "ClickHouse returned {status}: {}",
            String::from_utf8_lossy(&body).trim()
        );
    }

    let envelope: Value =
        serde_json::from_slice(&body).context("ClickHouse response was not valid JSON")?;

    match envelope.get("data") {
        Some(Value::Array(rows)) => Ok(rows.clone()),
        _ => bail!("ClickHouse response had no data array"),
    }
}

/// Runs a query expected to return a single row, e.g. an aggregate with no
/// GROUP BY. Returns `Value::Null` when the table is empty.
pub(crate) async fn query_one(sql: &'static str) -> Result<Value> {
    Ok(query(sql).await?.into_iter().next().unwrap_or(Value::Null))
}
