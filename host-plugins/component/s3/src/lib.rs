//! An S3 backend for `wasmcloud:blobstore`, as a wasmCloud host component
//! plugin.
//!
//! The plugin holds the endpoint, region, and credentials and signs every
//! request itself, so a workload names a container and an object and never sees
//! a secret. That, plus the `allowedHosts` policy bounding where those
//! credentials can be sent, is what the capability boundary buys here — S3 is
//! stateless request/response, so unlike the Kafka plugin next door there is no
//! long-lived session to protect.
//!
//! The interface is the standard `wasmcloud:blobstore`, not something bespoke:
//! a workload written against it gets S3 from this plugin, or the host's
//! built-in filesystem/NATS backend from no plugin at all, without changing.
//! Object bodies cross as `stream<u8>`, which is bridge-safe across a
//! plugin/workload store boundary where an arbitrary resource handle is not.

mod bindings {
    #![allow(unsafe_code)]
    wit_bindgen::generate!({ world: "s3-plugin", generate_all });
}

mod sigv4;

use bindings::exports::wasmcloud::blobstore::blobstore::Guest as BlobstoreGuest;
use bindings::exports::wasmcloud::blobstore::container::{
    Container as ContainerResource, ContainerMetadata, Guest as ContainerGuest, GuestContainer,
    ObjectMetadata,
};
use bindings::exports::wasmcloud::blobstore::types::{Error as BlobError, ObjectId};
use bindings::wasi::clocks::wall_clock;
use bindings::wasi::config::store;
use bindings::wasi::http::outgoing_handler;
use bindings::wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use bindings::wasi::io::streams::StreamError;
use bindings::wasi::logging::logging::{log, Level};

use sigv4::{encode_segment, CanonicalRequest, Credentials};

/// Base URL of the S3 API, e.g. `http://192.168.1.10:9100`. Required: no
/// default could be right for someone else's deployment.
const CFG_ENDPOINT: &str = "endpoint";
const CFG_ACCESS_KEY: &str = "access-key";
const CFG_SECRET_KEY: &str = "secret-key";
/// Signing region. Any S3-compatible server accepts some region string; the
/// signature has to be computed with the one it expects.
const CFG_REGION: &str = "region";

const DEFAULT_REGION: &str = "us-east-1";
const LOG_CONTEXT: &str = "cosmonic-s3";

struct Component;

/// A handle to one bucket. Holds only the name: every operation re-reads the
/// endpoint config, which is a cheap host call, and S3 has no per-container
/// session worth keeping.
struct S3Container {
    name: String,
}

struct Endpoint {
    scheme: Scheme,
    /// `host` or `host:port` — the authority, and the value signed as `host`.
    authority: String,
    access_key: String,
    secret_key: String,
    region: String,
}

fn config(key: &str) -> Result<Option<String>, BlobError> {
    store::get(key).map_err(|e| BlobError::Other(format!("could not read config '{key}': {e:?}")))
}

fn required(key: &str) -> Result<String, BlobError> {
    config(key)?
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| BlobError::Other(format!("config key '{key}' is unset or empty")))
}

impl Endpoint {
    fn load() -> Result<Self, BlobError> {
        let raw = required(CFG_ENDPOINT)?;
        let (scheme, rest) = match raw.split_once("://") {
            Some(("http", rest)) => (Scheme::Http, rest),
            Some(("https", rest)) => (Scheme::Https, rest),
            Some((other, _)) => {
                return Err(BlobError::Other(format!(
                    "config '{CFG_ENDPOINT}' has scheme '{other}'; expected http or https"
                )))
            }
            None => (Scheme::Http, raw.as_str()),
        };
        Ok(Self {
            scheme,
            authority: rest.trim_end_matches('/').to_owned(),
            access_key: required(CFG_ACCESS_KEY)?,
            secret_key: required(CFG_SECRET_KEY)?,
            region: config(CFG_REGION)?.unwrap_or_else(|| DEFAULT_REGION.to_owned()),
        })
    }
}

/// Send one signed request, returning `(status, body)`.
///
/// Path-style addressing (`/bucket/key`), because a server reached by address
/// has no wildcard DNS to make `bucket.host` resolve.
/// Everything a signed request needs, so the callers below can describe one
/// without repeating the signing and header dance.
struct Request<'a> {
    method: Method,
    bucket: &'a str,
    key: &'a str,
    query: &'a str,
    /// Body bytes, for a request whose payload is known and signed. A streaming
    /// upload sends this empty and writes to the returned body stream instead.
    body: Vec<u8>,
    headers: &'a [(&'a str, String)],
    /// Payload hash to sign. `None` signs over `body`; a streaming upload
    /// overrides it with a sentinel, since the bytes are not known yet.
    payload_override: Option<&'a str>,
    /// Bytes the caller will write to the body stream, when that differs from
    /// `body.len()` — the `content-length` an `aws-chunked` upload must declare.
    content_length_override: Option<u64>,
}

