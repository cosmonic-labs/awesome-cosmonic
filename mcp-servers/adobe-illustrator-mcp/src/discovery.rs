//! The default route: a discovery/health document on `GET /`.
//!
//! The MCP specification does not say what a server should return on its root
//! path, but returning `404`/`405` there is a poor default: a developer
//! pasting the URL into a browser, a load balancer probing the workload, and a
//! crawler indexing the deployment all get a dead end, and there is no way to
//! ask "what is this and what does it offer?" without completing a protocol
//! handshake.
//!
//! So `GET`/`HEAD` on `/` and `/health` return `200` with a small JSON
//! document: server identity, the MCP revision spoken, where the protocol
//! endpoints are, the tool names, the skills served (see [`crate::skills`]),
//! and — because this server is useless without one — the state of the
//! Illustrator bridge and how to start it. Protocol traffic stays on its own
//! verb and paths; everything else, including every `POST`, falls through to
//! the MCP transport untouched.
//!
//! `/healthz` keeps returning plain `ok` for the bridge's own liveness check
//! (see `serve_bridge_endpoint` in the crate root); it predates this document
//! and the shuttle script parses it.
//!
//! ## Why this route is not behind the Host guard
//!
//! The DNS-rebinding guard (`MCP_ALLOWED_HOSTS`) protects the MCP endpoint.
//! This document is deliberately outside it so health checks work under
//! whatever `Host` a probe sends. That is safe because it carries only
//! information a successful `initialize` would return anyway, and the server
//! emits no CORS headers on it, so a browser page cannot read the response
//! cross-origin.

use crate::{skills, state};

/// Canonical path for MCP protocol traffic. `POST` to any path reaches the
/// transport (`/` included, which is what the Cosmonic Desktop ingress and
/// most clients use); this is the path to prefer in new client config.
const MCP_PATH: &str = "/mcp";

/// Paths that serve the discovery document on a read verb.
const DISCOVERY_PATHS: &[&str] = &["/", "/health"];

/// A bridge that polled this recently is considered live. Both vehicles poll
/// on a ~2 s timer, so this is several missed cycles' grace.
const BRIDGE_LIVE_MS: u64 = 10_000;

/// Whether this request should be answered with the discovery document rather
/// than handed to the MCP transport.
pub fn matches<B>(request: &http::Request<B>) -> bool {
    matches!(*request.method(), http::Method::GET | http::Method::HEAD)
        && DISCOVERY_PATHS.contains(&request.uri().path())
}

/// The discovery document.
pub fn document(tools: &[String]) -> String {
    let skills: Vec<_> = skills::SKILLS
        .iter()
        .map(|skill| {
            serde_json::json!({
                "name": skill.name,
                "description": skill.description(),
                "uri": skill.entry_uri(),
            })
        })
        .collect();

    // Never fail the probe over the store: a bridge whose state cannot be read
    // is reported as not connected, which is also what the caller should do
    // about it.
    let poll_age_ms = state::last_poll_age_ms().ok().flatten();
    let bridge_connected = poll_age_ms.is_some_and(|age| age < BRIDGE_LIVE_MS);

    let document = serde_json::json!({
        "status": "ok",
        "server": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "description": env!("CARGO_PKG_DESCRIPTION"),
        },
        "protocol": {
            "name": "Model Context Protocol",
            "specVersion": rmcp::model::ProtocolVersion::V_2026_07_28.as_str(),
            "transport": "streamable-http",
            "stateless": true,
        },
        "endpoints": {
            // POST reaches the transport on any path; `/` is kept working for
            // existing clients and for the Desktop ingress.
            "mcp": MCP_PATH,
            "discovery": DISCOVERY_PATHS[0],
            "health": DISCOVERY_PATHS[1],
            // The bridge's own surface, so whoever is setting one up can find
            // the vehicle they need without reading the README.
            "bridgeCommand": "/bridge/command",
            "bridgeResult": "/bridge/result",
            "bridgeShuttle": "/bridge/shuttle.sh",
            "bridgePump": "/bridge/pump.jsx",
            "bridgeCommands": "/bridge/commands.jsx",
            "healthz": "/healthz",
        },
        "capabilities": {
            "tools": tools,
            "resources": {
                // Skills over MCP: the catalog to read first, then the
                // playbooks it points at.
                "extension": skills::EXTENSION_ID,
                "skillIndex": skills::INDEX_URI,
            },
        },
        "skills": skills,
        // Every live tool is a no-op without a bridge, so say so up front
        // rather than making a caller discover it one timed-out call later.
        "bridge": {
            "connected": bridge_connected,
            "lastPollAgeMs": poll_age_ms,
            "hint": if bridge_connected {
                "An Illustrator bridge is polling normally."
            } else {
                "No Illustrator bridge is polling; live tools will queue and time out. \
                 Start one: fetch /bridge/shuttle.sh and run it in a terminal, or install \
                 the CEP panel with ./install-bridge.sh and open Window > Extensions > \
                 Illustrator MCP Bridge."
            },
        },
        "documentation": "https://cosmonic.com/docs/desktop",
    });
    // Serialization of owned data cannot fail, but a component must never
    // panic: a trap takes the whole instance down.
    serde_json::to_string_pretty(&document).unwrap_or_else(|_| String::from(r#"{"status":"ok"}"#))
}
