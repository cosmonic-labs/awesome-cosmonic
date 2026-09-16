//! Request Bin — a disposable endpoint that records exactly what was sent to it.
//!
//! Point a webhook, a form, or a misbehaving integration at a bin URL and read
//! back the method, path, headers and body of every request it received. The
//! question it answers is the one you cannot answer from your own logs: *what
//! did they actually send me?*
//!
//! Requests are stored in the host's key-value store (`wasi:keyvalue`), so a
//! bin survives the component scaling to zero. The component declares NO
//! outbound network access: it can be reached, and it can write to its bucket,
//! and that is the whole of it. Its Launchpad card reads `OUTBOUND none`.
//!
//! Routes:
//!   GET    /                   the browser UI
//!   POST   /api/bins           create a bin; returns { id }
//!   ANY    /b/<id>             record a request into bin <id> (this is the URL you hand out)
//!   GET    /api/bins/<id>      the recorded requests, newest first
//!   DELETE /api/bins/<id>      forget the bin
//!   GET    /healthz            "ok"
//!
//! p2 rather than p3: the WASI 0.3 HTTP surface does not compose with an extra
//! `wasi:keyvalue` import, so this exports `wasi:http/incoming-handler@0.2.2`
//! and is served one-shot per request. The warm-pool knobs are inert here,
//! which costs nothing for a single store round-trip.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::io::streams::StreamError;
use bindings::wasi::keyvalue::store::{open, Bucket};

struct Component;

/// Bucket name handed to `open()`. The host chooses what backs it; Cosmonic
/// Desktop wires `wasi:keyvalue` to its filesystem-backed store.
const BACKEND: &str = "in_memory";
/// Requests kept per bin. A bin is a debugging scratchpad, not an archive, and
/// an unbounded list is a memory leak with a URL.
const MAX_REQUESTS: usize = 50;
/// Bytes of a single request body retained. Enough for any webhook payload
/// worth reading by eye; a 10 MB upload should not evict the other 49.
const MAX_BODY_KEPT: usize = 64 * 1024;
/// Total bytes read off the wire before giving up.
const MAX_BODY_READ: usize = 1024 * 1024;

type HttpResult = (u16, Vec<(String, Vec<u8>)>, Vec<u8>);

const PAGE: &str = include_str!("index.html");