impl<'a> Request<'a> {
    fn new(method: Method, bucket: &'a str, key: &'a str) -> Self {
        Self {
            method,
            bucket,
            key,
            query: "",
            body: Vec::new(),
            headers: &[],
            payload_override: None,
            content_length_override: None,
        }
    }
    fn query(mut self, query: &'a str) -> Self {
        self.query = query;
        self
    }
    fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
    fn headers(mut self, headers: &'a [(&'a str, String)]) -> Self {
        self.headers = headers;
        self
    }
}

/// Build, sign, and dispatch a request, returning the live response.
///
/// Stops before reading the body so a caller can either collect it or pump it
/// somewhere — the difference between an object that fits in memory and one
/// that must not be held there at all.
fn dispatch(req: &Request<'_>) -> Result<bindings::wasi::http::types::IncomingResponse, BlobError> {
    let method = req.method.clone();
    let bucket = req.bucket;
    let key = req.key;
    let query = req.query;
    let body = req.body.clone();
    let extra_headers = req.headers;
    let endpoint = Endpoint::load()?;

    // Each key segment is encoded separately so a `/` inside a key stays a
    // separator — which is what makes `a/b/c.json` a nested key rather than one
    // oddly-named object.
    let encoded_key = key
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    let path = if encoded_key.is_empty() {
        format!("/{}", encode_segment(bucket))
    } else {
        format!("/{}/{encoded_key}", encode_segment(bucket))
    };

    let signed = sigv4::sign(
        &CanonicalRequest {
            method: method_str(&method),
            path: &path,
            query,
            host: &endpoint.authority,
            payload: &body,
            payload_hash_override: req.payload_override,
        },
        &Credentials {
            access_key: &endpoint.access_key,
            secret_key: &endpoint.secret_key,
            region: &endpoint.region,
        },
        wall_clock::now().seconds as i64,
    );

    let headers = Fields::new();
    let set = |name: &str, value: &str| {
        headers
            .append(name, value.as_bytes())
            .map_err(|e| BlobError::Other(format!("could not set header '{name}': {e:?}")))
    };
    set("authorization", &signed.authorization)?;
    set("x-amz-date", &signed.x_amz_date)?;
    set("x-amz-content-sha256", &signed.x_amz_content_sha256)?;
    // Explicit, because without it the transport falls back to chunked encoding
    // and S3 answers 411 MissingContentLength — it will not take a body whose
    // length it does not know up front.
    set(
        "content-length",
        &req.content_length_override
            .unwrap_or(body.len() as u64)
            .to_string(),
    )?;
    for (name, value) in extra_headers {
        set(name, value)?;
    }
    // `host` is signed but not set here: p2 derives it from the authority
    // below, and setting both risks them disagreeing, which reads as a
    // signature mismatch rather than a duplicated header.

    let request = OutgoingRequest::new(headers);
    let _ = request.set_method(&method);
    let _ = request.set_scheme(Some(&endpoint.scheme));
    let _ = request.set_authority(Some(&endpoint.authority));
    let _ = request.set_path_with_query(Some(&if query.is_empty() {
        path.clone()
    } else {
        format!("{path}?{query}")
    }));

    // The body handle has to be taken before the request is handed over, but the
    // bytes must be written *after* — the transport only drains the outgoing
    // body once the request is in flight. Writing first works for a small body
    // that fits the internal buffer and deadlocks for anything larger, because
    // nothing is reading the other end yet.
    let out_body = request
        .body()
        .map_err(|()| BlobError::Other("request body already taken".to_owned()))?;

    let future_response = outgoing_handler::handle(request, None).map_err(|e| {
        // An egress denial and a dead server both land here, and the difference
        // is a policy fix versus an outage — so it is logged with the authority
        // that was attempted, which the workload's `error` does not carry.
        log(
            Level::Error,
            LOG_CONTEXT,
            &format!("request to {} refused: {e:?}", endpoint.authority),
        );
        BlobError::StoreUnavailable
    })?;

    if !body.is_empty() {
        let stream = out_body
            .write()
            .map_err(|()| BlobError::Other("request body stream already taken".to_owned()))?;
        // 4096 is the most `blocking-write-and-flush` accepts in one call.
        for chunk in body.chunks(4096) {
            stream.blocking_write_and_flush(chunk).map_err(|e| {
                log(Level::Error, LOG_CONTEXT, &format!("writing body: {e:?}"));
                BlobError::StoreUnavailable
            })?;
        }
        // The stream borrows the body, so it goes before the body is finished.
        drop(stream);
    }
    OutgoingBody::finish(out_body, None)
        .map_err(|e| BlobError::Other(format!("finishing request body: {e:?}")))?;

    future_response.subscribe().block();
    let response = future_response
        .get()
        .ok_or_else(|| BlobError::Other("response future resolved to nothing".to_owned()))?
        .map_err(|()| BlobError::Other("response already taken".to_owned()))?
        .map_err(|e| {
            log(
                Level::Error,
                LOG_CONTEXT,
                &format!("{}: {e:?}", endpoint.authority),
            );
            BlobError::StoreUnavailable
        })?;

    Ok(response)
}

