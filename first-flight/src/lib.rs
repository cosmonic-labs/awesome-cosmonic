//! First Flight — the page a freshly launched WebAssembly workload serves back
//! on the user's own machine. A single `wasi:http` component (no extra
//! capabilities) that serves one self-contained HTML page and proves it is a
//! live program, not a static file: it reflects the workload name and the host
//! it is answering on, and it exposes `/count` so the page's button issues a
//! REAL request the component handles.
//!
//! Deliberately stateless: the wasmCloud runtime instantiates the component per
//! invocation, so there is no reliable cross-request memory to lean on. Rather
//! than fake a persistent counter, the page's live tallies are scoped to the
//! visit and every increment maps to a genuine request round-trip.

use wasmcloud_component::http;

struct Component;

http::export!(Component);

/// The page template, with the brand fonts inlined so the component is fully
/// self-contained (no external requests, same as any sandboxed workload).
const TEMPLATE: &str = include_str!("index.html");

/// Minimal HTML-escaping for the two values that reach the page from the
/// request/env (host authority, workload name) — reflected text must never be
/// able to break out of its text node.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn response(body: String, content_type: &'static str) -> http::Result<http::Response<String>> {
    let mut resp = http::Response::new(body);
    resp.headers_mut()
        .insert("content-type", content_type.parse().unwrap());
    // The page is live per request; never let a proxy or the browser cache it.
    resp.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    Ok(resp)
}

impl http::Server for Component {
    fn handle(
        request: http::IncomingRequest,
    ) -> http::Result<http::Response<impl http::OutgoingBody>> {
        let path = request.uri().path();

        // The page's "Send another request" button hits this: a real request,
        // answered by the component, which the page counts on success.
        if path == "/count" {
            return response(
                String::from("{\"ok\":true}"),
                "application/json; charset=utf-8",
            );
        }

        if path != "/" && path != "/index.html" {
            let mut resp = http::Response::new(String::from("not found\n"));
            *resp.status_mut() = http::StatusCode::NOT_FOUND;
            return Ok(resp);
        }

        // Workload name: configurable via env, defaulting to the Launchpad
        // entry's name. Host: the authority the request came in on (wasi:http
        // carries it on the URI; the classic Host header is the fallback).
        let name = std::env::var("FIRST_FLIGHT_NAME").unwrap_or_else(|_| "first-flight".to_string());
        let host = request
            .uri()
            .authority()
            .map(|a| a.as_str().to_string())
            .or_else(|| {
                request
                    .headers()
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .map(|h| h.to_string())
            })
            .map(|h| h.chars().take(64).collect::<String>())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "your host".to_string());

        let html = TEMPLATE
            .replace("__FF_WORKLOAD__", &escape(&name))
            .replace("__FF_HOST__", &escape(&host));

        response(html, "text/html; charset=utf-8")
    }
}
