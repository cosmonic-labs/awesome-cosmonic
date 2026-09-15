//! Sandboxed Webhook: a signature-verifying forwarder.
//!
//! A public HTTP endpoint that receives a webhook, verifies its HMAC-SHA256
//! signature, and forwards the payload to exactly ONE downstream host. The
//! point of the example is the host's egress policy: `allowedHosts` names the
//! single forward target and denies everything else, so even a fully
//! compromised handler physically cannot exfiltrate the payload anywhere you
//! did not name. Least-privilege egress, enforced by the sandbox rather than by
//! the code.
//!
//! - `GET  /` renders an info page. Once a secret is configured it prints a
//!   ready-to-run `curl` carrying a valid signature; until then it says what is
//!   missing and returns 503, matching what `POST /` would do.
//! - `POST /` verifies `X-Hub-Signature-256: sha256=<hex>` over the raw body,
//!   then forwards the body to `WEBHOOK_FORWARD_URL`.
//!
//! Config (workload environment):
//! - `WEBHOOK_SIGNING_SECRET`: HMAC key. Required. There is no default, so
//!   until it is set every POST is refused with 503 and nothing is forwarded.
//! - `WEBHOOK_FORWARD_URL`: the single downstream URL. Its host MUST be listed
//!   in the workload's `allowedHosts`, or the forward is denied by the sandbox.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use wstd::http::{Body, Client, Method, Request, Response, StatusCode};

type HmacSha256 = Hmac<Sha256>;

/// Default downstream. Its host is the single entry in `allowedHosts`.
const DEFAULT_FORWARD_URL: &str = "https://postman-echo.com/post";
const SIG_HEADER: &str = "x-hub-signature-256";
/// Sample body the info page signs, so the shown `curl` actually succeeds.
const SAMPLE_BODY: &str = r#"{"event":"ping","from":"cosmonic"}"#;
/// Largest body this receiver will accept. A webhook is an internet-facing
/// endpoint, so it should refuse rather than buffer whatever it is handed.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The HMAC key, or `None` when it is unset or empty.
///
/// There is deliberately no default. An earlier version fell back to a secret
/// written in this file, which meant a deployment that skipped the setup step
/// kept verifying signatures against a value published in a public repository:
/// anyone could forge a valid header, and nothing in the response said so. For
/// an example whose entire subject is signature verification, failing closed is
/// the only defensible default.
fn signing_secret() -> Option<String> {
    match std::env::var("WEBHOOK_SIGNING_SECRET") {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    let method = req.method().clone();
    if method == Method::GET || method == Method::HEAD {
        info_page(&req)
    } else if method == Method::POST {
        handle_webhook(req).await
    } else {
        text(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only GET (info) and POST (webhook) are supported.\n",
        )
    }
}

/// Verify the signature, then forward the raw body to the one allow-listed host.
async fn handle_webhook(req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    let Some(secret) = signing_secret() else {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            &Outcome::error(
                "WEBHOOK_SIGNING_SECRET is not set, so no signature can be verified \
                 and nothing will be forwarded",
            ),
        );
    };
    let forward_url = env_or(ForwardVar, DEFAULT_FORWARD_URL);

    let (parts, mut body) = req.into_parts();

    // Refuse an oversized body before reading it, when the client declares one.
    if let Some(declared) = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if declared > MAX_BODY_BYTES {
            return reply(
                StatusCode::PAYLOAD_TOO_LARGE,
                &Outcome::error("body exceeds the 1 MiB limit"),
            );
        }
    }

    let provided_sig = parts
        .headers
        .get(SIG_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();

    let payload = match body.bytes_contents().await {
        Ok(b) => b.to_vec(),
        Err(_) => return text(StatusCode::BAD_REQUEST, "Could not read request body.\n"),
    };
    // A client that lied about (or omitted) content-length still gets refused,
    // after the fact rather than before it.
    if payload.len() > MAX_BODY_BYTES {
        return reply(
            StatusCode::PAYLOAD_TOO_LARGE,
            &Outcome::error("body exceeds the 1 MiB limit"),
        );
    }

    match verify(&secret, &payload, provided_sig.as_deref()) {
        SigResult::Missing => {
            return reply(
                StatusCode::UNAUTHORIZED,
                &Outcome::error("missing X-Hub-Signature-256 header"),
            );
        }
        SigResult::Invalid => {
            return reply(
                StatusCode::UNAUTHORIZED,
                &Outcome::error("signature does not match the configured secret"),
            );
        }
        SigResult::Ok => {}
    }

    // The only outbound call the component makes. It reaches the configured host
    // ONLY because that host is in `allowedHosts`; point WEBHOOK_FORWARD_URL at
    // anything else and the sandbox denies this send.
    let outbound = Request::builder()
        .method(Method::POST)
        .uri(&forward_url)
        .header("content-type", content_type)
        .header("x-forwarded-by", "cosmonic-sandboxed-webhook")
        .body(Body::from(payload));

    let outbound = match outbound {
        Ok(r) => r,
        Err(_) => {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                &Outcome::error(
                    "could not build the outbound request: check WEBHOOK_FORWARD_URL \
                     and the inbound content-type header",
                ),
            );
        }
    };

    match Client::new().send(outbound).await {
        Ok(resp) => reply(
            StatusCode::OK,
            &Outcome::forwarded(&forward_url, resp.status().as_u16()),
        ),
        Err(_) => reply(
            StatusCode::BAD_GATEWAY,
            &Outcome::denied(&forward_url),
        ),
    }
}