/// Read a response body whole. For control-plane answers — listings, error
/// documents, multipart bookkeeping — which are small by construction.
fn collect(
    response: bindings::wasi::http::types::IncomingResponse,
) -> Result<(u16, Vec<u8>), BlobError> {
    let status = response.status();
    let incoming = response
        .consume()
        .map_err(|()| BlobError::Other("response body already consumed".to_owned()))?;
    let stream = incoming
        .stream()
        .map_err(|()| BlobError::Other("response body stream already taken".to_owned()))?;
    let mut out = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(_)) => return Err(BlobError::StoreUnavailable),
        }
    }
    Ok((status, out))
}

/// One response header's first value, as a string.
fn header(response: &bindings::wasi::http::types::IncomingResponse, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .into_iter()
        .next()
        .and_then(|v| String::from_utf8(v).ok())
}

/// Build, send, and collect — the shape every non-streaming operation wants.
fn send(
    method: Method,
    bucket: &str,
    key: &str,
    query: &str,
    body: Vec<u8>,
    extra_headers: &[(&str, String)],
) -> Result<(u16, Vec<u8>), BlobError> {
    let req = Request::new(method, bucket, key)
        .query(query)
        .body(body)
        .headers(extra_headers);
    collect(dispatch(&req)?)
}

fn method_str(method: &Method) -> &'static str {
    match method {
        Method::Get => "GET",
        Method::Put => "PUT",
        Method::Delete => "DELETE",
        Method::Head => "HEAD",
        Method::Post => "POST",
        _ => "GET",
    }
}

/// Map a non-2xx status onto the interface's named cases. The body is an S3 XML
/// error document whose `<Code>` is more specific than the status, so it is
/// preferred where present.
fn status_error(status: u16, body: &[u8]) -> BlobError {
    let text = String::from_utf8_lossy(body);
    let code = between(&text, "<Code>", "</Code>").unwrap_or_default();
    match (status, code.as_str()) {
        (_, "NoSuchBucket") => BlobError::NoSuchContainer,
        (_, "NoSuchKey") => BlobError::NoSuchObject,
        (_, "BucketAlreadyOwnedByYou") | (_, "BucketAlreadyExists") => {
            BlobError::ContainerAlreadyExists
        }
        (403, _) => BlobError::AccessDenied,
        (404, _) => BlobError::NoSuchObject,
        (408, _) | (504, _) => BlobError::Timeout,
        (429, _) | (507, _) => BlobError::QuotaExceeded,
        (500..=599, _) => BlobError::StoreUnavailable,
        _ => BlobError::Other(format!("HTTP {status} {code}")),
    }
}

fn ok_or_status(status: u16, body: Vec<u8>) -> Result<Vec<u8>, BlobError> {
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(status_error(status, &body))
    }
}

/// The text between the first `open` and the next `close` after it. Enough XML
/// for S3's flat, machine-generated listing and error documents; a real parser
/// would be the right call the moment this needs attributes or namespaces.
fn between(haystack: &str, open: &str, close: &str) -> Option<String> {
    let start = haystack.find(open)? + open.len();
    let end = haystack[start..].find(close)? + start;
    Some(haystack[start..end].to_owned())
}

/// The object keys in one `ListObjectsV2` page.
fn parse_keys(body: &str) -> Vec<String> {
    body.split("<Contents>")
        .skip(1)
        .filter_map(|chunk| between(chunk, "<Key>", "</Key>"))
        .collect()
}

/// The token for the next page, or `None` when this was the last.
///
/// Keyed off `IsTruncated` rather than the token's presence: a server that
/// reports truncation without a token is broken, and looping on it forever
/// would be worse than stopping.
fn next_page_token(body: &str) -> Option<String> {
    if between(body, "<IsTruncated>", "</IsTruncated>").as_deref() != Some("true") {
        return None;
    }
    between(body, "<NextContinuationToken>", "</NextContinuationToken>")
}

/// The query for one listing page. Sorted by key, because that is the order
/// SigV4 canonicalises — `continuation-token` before `list-type`, or the
/// signature does not match and the server answers 403.
fn list_query(token: Option<&str>) -> String {
    match token {
        Some(t) => format!("continuation-token={}&list-type=2", encode_segment(t)),
        None => "list-type=2".to_string(),
    }
}

