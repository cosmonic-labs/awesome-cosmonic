//! Transport-agnostic request/response types.
//!
//! The wasm entry point (`lib.rs`, wasm32 only) translates the host's
//! `wasi:http` request into a [`Request`] and writes a [`Response`] back.
//! Keeping the dispatch layer in terms of these plain types is what lets the
//! whole registry be unit-tested on the host with no wasm runtime.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    #[default]
    Other,
}

impl Method {
    pub fn parse(s: &str) -> Method {
        match s.to_ascii_uppercase().as_str() {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "PATCH" => Method::Patch,
            "DELETE" => Method::Delete,
            _ => Method::Other,
        }
    }
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct Request {
    pub method_str: String,
    pub method: Method,
    /// Percent-decoded path, no query string. Always begins with '/'.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub content_range: Option<String>,
    /// Host header — used by the browse UI to render absolute pull commands.
    pub host: Option<String>,
    pub body: Vec<u8>,
}

impl Request {
    /// Build a request from raw parts, percent-decoding the path and query.
    pub fn new(method: &str, raw_path: &str, raw_query: Option<&str>, body: Vec<u8>) -> Self {
        Request {
            method_str: method.to_string(),
            method: Method::parse(method),
            path: percent_decode(raw_path),
            query: raw_query.map(parse_query).unwrap_or_default(),
            content_type: None,
            content_range: None,
            host: None,
            body,
        }
    }

    pub fn query_get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Default)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("content-type".to_string(), content_type.to_string()));
        self.body = body;
        self
    }

    pub fn json(status: u16, value: serde_json::Value) -> Self {
        let body = serde_json::to_vec(&value).unwrap_or_default();
        Response::new(status).with_body("application/json", body)
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Response::new(status).with_body("text/plain; charset=utf-8", body.into().into_bytes())
    }

    pub fn html(status: u16, body: String) -> Self {
        Response::new(status).with_body("text/html; charset=utf-8", body.into_bytes())
    }

    pub fn error(code: crate::error::ErrorCode, message: &str) -> Self {
        Response::new(code.status()).with_body("application/json", code.body(message))
    }
}

/// Percent-decode a string (RFC 3986). Invalid escapes are passed through
/// literally. `+` is left as-is (path/query segments here don't use form
/// encoding).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_digest_in_query() {
        let r = Request::new(
            "PUT",
            "/v2/foo/blobs/uploads/abc",
            Some("digest=sha256%3Adeadbeef"),
            vec![],
        );
        assert_eq!(r.query_get("digest"), Some("sha256:deadbeef"));
        assert_eq!(r.method, Method::Put);
    }

    #[test]
    fn decodes_path() {
        let r = Request::new("GET", "/v2/foo%2Fbar/tags/list", None, vec![]);
        assert_eq!(r.path, "/v2/foo/bar/tags/list");
    }
}
