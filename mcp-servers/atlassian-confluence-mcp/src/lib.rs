//! MCP server template for Cosmonic Desktop.
//!
//! This component exports [`wasi:http/handler@0.3.0`] (WASI p3) and serves the
//! Model Context Protocol over the streamable HTTP transport from the official
//! [`rmcp`] SDK, in the stateless mode introduced by the 2026-07-28 MCP
//! specification.
//!
//! Responses stream: headers are returned to the host as soon as `rmcp`
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

mod body;
pub mod bridge;
mod confluence;
mod discovery;
mod server;
pub mod skills;
mod telemetry;

use std::pin::Pin;
use std::sync::Arc;

use http_body::Body;
use http_body_util::{BodyExt, Full};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
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
        // MCP exchange. Every other request — including every POST — falls
        // through to the MCP transport unchanged.
        if discovery::matches(&request) {
            let body = discovery::document(&server::TemplateServer::tool_names());
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
            || Ok(server::TemplateServer::new()),
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
    // header, `mcp-server.localhost`) must set `MCP_ALLOWED_HOSTS` to the
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

/// Builds a plain-text `413 Payload Too Large` response without involving the
/// MCP transport (the request was rejected before it could be parsed).
fn payload_too_large(limit: usize) -> Result<Response, ErrorCode> {
    static_response(
        413,
        "text/plain; charset=utf-8",
        format!("Payload Too Large: request body exceeds {limit} bytes"),
    )
}

/// Builds a complete, already-known response body — used for the routes this
/// component answers itself rather than handing to the MCP transport.
///
/// The body still goes out through a spawned writer because `wasi:http`
/// responses are always streamed; there is simply nothing to wait for between
/// frames.
fn static_response(status: u16, content_type: &str, body: String) -> Result<Response, ErrorCode> {
    let headers = Fields::from_list(&[
        ("content-type".to_string(), content_type.as_bytes().to_vec()),
        // No CORS headers anywhere in this component, deliberately: a browser
        // page must not be able to read any of it cross-origin.
        ("cache-control".to_string(), b"no-store".to_vec()),
    ])
    .map_err(|err| ErrorCode::InternalError(Some(format!("invalid headers: {err}"))))?;
    let (mut writer, body_rx, result_rx) = BodyWriter::new();
    let (response, _transmit) = Response::new(headers, Some(body_rx), result_rx);
    response
        .set_status_code(status)
        .map_err(|()| ErrorCode::InternalError(Some("invalid status code".into())))?;
    wasip3::wit_bindgen::spawn(async move {
        let frame = http_body::Frame::data(bytes::Bytes::from(body));
        let _ = writer.send_frame(frame).await;
        drop(writer.stream_writer);
        let _ = writer.result_writer.write(Ok(None)).await;
    });
    Ok(response)
}
