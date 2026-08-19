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
//! - `GET  /` renders an info page (and a ready-to-run `curl` with a valid
//!   signature for the configured secret), so the entry shows something the
//!   moment it launches.
//! - `POST /` verifies `X-Hub-Signature-256: sha256=<hex>` over the raw body,
//!   then forwards the body to `WEBHOOK_FORWARD_URL`.
//!
//! Config (workload environment):
//! - `WEBHOOK_SIGNING_SECRET`: HMAC key. Defaults to a well-known demo secret
//!   so the example runs one-click; move it to a Cosmonic secret for real use.
//! - `WEBHOOK_FORWARD_URL`: the single downstream URL. Its host MUST be listed
//!   in the workload's `allowedHosts`, or the forward is denied by the sandbox.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use wstd::http::{Body, Client, Method, Request, Response, StatusCode};

type HmacSha256 = Hmac<Sha256>;

/// Well-known demo secret so the example works the instant it launches. Replace
/// it (via `WEBHOOK_SIGNING_SECRET`, ideally a Cosmonic secret) for real use.
const DEFAULT_SECRET: &str = "cosmonic-demo-secret";
/// Default downstream. Its host is the single entry in `allowedHosts`.
const DEFAULT_FORWARD_URL: &str = "https://postman-echo.com/post";
const SIG_HEADER: &str = "x-hub-signature-256";
/// Sample body the info page signs, so the shown `curl` actually succeeds.
const SAMPLE_BODY: &str = r#"{"event":"ping","from":"cosmonic"}"#;

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
    let secret = env_or(SecretVar, DEFAULT_SECRET);
    let forward_url = env_or(ForwardVar, DEFAULT_FORWARD_URL);

    let (parts, mut body) = req.into_parts();

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
                &Outcome::error("WEBHOOK_FORWARD_URL is not a valid URL"),
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
struct SecretVar;
struct ForwardVar;
trait EnvVar {
    fn key(&self) -> &'static str;
}
impl EnvVar for SecretVar {
    fn key(&self) -> &'static str {
        "WEBHOOK_SIGNING_SECRET"
    }
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
    let secret = env_or(SecretVar, DEFAULT_SECRET);
    let forward_url = env_or(ForwardVar, DEFAULT_FORWARD_URL);

    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8200")
        .to_owned();

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
         To make it yours: set WEBHOOK_SIGNING_SECRET (ideally a Cosmonic secret)\n\
         and WEBHOOK_FORWARD_URL, and put the new host in allowedHosts.\n"
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(page))
        .map_err(Into::into)
}