/// Result of signature verification.
enum SigResult {
    Missing,
    Invalid,
    Ok,
}

/// Verify `X-Hub-Signature-256: sha256=<hex>` (GitHub-style) over the raw body.
fn verify(secret: &str, payload: &[u8], provided: Option<&str>) -> SigResult {
    let provided = match provided {
        Some(p) => p,
        None => return SigResult::Missing,
    };
    let hex_sig = provided.strip_prefix("sha256=").unwrap_or(provided).trim();
    let expected = match hex::decode(hex_sig) {
        Ok(bytes) => bytes,
        Err(_) => return SigResult::Invalid,
    };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return SigResult::Invalid,
    };
    mac.update(payload);
    // Constant-time comparison, courtesy of the MAC.
    match mac.verify_slice(&expected) {
        Ok(()) => SigResult::Ok,
        Err(_) => SigResult::Invalid,
    }
}

/// JSON body the webhook path returns.
#[derive(serde::Serialize)]
struct Outcome<'a> {
    verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    forwarded_to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    downstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

impl<'a> Outcome<'a> {
    fn forwarded(url: &'a str, status: u16) -> Self {
        Self { verified: true, forwarded_to: Some(url), downstream_status: Some(status), error: None }
    }
    fn denied(url: &'a str) -> Self {
        Self {
            verified: true,
            forwarded_to: Some(url),
            downstream_status: None,
            error: Some("could not reach the forward target. If its host is not in allowedHosts, the sandbox blocked it; otherwise the downstream may be unavailable."),
        }
    }
    fn error(msg: &'a str) -> Self {
        Self { verified: false, forwarded_to: None, downstream_status: None, error: Some(msg) }
    }
}

fn reply(status: StatusCode, outcome: &Outcome<'_>) -> Result<Response<Body>, wstd::http::Error> {
    let body = serde_json::to_string(outcome).unwrap_or_else(|_| "{}".to_owned());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .map_err(Into::into)
}

fn text(status: StatusCode, msg: &str) -> Result<Response<Body>, wstd::http::Error> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(msg.to_owned()))
        .map_err(Into::into)
}

/// Marker types so `env_or` reads self-documenting at the call site.
struct ForwardVar;
trait EnvVar {
    fn key(&self) -> &'static str;
}
impl EnvVar for ForwardVar {
    fn key(&self) -> &'static str {
        "WEBHOOK_FORWARD_URL"
    }
}
fn env_or<V: EnvVar>(v: V, default: &str) -> String {
    std::env::var(v.key()).unwrap_or_else(|_| default.to_owned())
}

/// GET landing page: explains the sandbox story and prints a `curl` that works.
fn info_page(req: &Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    let forward_url = env_or(ForwardVar, DEFAULT_FORWARD_URL);

    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8200")
        .to_owned();

    // With no secret configured the receiver rejects every POST, so say that
    // instead of printing a curl that cannot work.
    let Some(secret) = signing_secret() else {
        let page = format!(
            "Sandboxed Webhook (not configured)\n\
             ==================================\n\n\
             WEBHOOK_SIGNING_SECRET is not set, so every POST is refused with 503\n\
             and nothing is forwarded. There is no default secret: one written in\n\
             the source would be public, and anyone could sign a request with it.\n\n\
             To finish setup, register a secret with Cosmonic Desktop and reference\n\
             it from the workload:\n\n\
             \x20 cosmonic secret set webhook-signing-secret\n\n\
             then apply manifests/workload.yaml, which maps that secret to\n\
             WEBHOOK_SIGNING_SECRET. Reload this page and it will print a signed\n\
             curl you can run.\n\n\
             Forward target : {forward_url}\n"
        );
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(page))
            .map_err(Into::into);
    };

    // Sign the sample body with the configured secret so the shown curl succeeds.
    let sig = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mut mac) => {
            mac.update(SAMPLE_BODY.as_bytes());
            hex::encode(mac.finalize().into_bytes())
        }
        Err(_) => String::new(),
    };

    let page = format!(
        "Sandboxed Webhook\n\
         =================\n\n\
         A webhook receiver that verifies an HMAC-SHA256 signature, then forwards\n\
         the payload to exactly one allow-listed host. Egress is deny-all: this\n\
         component can reach {forward_url}\n\
         and nothing else, because that host is the only entry in allowedHosts.\n\
         A compromised handler still cannot phone home.\n\n\
         Forward target : {forward_url}\n\
         Signature header: X-Hub-Signature-256: sha256=<hmac-sha256 of the raw body>\n\n\
         Try it (valid signature for the demo secret):\n\n\
         curl -sX POST http://{host}/ \\\n\
         \x20 -H 'content-type: application/json' \\\n\
         \x20 -H 'x-hub-signature-256: sha256={sig}' \\\n\
         \x20 -d '{SAMPLE_BODY}'\n\n\
         You get back the forward target and the downstream status. Now change one\n\
         byte of the body (so the signature no longer matches) and you get 401.\n\
         The request never leaves the sandbox.\n\n\
         To point it somewhere else: set WEBHOOK_FORWARD_URL and put the new host\n\
         in allowedHosts, or the forward is denied by the sandbox.\n"
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(page))
        .map_err(Into::into)
}
