//! MCP server that drives a live Adobe Illustrator instance.
//!
//! This component exports [`wasi:http/handler@0.3.0`] (WASI p3) and serves the
//! Model Context Protocol over the streamable HTTP transport from the official
//! [`rmcp`] SDK, in the stateless mode introduced by the 2026-07-28 MCP
//! specification.
//!
//! It answers on two surfaces:
//!
//! - the **MCP transport**, for clients — see [`server`] for the pure-compute
//!   helper tools and [`live`] for the ones that drive Illustrator;
//! - **`/bridge/*`**, polled by the bridge running inside Illustrator (the CEP
//!   panel, or the pump script), which claims queued commands and posts
//!   results back. Those routes are handled in [`serve_bridge_endpoint`],
//!   deliberately before the request lock: a poll queued behind the tool call
//!   that is waiting for it would deadlock the pair.
//!
//! MCP responses stream: headers are returned to the host as soon as `rmcp`
//! produces them, and body frames (including SSE events from long-running
//! tools) are pumped to the `wasi:http` body stream as they materialize. See
//! [`bridge`] for how the tokio and component-model async worlds interlock,
//! and [`bridge::outbound`] for how tool code performs outbound HTTP through
//! the `wasi:http` client bindings.
//!
//! Two things are served besides the protocol itself: a discovery document on
//! the default route (see [`discovery`]) so `GET /` is useful rather than a
//! dead end, and **skills** — natural-language playbooks published over the
//! MCP resources primitive (see [`skills`]).
//!
//! [`wasi:http/handler@0.3.0`]: https://github.com/WebAssembly/wasi-http

pub mod bridge;
mod discovery;
mod live;
mod server;
pub mod skills;
mod state;
mod telemetry;

use std::pin::Pin;
use std::sync::Arc;

use http_body::Body;
use http_body_util::{BodyExt, Full};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use wasip3::http::types::{ErrorCode, Fields, Request, Response};
use wasip3::http_compat::{http_from_wasi_request, BodyWriter};

/// Deadline for the host to accept one response-body frame (see the pump).
const SEND_FRAME_TIMEOUT_MS: u64 = 60_000;

struct Component;

wasip3::http::service::export!(Component);