/// Every object key in a container, following continuation tokens.
///
/// Paginated, because S3 caps a listing at 1000 keys and reports the rest as
/// truncated. Stopping at the first page would silently under-report — and
/// `clear` and `delete-container` are built on this, so a truncated listing
/// would mean reporting success having deleted a fraction of the container.
fn list_keys(bucket: &str) -> Result<Vec<String>, BlobError> {
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let query = list_query(token.as_deref());
        let (status, body) = send(Method::Get, bucket, "", &query, Vec::new(), &[])?;
        let body = ok_or_status(status, body)?;
        let text = String::from_utf8_lossy(&body);
        keys.extend(parse_keys(&text));
        match next_page_token(&text) {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    Ok(keys)
}

/// Abort an in-flight multipart upload unless it was explicitly completed.
///
/// S3 keeps the parts of an abandoned upload — and bills for them — until
/// something aborts it, so every early return from [`write_multipart`] has to
/// clean up. A guard rather than an abort at each `?` so a path added later
/// cannot forget.
///
/// This does not cover a hard trap: a wasm trap runs no destructors, and the
/// plugin's store is rebuilt from scratch. A bucket lifecycle rule expiring
/// incomplete uploads is the only backstop for that case.
struct AbortOnDrop<'a> {
    bucket: &'a str,
    key: &'a str,
    upload_id: &'a str,
    armed: bool,
}

impl AbortOnDrop<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let query = format!("uploadId={}", encode_segment(self.upload_id));
        // Best effort: this runs while unwinding an error, and a failure here
        // would only mask the error that caused it.
        if send(
            Method::Delete,
            self.bucket,
            self.key,
            &query,
            Vec::new(),
            &[],
        )
        .is_err()
        {
            log(
                Level::Warn,
                LOG_CONTEXT,
                &format!(
                    "could not abort multipart upload {}/{}; parts may linger",
                    self.bucket, self.key
                ),
            );
        }
    }
}