fn json_headers() -> Vec<(String, Vec<u8>)> {
    vec![
        ("content-type".into(), b"application/json; charset=utf-8".to_vec()),
        ("cache-control".into(), b"no-store".to_vec()),
        ("x-content-type-options".into(), b"nosniff".to_vec()),
    ]
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn q(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

fn err_json(status: u16, msg: &str) -> HttpResult {
    (status, json_headers(), format!("{{\"error\":{}}}", q(msg)).into_bytes())
}

/// A URL-safe random id. Bin ids are unguessable on purpose: a bin URL is the
/// only thing protecting whatever someone posts into it, so it must not be
/// enumerable.
fn new_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyz23456789";
    let bytes = bindings::wasi::random::random::get_random_bytes(16);
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// Reject anything that is not one of our own ids before it reaches the store.
/// The id lands in a key, and an unvalidated path segment is how a traversal or
/// a key-collision bug gets in.
fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn now_millis() -> u64 {
    let now = bindings::wasi::clocks::wall_clock::now();
    now.seconds * 1000 + u64::from(now.nanoseconds) / 1_000_000
}

fn bucket() -> Result<Bucket, String> {
    open(BACKEND).map_err(|e| format!("could not open the key-value store: {e:?}"))
}

/// The store key for a bin.
///
/// Deliberately FLAT. The host's key-value store is filesystem-backed and maps
/// a key onto a path, so a key containing `/` looks for a subdirectory that was
/// never created and every call fails with "No such file or directory" — which
/// reads like a broken store rather than a bad key. Ids are validated to
/// [a-z0-9] before they reach here, so the prefix cannot collide.
fn key_for(id: &str) -> String {
    format!("bin_{id}")
}

/// Is this body worth showing as text? A webhook payload is JSON or form data;
/// a binary upload rendered as mojibake helps nobody, so it is described
/// instead.
fn body_repr(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.chars().any(|c| (c as u32) < 0x09) => (s.to_string(), true),
        _ => (format!("<{} bytes of binary data>", bytes.len()), false),
    }
}

/// One recorded request, already JSON-encoded. The store holds a JSON array of
/// these; keeping them pre-encoded avoids parsing on read, and this component
/// never needs to inspect a past request's fields.
fn record_json(
    method: &str,
    path: &str,
    headers: &[(String, Vec<u8>)],
    body: &[u8],
) -> String {
    /* One JSON array per header NAME, not one key per occurrence. A header can
       legitimately repeat (Set-Cookie, X-Forwarded-For, Via), and emitting the
       name twice produces a duplicate key: JSON.parse keeps the last, so the UI
       silently showed one of them. For a tool whose promise is recording
       exactly what was sent, quietly dropping a repeated header is the one
       failure it cannot afford. Insertion order is preserved. */
    let mut order: Vec<&str> = Vec::new();
    let mut grouped: Vec<(&str, Vec<String>)> = Vec::new();
    for (k, v) in headers {
        let val = q(&String::from_utf8_lossy(v));
        match order.iter().position(|n| n == k) {
            Some(i) => grouped[i].1.push(val),
            None => {
                order.push(k);
                grouped.push((k, vec![val]));
            }
        }
    }
    let hdrs = grouped
        .iter()
        .map(|(k, vals)| format!("{}:[{}]", q(k), vals.join(",")))
        .collect::<Vec<_>>()
        .join(",");
    // Cut on a character boundary. Slicing at a fixed byte offset can land in
    // the middle of a multi-byte character, and the resulting invalid UTF-8 was
    // then reported as "binary data" for a body that was perfectly good text.
    let mut cut = body.len().min(MAX_BODY_KEPT);
    while cut > 0 && cut < body.len() && (body[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    let kept = &body[..cut];
    let (repr, is_text) = body_repr(kept);
    format!(
        "{{\"receivedAt\":{},\"method\":{},\"path\":{},\"headers\":{{{}}},\
         \"body\":{},\"bodyIsText\":{},\"bodyBytes\":{},\"bodyTruncated\":{}}}",
        now_millis(),
        q(method),
        q(path),
        hdrs,
        q(&repr),
        is_text,
        body.len(),
        body.len() > MAX_BODY_KEPT
    )
}

/// Append to the bin's JSON array, newest first, capped.
///
/// The array is stored as text and spliced rather than parsed: the only edit
/// this component makes is "push one at the front, drop from the end", and a
/// JSON parser would be a dependency bought for nothing.
fn append(b: &Bucket, id: &str, record: &str) -> Result<(), String> {
    let key = key_for(id);
    let existing = b
        .get(&key)
        .map_err(|e| format!("store read failed: {e:?}"))?
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&existing);
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']').trim();

    let mut parts: Vec<&str> = Vec::new();
    parts.push(record);
    if !inner.is_empty() {
        // Split on the boundary between top-level objects. Records are written
        // by this component only, so `},{` appears between them and nowhere
        // else at depth 0 — but a body containing that literal would fool a
        // naive split, so count braces outside strings.
        for piece in split_objects(inner) {
            if parts.len() >= MAX_REQUESTS {
                break;
            }
            parts.push(piece);
        }
    }
    let joined = format!("[{}]", parts.join(","));
    b.set(&key, joined.as_bytes())
        .map_err(|e| format!("store write failed: {e:?}"))
}

/// Split a `{..},{..}` run into its top-level objects, respecting strings and
/// escapes so a `},{` inside a recorded body cannot split a record in half.
fn split_objects(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start, mut in_str, mut esc) = (0i32, 0usize, false, false);
    for (i, c) in s.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    out
}

fn read_body(request: IncomingRequest) -> Result<Vec<u8>, String> {
    let body = request
        .consume()
        .map_err(|_| "failed to consume request body".to_string())?;
    let stream = body
        .stream()
        .map_err(|_| "failed to get body stream".to_string())?;
    let mut data = Vec::new();
    loop {
        match stream.blocking_read(65536) {
            Ok(chunk) => {
                data.extend_from_slice(&chunk);
                if data.len() > MAX_BODY_READ {
                    data.truncate(MAX_BODY_READ);
                    break;
                }
            }
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("read failed: {e:?}")),
        }
    }
    drop(stream);
    IncomingBody::finish(body);
    Ok(data)
}

fn method_name(m: &Method) -> String {
    match m {
        Method::Get => "GET".into(),
        Method::Post => "POST".into(),
        Method::Put => "PUT".into(),
        Method::Delete => "DELETE".into(),
        Method::Patch => "PATCH".into(),
        Method::Head => "HEAD".into(),
        Method::Options => "OPTIONS".into(),
        Method::Trace => "TRACE".into(),
        Method::Connect => "CONNECT".into(),
        Method::Other(s) => s.clone(),
    }
}