impl wasip3::exports::http::handler::Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        telemetry::init();

        let config = transport_config();
        let max_body = config.max_request_body_bytes;

        let request = http_from_wasi_request(request)?;

        // Default route. Answered before the transport, the request lock, and
        // the body read: a health probe must never queue behind an in-flight
        // MCP exchange (a live Illustrator command can hold one for minutes).
        // Every other request — including every POST — falls through.
        if discovery::matches(&request) {
            let body = discovery::document(&server::IllustratorServer::tool_names());
            return static_response(200, "application/json", body);
        }

        let (parts, body) = request.into_parts();

        // Buffer the request body in component-model context — an MCP request
        // is a single JSON-RPC message — but never more than the transport's
        // limit: rmcp's own 413 check only bounds memory when it drains the
        // stream itself, and here it receives an already-materialized body.
        // This happens BEFORE taking the request lock so a slow or stalled
        // upload can't block other exchanges.
        let bytes = match http_body_util::Limited::new(body, max_body).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(err) if err.is::<http_body_util::LengthLimitError>() => {
                return payload_too_large(max_body);
            }
            Err(err) => {
                return Err(ErrorCode::InternalError(Some(format!(
                    "failed to read body: {err}"
                ))));
            }
        };
        // The Illustrator bridge's own endpoints are served here, before the
        // request lock. They touch neither rmcp nor the tokio world — just the
        // keyvalue store — and they MUST NOT queue behind an in-flight tool
        // call, because that tool call is very likely waiting on the poll they
        // are trying to make.
        if let Some(response) = serve_bridge_endpoint(&parts, &bytes) {
            return response;
        }

        let request = http::Request::from_parts(parts, Full::new(bytes));

        // One exchange at a time per instance: the bridge can only drive one
        // tokio-world computation at once. Hosts scale out with more
        // instances, not intra-instance concurrency.
        let guard = bridge::request_lock().lock_owned().await;

        // Drop outbound jobs left over from a previous exchange that ended
        // early (e.g. the peer dropped the response stream mid-body): their
        // tools are dead, and servicing them here would run stale work under
        // this request's context.
        bridge::drain_stale_jobs();

        let span = telemetry::request_span(&request);
        let service = StreamableHttpService::new(
            || Ok(server::IllustratorServer::new()),
            Arc::new(NeverSessionManager::default()),
            config,
        );

        // Run rmcp dispatch until response headers are available. Tool code
        // runs in here; any outbound HTTP it submits is serviced by the
        // bridge along the way.
        let response = bridge::drive(tracing::Instrument::instrument(
            service.handle(request),
            span.clone(),
        ))
        .await;
        let (parts, mut body) = response.into_parts();

        // Hand the host a streaming response and pump rmcp's body frames from
        // a spawned component-model task: SSE events flow out as tools make
        // progress, and outbound requests keep being serviced until the body
        // completes.
        let headers = parts
            .headers
            .try_into()
            .map_err(|err| ErrorCode::InternalError(Some(format!("invalid headers: {err}"))))?;
        let (mut writer, body_rx, result_rx) = BodyWriter::new();
        let (wasi_response, transmit) = Response::new(headers, Some(body_rx), result_rx);
        wasi_response
            .set_status_code(parts.status.as_u16())
            .map_err(|()| ErrorCode::InternalError(Some("invalid status code".into())))?;

        wasip3::wit_bindgen::spawn(async move {
            let _guard = guard;
            loop {
                let frame = bridge::drive(tracing::Instrument::instrument(
                    std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
                    span.clone(),
                ))
                .await;
                match frame {
                    Some(Ok(frame)) => {
                        // Deadline on handing the frame to the host: a client
                        // that keeps the connection open but stops reading
                        // would otherwise park this pump forever while it
                        // holds the request lock. SSE keep-alives arrive
                        // every 15s, so a healthy peer never comes close.
                        match bridge::timeout(SEND_FRAME_TIMEOUT_MS, writer.send_frame(frame)).await
                        {
                            Ok(Ok(_)) => {}
                            // Peer dropped the read end or stopped reading;
                            // stop pumping and fail any tools mid-fetch.
                            Ok(Err(_)) | Err(bridge::TimedOut) => {
                                bridge::drain_stale_jobs();
                                return;
                            }
                        }
                    }
                    // The transport's body error type is Infallible.
                    Some(Err(never)) => match never {},
                    None => break,
                }
            }
            let trailers = std::mem::take(&mut writer.trailers);
            let trailers = (!trailers.is_empty())
                .then(|| trailers.try_into().ok())
                .flatten();
            drop(writer.stream_writer);
            let _ = writer.result_writer.write(Ok(trailers)).await;
            bridge::drain_stale_jobs();
            // Release the request lock before observing the transmission
            // result — this wait is for diagnostics only and must not extend
            // the exchange.
            drop(_guard);
            let transmit = std::future::IntoFuture::into_future(transmit);
            if let Ok(Err(code)) = bridge::timeout(SEND_FRAME_TIMEOUT_MS, transmit).await {
                tracing::warn!(parent: &span, error = %code, "response transmission failed");
            }
        });

        Ok(wasi_response)
    }
}

/// Transport configuration: fully stateless per the 2026-07-28 spec.
///
/// The 2026-07-28 MCP specification makes the protocol core stateless
/// (SEP-2567); disabling legacy session mode extends the same behavior to
/// clients speaking older protocol revisions, which lets this component scale
/// horizontally with no session affinity. Everything else keeps the SDK's
/// defaults — in particular responses use SSE streaming when the client
/// accepts it.
fn transport_config() -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    // DNS-rebinding guard. rmcp's default only accepts a `Host` of
    // localhost/127.0.0.1/::1 — the safe default for local development, and
    // it stays in force unless `MCP_ALLOWED_HOSTS` is set. Deployments where
    // requests arrive under another name (e.g. Cosmonic routes by Host
    // header, `illustrator-mcp.localhost`) must set `MCP_ALLOWED_HOSTS` to the
    // comma-separated hosts to accept (`host` matches any port, `host:port`
    // is exact), or to `*` to disable the guard entirely when an ingress in
    // front of the component already pins the Host.
    if let Ok(hosts) = std::env::var("MCP_ALLOWED_HOSTS") {
        config.allowed_hosts = match hosts.trim() {
            "*" => Vec::new(), // empty list disables the guard in rmcp
            _ => hosts
                .split(',')
                .map(|host| host.trim().to_owned())
                .filter(|host| !host.is_empty())
                .collect(),
        };
    }
    config
}