/// The `CompleteMultipartUpload` body for a set of uploaded parts.
fn complete_xml(etags: &[(usize, String)]) -> String {
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (number, etag) in etags {
        xml.push_str(&format!(
            "<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>"
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");
    xml
}

/// Frame `data` as `aws-chunked`: each chunk length-prefixed in hex, then a
/// zero-length chunk and an empty trailer.
fn chunk_frame(data: &[u8], chunk_size: usize) -> Vec<u8> {
    let mut framed = Vec::with_capacity(data.len() + 64);
    for chunk in data.chunks(chunk_size.max(1)) {
        framed.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        framed.extend_from_slice(chunk);
        framed.extend_from_slice(b"\r\n");
    }
    framed.extend_from_slice(b"0\r\n\r\n");
    framed
}

/// How much of a response body to read per host call.
const READ_CHUNK: u64 = 256 * 1024;

/// Bytes buffered before a multipart part is flushed. S3 requires every part
/// except the last to be at least 5 MiB, so this is the floor plus headroom —
/// and it is also the plugin's peak memory for an upload of any size.
const PART_SIZE: usize = 8 * 1024 * 1024;

/// How far a chunked upload will buffer while trying to learn the object's
/// length before giving up and telling the caller to use multipart.
const CHUNK_PROBE: usize = 64 * 1024 * 1024;

/// Bytes buffered before an `aws-chunked` frame is flushed. Unconstrained by
/// S3, so it is small: this is pure streaming, and the buffer only amortises
/// per-write overhead.
const CHUNK_SIZE: usize = 256 * 1024;

/// Sentinel that replaces the payload hash for an unsigned streaming upload.
/// Signing over the body is impossible when the body does not exist yet.
const STREAMING_UNSIGNED: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

/// `write-strategy`, or `write-strategy.<container>` to override one container.
const CFG_WRITE_STRATEGY: &str = "write-strategy";

/// How an object's bytes get to the server.
///
/// Both streaming strategies hold a bounded buffer rather than the object, so
/// either can write more than the plugin could ever hold — which on `wasm32`
/// means more than 4 GiB of address space would allow. They differ in what they
/// need to know up front, and that is the interesting trade:
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WriteStrategy {
    /// `CreateMultipartUpload` → N × `UploadPart` → `CompleteMultipartUpload`.
    /// Needs no length in advance, because each part carries its own — the only
    /// option when the size is genuinely unknown until the stream ends. Costs
    /// a 5 MiB-minimum part buffer and two extra round trips.
    Multipart,
    /// One `PUT` with `Content-Encoding: aws-chunked` and an unsigned-payload
    /// sentinel, framing the body as length-prefixed chunks. One round trip and
    /// a small buffer — but it must declare `x-amz-decoded-content-length`, so
    /// the total size has to be known before the first byte is sent. Streaming
    /// does not escape that requirement; it only escapes buffering.
    Chunked,
    /// Collect and `PUT`. Simplest, and bounded by memory — kept for small
    /// objects and as the baseline the other two are measured against.
    Buffered,
}

impl WriteStrategy {
    fn parse(raw: &str) -> Result<Self, BlobError> {
        match raw.trim() {
            "multipart" => Ok(Self::Multipart),
            "chunked" | "streaming" => Ok(Self::Chunked),
            "buffered" => Ok(Self::Buffered),
            other => Err(BlobError::Other(format!(
                "'{CFG_WRITE_STRATEGY}' is '{other}'; expected multipart, chunked, or buffered"
            ))),
        }
    }

    /// Per-container first, then the plugin-wide default, then multipart —
    /// which is the safe default because it is the one that works without
    /// knowing the size.
    fn for_container(container: &str) -> Result<Self, BlobError> {
        if let Some(raw) = config(&format!("{CFG_WRITE_STRATEGY}.{container}"))? {
            return Self::parse(&raw);
        }
        match config(CFG_WRITE_STRATEGY)? {
            Some(raw) => Self::parse(&raw),
            None => Ok(Self::Multipart),
        }
    }
}

/// Read a stream to its end. Only for bodies already known to be small.
async fn drain(data: wit_bindgen::StreamReader<u8>) -> Vec<u8> {
    let mut data = data;
    let mut out = Vec::new();
    loop {
        let (status, chunk) = data.read(Vec::with_capacity(64 * 1024)).await;
        out.extend_from_slice(&chunk);
        if matches!(status, wit_bindgen::StreamResult::Dropped) {
            break;
        }
    }
    out
}

/// Fill `buf` up to `want` bytes, returning false once the source is done.
async fn fill(data: &mut wit_bindgen::StreamReader<u8>, buf: &mut Vec<u8>, want: usize) -> bool {
    while buf.len() < want {
        let (status, chunk) = data.read(Vec::with_capacity(64 * 1024)).await;
        buf.extend_from_slice(&chunk);
        if matches!(status, wit_bindgen::StreamResult::Dropped) {
            return false;
        }
    }
    true
}

/// Upload as a multipart series. Peak memory is one part.
async fn write_multipart(
    bucket: &str,
    key: &str,
    data: wit_bindgen::StreamReader<u8>,
) -> Result<(), BlobError> {
    let (status, body) = send(Method::Post, bucket, key, "uploads=", Vec::new(), &[])?;
    let body = ok_or_status(status, body)?;
    let upload_id = between(&String::from_utf8_lossy(&body), "<UploadId>", "</UploadId>")
        .ok_or_else(|| BlobError::Other("no UploadId in CreateMultipartUpload".to_owned()))?;

    // Armed from here on: any early return below aborts the upload rather than
    // leaving its parts stored and billing.
    let mut guard = AbortOnDrop {
        bucket,
        key,
        upload_id: &upload_id,
        armed: true,
    };

    let mut data = data;
    let mut etags: Vec<(usize, String)> = Vec::new();
    let mut part_number = 1usize;
    let mut buf: Vec<u8> = Vec::with_capacity(PART_SIZE);

    loop {
        let more = fill(&mut data, &mut buf, PART_SIZE).await;
        // A zero-length object still needs one part, or Complete rejects it.
        if buf.is_empty() && !etags.is_empty() {
            break;
        }

        let part = std::mem::take(&mut buf);
        // Sorted by key, values encoded: the canonical form the signature is
        // computed over, and the same string sent on the wire.
        let query = format!(
            "partNumber={part_number}&uploadId={}",
            encode_segment(&upload_id)
        );
        let response = dispatch(
            &Request::new(Method::Put, bucket, key)
                .query(&query)
                .body(part),
        )?;
        let etag = header(&response, "etag");
        let (status, body) = collect(response)?;
        ok_or_status(status, body)?;
        let etag =
            etag.ok_or_else(|| BlobError::Other(format!("part {part_number} returned no ETag")))?;
        etags.push((part_number, etag));
        part_number += 1;
        buf = Vec::with_capacity(PART_SIZE);

        if !more {
            break;
        }
    }

    let query = format!("uploadId={}", encode_segment(&upload_id));
    let (status, body) = send(
        Method::Post,
        bucket,
        key,
        &query,
        complete_xml(&etags).into_bytes(),
        &[("content-type", "application/xml".to_string())],
    )?;
    let body = ok_or_status(status, body)?;
    // S3 can answer 200 and still fail, with the error inside the body — the
    // one place where checking the status is not enough.
    if String::from_utf8_lossy(&body).contains("<Error>") {
        return Err(status_error(status, &body));
    }

    // Completed, so there is nothing left to abort.
    guard.disarm();
    Ok(())
}

/// Upload as one `PUT` with an `aws-chunked` body.
///
/// Requires the total size up front (`x-amz-decoded-content-length`), so this
/// is only available where the length is already known. The stream is drained
/// once to learn it, which is why this path measures the object first — and why
/// it is *not* the default.
async fn write_chunked(
    bucket: &str,
    key: &str,
    data: wit_bindgen::StreamReader<u8>,
) -> Result<(), BlobError> {
    // The length has to be known before the first byte goes out, so the stream
    // is buffered until either it ends (length known) or the probe is full
    // (length unknowable without holding the whole object, which is the thing
    // this strategy exists to avoid).
    let mut data = data;
    let mut probe = Vec::with_capacity(CHUNK_PROBE);
    let ended = !fill(&mut data, &mut probe, CHUNK_PROBE).await;

    if !ended {
        return Err(BlobError::Other(format!(
            "chunked upload needs the object length up front \
             (x-amz-decoded-content-length), and this stream exceeds the {CHUNK_PROBE}-byte \
             probe without ending. Use the multipart strategy for a stream whose size is \
             not known in advance."
        )));
    }

    // The chunks are what make this streaming; the declared decoded length is
    // what makes it possible at all.
    let total = probe.len();
    let framed = chunk_frame(&probe, CHUNK_SIZE);

    let headers = [
        ("content-encoding", "aws-chunked".to_string()),
        ("x-amz-decoded-content-length", total.to_string()),
    ];
    let req = Request {
        method: Method::Put,
        bucket,
        key,
        query: "",
        body: framed,
        headers: &headers,
        // The signature covers the sentinel rather than the bytes, which is
        // what lets a body be sent that was not hashed in advance.
        payload_override: Some(STREAMING_UNSIGNED),
        content_length_override: None,
    };
    let (status, body) = collect(dispatch(&req)?)?;
    ok_or_status(status, body).map(|_| ())
}

impl ContainerGuest for Component {
    type Container = S3Container;
}

impl GuestContainer for S3Container {
    async fn name(&self) -> Result<String, BlobError> {
        Ok(self.name.clone())
    }

    /// S3 exposes no creation time for a bucket over the object API, so
    /// `created-at` is reported as 0 rather than invented.
    async fn info(&self) -> Result<ContainerMetadata, BlobError> {
        Ok(ContainerMetadata {
            name: self.name.clone(),
            created_at: 0,
        })
    }

    /// Read `[start, end]` inclusive. The bytes are handed back as a
    /// `stream<u8>`, which is what lets an object cross to the workload at all
    /// — an arbitrary resource handle could not make that trip.
    async fn get_data(
        &self,
        name: String,
        start: u64,
        end: u64,
    ) -> Result<wit_bindgen::StreamReader<u8>, BlobError> {
        let range =
            (start > 0 || end < u64::MAX).then(|| ("range", format!("bytes={start}-{end}")));
        let req = Request::new(Method::Get, &self.name, &name).headers(range.as_slice());
        let response = dispatch(&req)?;

        let status = response.status();
        if !(200..300).contains(&status) {
            // Small by construction: this is an S3 XML error document.
            let (_, body) = collect(response)?;
            return Err(status_error(status, &body));
        }

        let incoming = response
            .consume()
            .map_err(|()| BlobError::Other("response body already consumed".to_owned()))?;
        let body = incoming
            .stream()
            .map_err(|()| BlobError::Other("response body stream already taken".to_owned()))?;

        // Pump rather than collect: the response, its body, and the stream all
        // move into this task, which copies one chunk at a time into the
        // `stream<u8>` the caller holds. Memory stays at one chunk however
        // large the object is, and backpressure is automatic — if the caller
        // reads slowly, `write_all` blocks here and the TCP window closes.
        let (mut tx, rx) = bindings::wit_stream::new();
        wit_bindgen::spawn_local(async move {
            loop {
                match body.blocking_read(READ_CHUNK) {
                    Ok(chunk) => {
                        if !chunk.is_empty() {
                            tx.write_all(chunk).await;
                        }
                    }
                    Err(StreamError::Closed) => break,
                    Err(StreamError::LastOperationFailed(_)) => break,
                }
            }
            drop(tx);
            // Keep the response alive until the body is drained; dropping it
            // early would close the connection mid-transfer.
            drop(incoming);
            drop(response);
        });
        Ok(rx)
    }

    /// Upload by whichever strategy this container is configured for. Both
    /// hold at most a bounded buffer, however large the object is; see
    /// [`WriteStrategy`].
    async fn write_data(
        &self,
        name: String,
        data: wit_bindgen::StreamReader<u8>,
    ) -> Result<(), BlobError> {
        match WriteStrategy::for_container(&self.name)? {
            WriteStrategy::Multipart => write_multipart(&self.name, &name, data).await,
            WriteStrategy::Chunked => write_chunked(&self.name, &name, data).await,
            WriteStrategy::Buffered => {
                let body = drain(data).await;
                let (status, response) = send(Method::Put, &self.name, &name, "", body, &[])?;
                ok_or_status(status, response).map(|_| ())
            }
        }
    }

    async fn list_objects(&self) -> Result<wit_bindgen::StreamReader<String>, BlobError> {
        let names = list_keys(&self.name)?;
        let (mut tx, rx) = bindings::wit_stream::new();
        wit_bindgen::spawn_local(async move {
            tx.write_all(names).await;
            drop(tx);
        });
        Ok(rx)
    }

    /// S3 treats deleting an absent key as success, and so does this.
    async fn delete_object(&self, name: String) -> Result<(), BlobError> {
        let (status, body) = send(Method::Delete, &self.name, &name, "", Vec::new(), &[])?;
        if status == 404 {
            return Ok(());
        }
        ok_or_status(status, body).map(|_| ())
    }

    /// One request each rather than S3's batch delete, which requires an MD5 of
    /// the request body. Correct but chatty; worth revisiting for large sets.
    async fn delete_objects(&self, names: Vec<String>) -> Result<(), BlobError> {
        for name in names {
            self.delete_object(name).await?;
        }
        Ok(())
    }

    async fn has_object(&self, name: String) -> Result<bool, BlobError> {
        let (status, body) = send(Method::Head, &self.name, &name, "", Vec::new(), &[])?;
        match status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(status_error(status, &body)),
        }
    }

    /// Size comes from a prefixed listing rather than a `HEAD`, because this
    /// transport does not surface response headers.
    async fn object_info(&self, name: String) -> Result<ObjectMetadata, BlobError> {
        let (status, body) = send(
            Method::Get,
            &self.name,
            "",
            &format!("list-type=2&max-keys=1&prefix={}", encode_segment(&name)),
            Vec::new(),
            &[],
        )?;
        let body = ok_or_status(status, body)?;
        let text = String::from_utf8_lossy(&body);
        let entry = text
            .split("<Contents>")
            .skip(1)
            .find(|chunk| between(chunk, "<Key>", "</Key>").as_deref() == Some(name.as_str()))
            .ok_or(BlobError::NoSuchObject)?;

        Ok(ObjectMetadata {
            name: name.clone(),
            container: self.name.clone(),
            created_at: 0,
            size: between(entry, "<Size>", "</Size>")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        })
    }

    async fn clear(&self) -> Result<(), BlobError> {
        for name in list_keys(&self.name)? {
            self.delete_object(name).await?;
        }
        Ok(())
    }
}

impl BlobstoreGuest for Component {
    async fn create_container(name: String) -> Result<ContainerResource, BlobError> {
        let (status, body) = send(Method::Put, &name, "", "", Vec::new(), &[])?;
        ok_or_status(status, body)?;
        Ok(ContainerResource::new(S3Container { name }))
    }

    /// Existence is checked rather than assumed: handing back a handle to a
    /// bucket that is not there would turn one clear error into a confusing one
    /// on the first object operation.
    async fn get_container(name: String) -> Result<ContainerResource, BlobError> {
        let (status, body) = send(Method::Head, &name, "", "", Vec::new(), &[])?;
        match status {
            200..=299 => Ok(ContainerResource::new(S3Container { name })),
            404 => Err(BlobError::NoSuchContainer),
            _ => Err(status_error(status, &body)),
        }
    }

    /// S3 refuses to delete a non-empty bucket, so the objects go first — which
    /// is what "deletes a container and all objects within it" asks for.
    async fn delete_container(name: String) -> Result<(), BlobError> {
        for key in list_keys(&name)? {
            let (status, body) = send(Method::Delete, &name, &key, "", Vec::new(), &[])?;
            if status != 404 {
                ok_or_status(status, body)?;
            }
        }
        let (status, body) = send(Method::Delete, &name, "", "", Vec::new(), &[])?;
        if status == 404 {
            return Err(BlobError::NoSuchContainer);
        }
        ok_or_status(status, body).map(|_| ())
    }

    async fn container_exists(name: String) -> Result<bool, BlobError> {
        let (status, body) = send(Method::Head, &name, "", "", Vec::new(), &[])?;
        match status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(status_error(status, &body)),
        }
    }

    /// Server-side copy: the bytes never travel back through this plugin.
    async fn copy_object(src: ObjectId, dest: ObjectId) -> Result<(), BlobError> {
        let source = format!(
            "/{}/{}",
            encode_segment(&src.container),
            src.object
                .split('/')
                .map(encode_segment)
                .collect::<Vec<_>>()
                .join("/")
        );
        let (status, body) = send(
            Method::Put,
            &dest.container,
            &dest.object,
            "",
            Vec::new(),
            &[("x-amz-copy-source", source)],
        )?;
        ok_or_status(status, body).map(|_| ())
    }

    /// Copy then delete, because S3 has no move. Not atomic: a failure after
    /// the copy leaves both copies, which is the usual S3 rename caveat.
    async fn move_object(src: ObjectId, dest: ObjectId) -> Result<(), BlobError> {
        let source = src.clone();
        Self::copy_object(src, dest).await?;
        let (status, body) = send(
            Method::Delete,
            &source.container,
            &source.object,
            "",
            Vec::new(),
            &[],
        )?;
        if status == 404 {
            return Ok(());
        }
        ok_or_status(status, body).map(|_| ())
    }
}

mod export {
    #![allow(unsafe_code)]
    use super::{bindings, Component};
    bindings::export!(Component with_types_in bindings);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A truncated listing must be followed, not silently cut short. `clear`
    /// and `delete-container` are built on `list_keys`, so stopping at the
    /// first page would mean reporting success having deleted a fraction of a
    /// container.
    #[test]
    fn a_truncated_listing_yields_a_continuation_token() {
        let page = "<ListBucketResult><Contents><Key>a.txt</Key></Contents>\
                    <Contents><Key>b/c.txt</Key></Contents>\
                    <IsTruncated>true</IsTruncated>\
                    <NextContinuationToken>tok/123+abc</NextContinuationToken>\
                    </ListBucketResult>";
        assert_eq!(parse_keys(page), vec!["a.txt", "b/c.txt"]);
        assert_eq!(next_page_token(page).as_deref(), Some("tok/123+abc"));
    }

    /// The last page ends the loop even though S3 still echoes the request's
    /// own token back in it.
    #[test]
    fn a_final_listing_page_ends_the_walk() {
        let page = "<ListBucketResult><Contents><Key>only.txt</Key></Contents>\
                    <IsTruncated>false</IsTruncated>\
                    <ContinuationToken>previous</ContinuationToken></ListBucketResult>";
        assert_eq!(parse_keys(page), vec!["only.txt"]);
        assert_eq!(
            next_page_token(page),
            None,
            "IsTruncated decides, not the presence of a token"
        );
    }

    /// A server that claims truncation but sends no token would otherwise loop
    /// forever asking for the same page.
    #[test]
    fn truncation_without_a_token_stops_rather_than_looping() {
        let page = "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>";
        assert_eq!(next_page_token(page), None);
    }

    /// SigV4 canonicalises query parameters in sorted order, and the string
    /// signed has to be the string sent. `continuation-token` sorts before
    /// `list-type`; getting this backwards is a 403 that reads like a
    /// credentials problem.
    #[test]
    fn a_listing_query_is_in_canonical_order() {
        assert_eq!(list_query(None), "list-type=2");
        let paged = list_query(Some("abc+/=="));
        assert!(
            paged.starts_with("continuation-token="),
            "sorted before list-type: {paged}"
        );
        assert!(paged.ends_with("&list-type=2"), "{paged}");
        assert!(
            !paged.contains("abc+/=="),
            "the token's reserved characters must be encoded: {paged}"
        );
    }

    #[test]
    fn complete_xml_lists_every_part_in_order() {
        let xml = complete_xml(&[(1, "\"aaa\"".into()), (2, "\"bbb\"".into())]);
        assert_eq!(
            xml,
            "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>\"aaa\"</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>\"bbb\"</ETag></Part>\
             </CompleteMultipartUpload>"
        );
    }

    /// `aws-chunked` framing: hex length, CRLF, payload, CRLF, then a
    /// zero-length chunk. A malformed frame is rejected by the server as a
    /// signature or length error, which reads like anything but a framing bug.
    #[test]
    fn chunk_framing_is_length_prefixed_and_terminated() {
        let framed = chunk_frame(b"hello world", 5);
        assert_eq!(
            framed,
            b"5\r\nhello\r\n5\r\n worl\r\n1\r\nd\r\n0\r\n\r\n".to_vec(),
            "three chunks of 5, 5, 1 then the terminator"
        );

        // An empty body is still a well-formed stream: just the terminator.
        assert_eq!(chunk_frame(b"", 8), b"0\r\n\r\n".to_vec());
    }

    #[test]
    fn write_strategies_parse_and_reject_by_name() {
        assert!(matches!(
            WriteStrategy::parse("multipart"),
            Ok(WriteStrategy::Multipart)
        ));
        assert!(matches!(
            WriteStrategy::parse(" chunked "),
            Ok(WriteStrategy::Chunked)
        ));
        assert!(matches!(
            WriteStrategy::parse("streaming"),
            Ok(WriteStrategy::Chunked)
        ));
        assert!(matches!(
            WriteStrategy::parse("buffered"),
            Ok(WriteStrategy::Buffered)
        ));
        match WriteStrategy::parse("magic") {
            Err(BlobError::Other(msg)) => {
                assert!(
                    msg.contains("magic"),
                    "the error should name the value: {msg}"
                )
            }
            other => panic!("expected a named error, got {other:?}"),
        }
    }

    /// The status/code mapping is what a caller matches on, so a `NoSuchKey`
    /// must not arrive as a generic failure.
    #[test]
    fn s3_error_documents_map_to_named_cases() {
        let no_key = b"<Error><Code>NoSuchKey</Code></Error>";
        assert!(matches!(status_error(404, no_key), BlobError::NoSuchObject));

        let no_bucket = b"<Error><Code>NoSuchBucket</Code></Error>";
        assert!(matches!(
            status_error(404, no_bucket),
            BlobError::NoSuchContainer
        ));

        // A 403 with no parseable body is still access denied, not "other".
        assert!(matches!(status_error(403, b""), BlobError::AccessDenied));
        assert!(matches!(
            status_error(503, b""),
            BlobError::StoreUnavailable
        ));
    }
}
