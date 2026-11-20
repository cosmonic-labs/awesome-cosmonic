//! Optional client for the NATS monitoring endpoints — the server-wide
//! telemetry the NATS binding structurally cannot reach.
//!
//! ## Why this is a second road, not a wider grant
//!
//! Server-wide state — connection counts, slow consumers, memory and storage
//! pressure, the whole stream inventory — lives behind `$SYS` and the
//! JetStream API. Both are *host-reserved* on the `wasmcloud:nats` binding: a
//! denial there carries the `reserved` reason, whose own WIT documentation
//! says "No grant can open this one". That is a deliberate boundary rather
//! than a missing grant, so no amount of `subject-allow` will ever open it,
//! and a diagnostic surface that needs this data has to get it elsewhere.
//!
//! A NATS server started with `-m <port>` publishes the same data over plain
//! HTTP, which a component *can* reach through `wasi:http` under its own
//! `allowedHosts` policy. This module takes that road. It is:
//!
//! - **Optional.** With `MCP_NATS_MONITOR_URL` unset every tool here returns a
//!   `monitor-not-configured` failure spelling out how to turn it on, and the
//!   NATS-binding tools are entirely unaffected.
//! - **Read-only by construction.** The monitoring endpoints expose no
//!   mutating verbs; this module only ever issues GETs.
//! - **Gated by the outbound allow-list.** `allowedHosts` is deny-all by
//!   default, so the host in `MCP_NATS_MONITOR_URL` must be listed explicitly
//!   before a single request leaves the component.
//!
//! ## Choosing an address that actually resolves
//!
//! The address has to be reachable from the component's *outbound HTTP* path,
//! and that path resolves names with the ordinary system resolver. Two
//! consequences, both load-bearing:
//!
//! - **`127.0.0.1` never works.** Inside the sandbox loopback is the guest's
//!   own virtual network, not the machine the host runs on.
//! - **`host.wasmcloud.internal` does not work here either — yet.** That
//!   reserved name is the documented door to the machine's loopback, but it is
//!   resolved inside the `wasi:sockets` path only. The `wasi:http` client this
//!   module goes through builds a plain connector over the system resolver and
//!   never consults the reserved zone, so the name fails with a DNS error
//!   ("address not available") no matter what `allowedHostLoopbackPorts` says
//!   or whether the operator's loopback switch is on.
//!
//! What works today is an address the system resolver can answer for and the
//! NATS server actually listens on. `nats-server -m 8222` binds every
//! interface, so the machine's own LAN address reaches it:
//!
//! ```yaml
//! localResources:
//!   environment:
//!     config:
//!       MCP_NATS_MONITOR_URL: "http://192.168.1.43:8222"
//!   allowedHosts: ["192.168.1.43"]
//!   # Harmless today (the http path ignores it) and already correct for the
//!   # day host.wasmcloud.internal is honored here.
//!   allowedHostLoopbackPorts: ["8222"]
//! ```
//!
//! A LAN address moves with DHCP, so a stable hostname or a fixed address for
//! the NATS host is the better answer anywhere but a laptop. A remote NATS
//! server needs nothing special: name its host and put it in `allowedHosts`.

use bytes::Bytes;
use serde_json::Value;

use crate::bridge::outbound;
use crate::nats::Failure;

/// Base URL of the NATS monitoring port. Unset disables every tool here.
const ENDPOINT_VAR: &str = "MCP_NATS_MONITOR_URL";