/// The Illustrator ExtendScript command library. Shared verbatim by both
/// bridge vehicles and served at `/bridge/commands.jsx` so the installed copy
/// can never drift from what this component expects.
const COMMANDS_JSX: &str = include_str!("../bridge/commands.jsx");

/// The self-contained pump script: command library + Socket HTTP client +
/// bounded drain loop. Runs from File > Scripts (or AppleScript `do
/// javascript`) with zero installation.
const PUMP_JSX: &str = concat!(
    include_str!("../bridge/commands.jsx"),
    "\n",
    include_str!("../bridge/pump-driver.jsx"),
);

/// The AppleScript shuttle: the zero-install bridge vehicle for Illustrator
/// 2023+ (whose ExtendScript lost the `Socket` class the pump needs). Claims
/// commands over HTTP from a terminal and executes them in Illustrator via
/// `osascript`.
const SHUTTLE_SH: &str = include_str!("../bridge/shuttle.sh");

/// CEP extension files, for the persistent panel (Window > Extensions).
const CEP_MANIFEST: &str = include_str!("../bridge/cep/CSXS/manifest.xml");
const CEP_INDEX: &str = include_str!("../bridge/cep/index.html");
const CEP_MAIN: &str = include_str!("../bridge/cep/main.js");

/// Serves the endpoints the Illustrator bridge uses, or returns `None` for
/// anything that belongs to the MCP transport.
///
/// Everything here is synchronous keyvalue work: no tokio, no request lock,
/// no outbound I/O. Responses carry permissive CORS headers because the CEP
/// panel calls these routes with XHR from a `file://` origin.
fn serve_bridge_endpoint(
    parts: &http::request::Parts,
    body: &bytes::Bytes,
) -> Option<Result<Response, ErrorCode>> {
    let path = parts.uri.path();
    if !(path.starts_with("/bridge/") || path == "/healthz") {
        return None;
    }
    // CORS preflight for the CEP panel's POST /bridge/result.
    if parts.method == http::Method::OPTIONS {
        return Some(simple_response(204, "text/plain", ""));
    }
    let query = parts.uri.query();
    match (&parts.method, path) {
        (&http::Method::GET, "/bridge/command") => {
            // `v=2` marks a bridge that can parse our responses; older ones
            // mishandle LF-only headers and would consume commands then drop
            // them. `client` is the bridge instance — the newest wins, so
            // reloading the panel supersedes the old one instead of the two
            // racing for commands.
            let client = query.and_then(|q| param(q, "client")).unwrap_or(0);
            let current = state::register_client(client).unwrap_or(false);
            let serving = query.and_then(|q| param::<u32>(q, "v")) == Some(2) && current;

            // Stamp every poll, served or not: recording only served polls
            // makes a stale bridge indistinguishable from no bridge at all,
            // which sends whoever is debugging to the wrong fix.
            let _ = state::record_poll(serving);

            let body = if serving {
                match state::take_pending_command() {
                    Ok(Some(command)) => command.to_string(),
                    Ok(None) => json!({ "command": null }).to_string(),
                    Err(err) => {
                        return Some(json_response(500, &json!({ "error": err })));
                    }
                }
            } else {
                json!({
                    "command": null,
                    "note": "this bridge is outdated or has been superseded by a newer \
                             bridge instance; reinstall with ./install-bridge.sh and \
                             reopen the Illustrator MCP Bridge panel (or rerun the pump)",
                })
                .to_string()
            };
            Some(simple_response(200, "application/json", body))
        }
        (&http::Method::POST, "/bridge/result") => {
            let id = query.and_then(|q| param(q, "id"));
            Some(match state::store_result(id, body) {
                Ok(()) => json_response(200, &json!({ "status": "ok" })),
                Err(err) => json_response(500, &json!({ "error": err })),
            })
        }
        (&http::Method::GET, "/bridge/commands.jsx") => Some(simple_response(
            200,
            "application/javascript; charset=utf-8",
            COMMANDS_JSX,
        )),
        (&http::Method::GET, "/bridge/pump.jsx") => Some(simple_response(
            200,
            "application/javascript; charset=utf-8",
            PUMP_JSX,
        )),
        (&http::Method::GET, "/bridge/shuttle.sh") => Some(simple_response(
            200,
            "text/x-shellscript; charset=utf-8",
            SHUTTLE_SH,
        )),
        (&http::Method::GET, "/bridge/cep/manifest.xml") => {
            Some(simple_response(200, "application/xml", CEP_MANIFEST))
        }
        (&http::Method::GET, "/bridge/cep/index.html") => Some(simple_response(
            200,
            "text/html; charset=utf-8",
            CEP_INDEX,
        )),
        (&http::Method::GET, "/bridge/cep/main.js") => Some(simple_response(
            200,
            "application/javascript; charset=utf-8",
            CEP_MAIN,
        )),
        (&http::Method::GET, "/healthz") => Some(simple_response(200, "text/plain", "ok\n")),
        _ => None,
    }
}

