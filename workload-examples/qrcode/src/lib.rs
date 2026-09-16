use std::io::Cursor;

use image::{ImageFormat, Luma};
use qrcode::{EcLevel, QrCode};
use serde::{Deserialize, Serialize};
use wstd::http::{Body, Request, Response, StatusCode};

static UI_HTML: &str = include_str!("../ui.html");

/// Upper bound on a request body. A QR code tops out well below this (a
/// version-40 code at the lowest error correction holds about 2,953 bytes), so
/// anything larger is a mistake or a probe, and reading it costs us memory we
/// do not need to spend.
const MAX_BODY_BYTES: usize = 8 * 1024;

/// Longest payload we will try to encode. The encoder itself enforces the real
/// limit, which varies with the content and the error-correction level, but
/// checking first lets us answer with a clear message rather than a failure
/// from inside the library.
const MAX_PAYLOAD_CHARS: usize = 2_000;

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    match router(req).await {
        Ok(resp) => Ok(resp),
        // Anything reaching here is a bug in this component rather than
        // something the caller did: every expected failure is answered with a
        // 4xx below. Log it with detail, and tell the caller only that it was
        // our fault.
        Err(e) => {
            eprintln!("qrcode: unhandled error: {e:?}");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong generating the code. Check the workload logs.",
            )
        }
    }
}

async fn router(req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    match (req.method().as_str(), req.uri().path()) {
        ("GET", "/") | ("HEAD", "/") => home().await,
        ("POST", "/qrcode") => qrcode(req).await,
        // A GET of /qrcode is the common mistake (it is the obvious thing to
        // try from a browser address bar), so name the fix rather than 404.
        (_, "/qrcode") => json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Send a POST to /qrcode with a JSON body: {\"payload\": \"...\"}",
        ),
        _ => json_error(StatusCode::NOT_FOUND, "Not found."),
    }
}

async fn home() -> Result<Response<Body>, wstd::http::Error> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html; charset=utf-8")
        // The page is regenerated with the component; never let a stale copy
        // outlive a redeploy.
        .header("Cache-Control", "no-store")
        .body(UI_HTML.into())
        .map_err(Into::into)
}

#[derive(Deserialize)]
struct QrRequest {
    payload: String,
}

async fn qrcode(mut req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    // The body is read in full before anything can inspect it, so a declared
    // length is the only chance to refuse one that is too large. A request
    // without the header (a chunked upload, say) would otherwise be buffered
    // whole and only rejected afterwards, which is not a limit at all.
    let Some(len) = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    else {
        return json_error(
            StatusCode::LENGTH_REQUIRED,
            "Send a Content-Length header; chunked bodies are not accepted.",
        );
    };
    if len > MAX_BODY_BYTES {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "That body is too large. Send at most 8 KiB.",
        );
    }

    // A malformed body is the caller's mistake, so answer 400 with the reason
    // rather than letting it fall through to the 500 handler.
    let parsed: Result<QrRequest, _> = req.body_mut().json().await;
    let Ok(js_req) = parsed else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Expected a JSON body of the form {\"payload\": \"...\"}.",
        );
    };

    let payload = js_req.payload.trim();
    if payload.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Enter some text to encode.",
        );
    }
    if payload.chars().count() > MAX_PAYLOAD_CHARS {
        return json_error(
            StatusCode::BAD_REQUEST,
            "That text is too long to fit in a QR code. Try 2,000 characters or fewer.",
        );
    }

    // Medium error correction: a code that still scans with a logo over it or a
    // crease through it, without the size penalty of the highest level.
    let code = match QrCode::with_error_correction_level(payload, EcLevel::M) {
        Ok(code) => code,
        Err(e) => {
            eprintln!(
                "qrcode: encoder rejected a {} char payload: {e}",
                payload.chars().count()
            );
            return json_error(
                StatusCode::BAD_REQUEST,
                "That text could not be encoded as a QR code. Try something shorter.",
            );
        }
    };

    // A quiet zone is part of the spec: without the light margin many scanners
    // will not find the code at all.
    let img = code
        .render::<Luma<u8>>()
        .min_dimensions(320, 320)
        .quiet_zone(true)
        .build();

    let mut body = vec![];
    img.write_to(&mut Cursor::new(&mut body), ImageFormat::Png)?;

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "image/png")
        .header("Cache-Control", "no-store")
        .body(body.into())
        .map_err(Into::into)
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// A machine-readable error the browser UI can render as text. Every non-2xx
/// answer uses this shape so the page never has to guess whether it received an
/// image or a failure.
///
/// Serialized rather than formatted: hand-escaping covers `\` and `"` but not
/// the control characters JSON forbids raw, so the moment a caller passes
/// anything dynamic the body would be invalid and the page would fall back to a
/// generic status line, losing the message this exists to deliver.
fn json_error(status: StatusCode, message: &str) -> Result<Response<Body>, wstd::http::Error> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-store")
        .body(Body::from_json(&ErrorBody { error: message })?)
        .map_err(Into::into)
}