/// The configured monitoring base URL, with any trailing slash trimmed.
///
/// `None` means the operator did not opt in, which is the default and not an
/// error — callers should render [`not_configured`] rather than failing hard.
pub fn endpoint() -> Option<String> {
    std::env::var(ENDPOINT_VAR).ok().and_then(|raw| {
        let trimmed = raw.trim().trim_end_matches('/').to_owned();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}

/// Whether server-wide telemetry is available in this deployment.
pub fn is_enabled() -> bool {
    endpoint().is_some()
}

/// The failure returned by every monitoring tool when the operator has not
/// opted in. It carries the whole enablement recipe: an agent that hits this
/// should be able to hand the user something actionable without guessing.
pub fn not_configured() -> Failure {
    Failure::new(
        "monitor-not-configured",
        "server-wide telemetry is off: this workload has no NATS monitoring \
         endpoint configured. It is deliberately separate from the NATS \
         binding, because connection counts, slow consumers, and the stream \
         inventory live behind $SYS and the JetStream API, which the binding \
         reserves for the host and no grant can open. To turn it on: (1) start \
         NATS with a monitoring port (`nats-server -m 8222`); (2) set \
         MCP_NATS_MONITOR_URL on this workload — use \
         an address the component can actually resolve — NOT 127.0.0.1 \
         (that is the guest's own virtual network) and NOT \
         host.wasmcloud.internal (that reserved name is honored on the \
         wasi:sockets path, not on outbound HTTP). For a NATS server on this \
         machine, `nats-server -m` binds every interface, so the machine's LAN \
         address works: http://<lan-ip>:8222; (3) add that same host to \
         `allowedHosts`, which is deny-all by default. Every other tool on \
         this server keeps working without any of this",
    )
}

/// GETs one monitoring endpoint and parses the response as JSON.
///
/// `path` is a root-relative path with any query string already attached
/// (`"/varz"`, `"/connz?limit=64"`). Errors are shaped like every other
/// [`Failure`] so tool code can render them uniformly.
pub async fn get(path: &str) -> Result<Value, Failure> {
    let Some(base) = endpoint() else {
        return Err(not_configured());
    };
    let url = format!("{base}{path}");

    let request = http::Request::get(&url)
        .header("accept", "application/json")
        .body(Bytes::new())
        .map_err(|err| {
            Failure::new(
                "monitor-bad-url",
                format!("{ENDPOINT_VAR} does not form a valid URL ({url}): {err}"),
            )
        })?;

    let response = outbound::fetch(request).await.map_err(|err| {
        Failure::new(
            "monitor-unreachable",
            format!(
                "could not reach the NATS monitoring endpoint at {url}: {err}. \
                 Check, in order: NATS is running with `-m`; the host in the \
                 URL is in `allowedHosts` (deny-all by default); and the URL \
                 names an address outbound HTTP can resolve. A DNS error on \
                 `host.wasmcloud.internal` is expected — that reserved name is \
                 resolved on the wasi:sockets path, not on outbound HTTP. Use \
                 the machine's LAN address instead (`nats-server -m` binds \
                 every interface), or a real hostname for a remote server. \
                 127.0.0.1 is the guest's own virtual network and never reaches \
                 the machine"
            ),
        )
    })?;

    let status = response.status();
    if !status.is_success() {
        return Err(Failure::new(
            "monitor-http-error",
            format!(
                "the NATS monitoring endpoint at {url} answered HTTP {}. \
                 The path may not exist on this server version",
                status.as_u16()
            ),
        ));
    }

    serde_json::from_slice(response.body()).map_err(|err| {
        Failure::new(
            "monitor-bad-response",
            format!(
                "the response from {url} was not the JSON this expects: {err}. \
                 Confirm {ENDPOINT_VAR} points at a NATS monitoring port and \
                 not at some other HTTP service"
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Endpoint shortcuts
// ---------------------------------------------------------------------------

/// `/healthz` — the server's own readiness verdict.
pub async fn healthz() -> Result<Value, Failure> {
    get("/healthz").await
}

/// `/varz` — server identity, uptime, connection and subscription counts,
/// slow-consumer count, memory/CPU, and cumulative message counters.
pub async fn varz() -> Result<Value, Failure> {
    get("/varz").await
}

/// `/jsz` — JetStream usage against its configured limits.
///
/// With `detailed` the response also carries `account_details[].stream_detail`,
/// every stream's `config` and `state`, and each stream's `consumer_detail`.
/// That is the whole inventory in one request, which is what makes enumeration
/// possible at all: the NATS binding has no list-streams call, so without this
/// a caller must already know every name it wants to inspect.
pub async fn jsz(detailed: bool) -> Result<Value, Failure> {
    if detailed {
        get("/jsz?accounts=true&streams=true&consumers=true&config=true").await
    } else {
        get("/jsz").await
    }
}

/// `/connz` — the connected clients, newest first by default.
pub async fn connz(limit: u32) -> Result<Value, Failure> {
    get(&format!("/connz?limit={}&subs=true", limit.clamp(1, 512))).await
}