fn route(request: IncomingRequest) -> HttpResult {
    let method = method_name(&request.method());
    let path_and_query = request.path_with_query().unwrap_or_default();
    let path = path_and_query.split('?').next().unwrap_or("").to_string();
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let headers: Vec<(String, Vec<u8>)> = request.headers().entries();

    let b = match bucket() {
        Ok(b) => b,
        Err(e) => return err_json(500, &e),
    };

    match (method.as_str(), segments.as_slice()) {
        ("GET", []) => (
            200,
            vec![("content-type".into(), b"text/html; charset=utf-8".to_vec())],
            PAGE.as_bytes().to_vec(),
        ),
        ("GET", [s]) if s == "healthz" => (
            200,
            vec![("content-type".into(), b"text/plain; charset=utf-8".to_vec())],
            b"ok\n".to_vec(),
        ),
        ("POST", [a, bn]) if a == "api" && bn == "bins" => {
            let id = new_id();
            match b.set(&key_for(&id), b"[]") {
                Ok(()) => (200, json_headers(), format!("{{\"id\":{}}}", q(&id)).into_bytes()),
                Err(e) => err_json(500, &format!("could not create the bin: {e:?}")),
            }
        }
        ("GET", [a, bn, id]) if a == "api" && bn == "bins" => {
            if !valid_id(id) {
                return err_json(400, "not a bin id");
            }
            match b.get(&key_for(id)) {
                Ok(Some(v)) => (200, json_headers(), v),
                Ok(None) => err_json(404, "no such bin"),
                Err(e) => err_json(500, &format!("store read failed: {e:?}")),
            }
        }
        ("DELETE", [a, bn, id]) if a == "api" && bn == "bins" => {
            if !valid_id(id) {
                return err_json(400, "not a bin id");
            }
            match b.delete(&key_for(id)) {
                Ok(()) => (200, json_headers(), b"{\"deleted\":true}".to_vec()),
                Err(e) => err_json(500, &format!("store delete failed: {e:?}")),
            }
        }
        // The catch-all: anything at /b/<id> is recorded, whatever the method.
        (_, [bseg, id]) if bseg == "b" => {
            if !valid_id(id) {
                return err_json(404, "no such bin");
            }
            match b.exists(&key_for(id)) {
                Ok(true) => {}
                Ok(false) => return err_json(404, "no such bin"),
                Err(e) => return err_json(500, &format!("store read failed: {e:?}")),
            }
            let body = match read_body(request) {
                Ok(v) => v,
                Err(e) => return err_json(400, &e),
            };
            let record = record_json(&method, &path_and_query, &headers, &body);
            match append(&b, id, &record) {
                Ok(()) => (
                    200,
                    json_headers(),
                    b"{\"recorded\":true}".to_vec(),
                ),
                Err(e) => err_json(500, &e),
            }
        }
        _ => err_json(404, "not found"),
    }
}

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let (status, headers, body) = route(request);
        if let Err(e) = write_response(response_out, status, &headers, &body) {
            // The response outparam is already consumed by this point on the
            // happy path; there is nowhere left to report to but the log.
            eprintln!("request-bin: {e}");
        }
    }
}

fn write_response(
    response_out: ResponseOutparam,
    status: u16,
    headers: &[(String, Vec<u8>)],
    body: &[u8],
) -> Result<(), String> {
    let fields = Fields::from_list(headers).map_err(|e| format!("invalid headers: {e:?}"))?;
    let response = OutgoingResponse::new(fields);
    response
        .set_status_code(status)
        .map_err(|()| format!("invalid status code: {status}"))?;
    let out_body = response
        .body()
        .map_err(|()| "failed to take response body".to_string())?;
    ResponseOutparam::set(response_out, Ok(response));
    let stream = out_body
        .write()
        .map_err(|()| "failed to open body stream".to_string())?;
    // The host caps a single blocking-write-and-flush at 4096 bytes (a larger
    // call silently drops the write), so chunk it.
    for chunk in body.chunks(4096) {
        stream
            .blocking_write_and_flush(chunk)
            .map_err(|e| format!("write failed: {e:?}"))?;
    }
    drop(stream);
    OutgoingBody::finish(out_body, None).map_err(|e| format!("finish failed: {e:?}"))?;
    Ok(())
}

#[allow(unsafe_code)] // bindings::export! emits unsafe FFI shims
mod export {
    use super::{bindings, Component};
    bindings::export!(Component with_types_in bindings);
}
