//! Insights API: the read side of the pipeline. A wasip3 component.
//!
//! Serves a dashboard and a small JSON API over the tables that
//! `infra/clickhouse/init/01-ingest-pipeline.sql` fills. It holds no state and
//! caches nothing; every number on the page is a live query.
//!
//! Being wasip3 means the entrypoint is `wasi:http/handler@0.3.0`: one
//! `async fn handle` returning a response, with bodies as native
//! `stream<u8>`. Each ClickHouse query is a plain `.await` on
//! `wasi:http/client@0.3.0`.
//!
//! Routes:
//!   GET /                a dashboard over the tables below
//!   GET /api/overview    totals, time series, breakdowns, consumer health
//!   GET /api/recent      the newest raw events
//!   GET /api/errors      dead-lettered messages
//!   GET /healthz         liveness, including whether ClickHouse answers

use anyhow::Result;
use serde_json::{Value, json};

mod bindings {
    wit_bindgen::generate!({
        world: "insights-api",
        path: "wit",
        generate_all,
    });
}

mod clickhouse;
mod config;
mod http;
mod queries;

use bindings::exports::wasi::http::handler::Guest;
use bindings::wasi::http::types::{ErrorCode, Method, Request, Response};

use config::config;
use http::{json, respond};

static UI_HTML: &str = include_str!("../ui.html");

struct Component;

impl Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let method = request.get_method();
        let target = request.get_path_with_query().unwrap_or_default();
        let path = target.split('?').next().unwrap_or("/").to_string();

        Ok(match (&method, path.as_str()) {
            (Method::Get, "/") => dashboard(),
            (Method::Get, "/api/overview") => finish(overview().await),
            (Method::Get, "/api/recent") => finish(recent().await),
            (Method::Get, "/api/errors") => finish(errors().await),
            (Method::Get, "/healthz") => finish(health().await),
            _ => json(404, &json!({"error": "not found"})),
        })
    }
}

/// Everything the dashboard needs in one round trip.
async fn overview() -> Result<Value> {
    Ok(json!({
        "totals": clickhouse::query_one(queries::TOTALS).await?,
        "per_minute": clickhouse::query(queries::PER_MINUTE).await?,
        "by_event_type": clickhouse::query(queries::BY_EVENT_TYPE).await?,
        "by_country": clickhouse::query(queries::BY_COUNTRY).await?,
        "consumers": clickhouse::query(queries::CONSUMERS).await?,
    }))
}

async fn recent() -> Result<Value> {
    Ok(json!({ "events": clickhouse::query(queries::RECENT).await? }))
}

async fn errors() -> Result<Value> {
    Ok(json!({ "errors": clickhouse::query(queries::ERRORS).await? }))
}

/// Reports unhealthy if ClickHouse cannot be reached, so this is a usable
/// readiness probe rather than a check that the component is merely running.
async fn health() -> Result<Value> {
    let cfg = config();
    let reachable = clickhouse::query_one("SELECT 1 AS ok FORMAT JSON")
        .await
        .is_ok();
    Ok(json!({
        "status": if reachable { "ok" } else { "degraded" },
        "clickhouse_url": cfg.url,
        "clickhouse_database": cfg.database,
        "clickhouse_reachable": reachable,
        "wasi_http": "0.3.0",
    }))
}

/// The dashboard needs the gateway's origin to drive the traffic generator.
/// Injected as JSON rather than templated into markup so a config value can
/// never become markup.
fn dashboard() -> Response {
    let gateway = serde_json::to_string(&config().gateway_url)
        .unwrap_or_else(|_| "\"http://localhost:8000\"".to_string());
    let page = UI_HTML.replace("\"__GATEWAY_URL__\"", &gateway);

    respond(
        200,
        &[("content-type", "text/html; charset=utf-8")],
        page.into_bytes(),
    )
}

/// A query failure is an upstream failure, so it surfaces as 502 with the
/// ClickHouse message intact - that message is usually the actual answer.
fn finish(result: Result<Value>) -> Response {
    match result {
        Ok(value) => json(200, &value),
        Err(e) => {
            eprintln!("query failed: {e:?}");
            json(502, &json!({"error": format!("{e:#}")}))
        }
    }
}

bindings::export!(Component with_types_in bindings);