/// Reads one `name=value` pair out of a query string.
fn param<T: std::str::FromStr>(query: &str, name: &str) -> Option<T> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| value.parse().ok())
}

fn json_response(status: u16, value: &serde_json::Value) -> Result<Response, ErrorCode> {
    simple_response(status, "application/json", value.to_string())
}

/// Builds a complete, non-streaming response outside the MCP transport.
fn simple_response(
    status: u16,
    content_type: &str,
    body: impl Into<bytes::Bytes>,
) -> Result<Response, ErrorCode> {
    let headers = Fields::from_list(&[
        ("content-type".to_string(), content_type.as_bytes().to_vec()),
        // The CEP panel calls the bridge routes from a file:// origin.
        ("access-control-allow-origin".to_string(), b"*".to_vec()),
        (
            "access-control-allow-methods".to_string(),
            b"GET, POST, OPTIONS".to_vec(),
        ),
        (
            "access-control-allow-headers".to_string(),
            b"content-type".to_vec(),
        ),
    ])
    .map_err(|err| ErrorCode::InternalError(Some(format!("invalid headers: {err}"))))?;
    let (mut writer, body_rx, result_rx) = BodyWriter::new();
    let (response, _transmit) = Response::new(headers, Some(body_rx), result_rx);
    response
        .set_status_code(status)
        .map_err(|()| ErrorCode::InternalError(Some("invalid status code".into())))?;
    let body = body.into();
    wasip3::wit_bindgen::spawn(async move {
        if !body.is_empty() {
            let _ = writer.send_frame(http_body::Frame::data(body)).await;
        }
        drop(writer.stream_writer);
        let _ = writer.result_writer.write(Ok(None)).await;
    });
    Ok(response)
}

/// Builds a complete, non-streaming response with **no** CORS headers.
///
/// The discovery document uses this rather than [`simple_response`]
/// deliberately: the bridge routes are opened to a `file://` origin because
/// the CEP panel needs it, but nothing should let an arbitrary browser page
/// read this server's identity, tool list, or bridge state cross-origin.
fn static_response(
    status: u16,
    content_type: &str,
    body: impl Into<bytes::Bytes>,
) -> Result<Response, ErrorCode> {
    let headers = Fields::from_list(&[
        ("content-type".to_string(), content_type.as_bytes().to_vec()),
        ("cache-control".to_string(), b"no-store".to_vec()),
    ])
    .map_err(|err| ErrorCode::InternalError(Some(format!("invalid headers: {err}"))))?;
    let (mut writer, body_rx, result_rx) = BodyWriter::new();
    let (response, _transmit) = Response::new(headers, Some(body_rx), result_rx);
    response
        .set_status_code(status)
        .map_err(|()| ErrorCode::InternalError(Some("invalid status code".into())))?;
    let body = body.into();
    wasip3::wit_bindgen::spawn(async move {
        if !body.is_empty() {
            let _ = writer.send_frame(http_body::Frame::data(body)).await;
        }
        drop(writer.stream_writer);
        let _ = writer.result_writer.write(Ok(None)).await;
    });
    Ok(response)
}

/// Builds a plain-text `413 Payload Too Large` response without involving the
/// MCP transport (the request was rejected before it could be parsed).
fn payload_too_large(limit: usize) -> Result<Response, ErrorCode> {
    static_response(
        413,
        "text/plain; charset=utf-8",
        format!("Payload Too Large: request body exceeds {limit} bytes"),
    )
}
