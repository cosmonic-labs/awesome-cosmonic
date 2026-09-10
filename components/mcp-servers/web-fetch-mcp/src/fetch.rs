//! URL fetcher: GETs an `http://` or `https://` URL over `wasi:http` via the
//! bridge's outbound client, bounded by the workload's `allowedHosts` egress
//! allowlist.
//!
//! The response body is capped at [`MAX_BODY_BYTES`] (truncated, with a
//! `truncated` flag) so a huge page can't blow up memory or an agent's
//! context. Binary content types are summarized rather than returned as bytes.
//! When the outbound call fails because the host isn't in the allowlist, the
//! error is phrased as a friendly, actionable message rather than raw
//! transport detail.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{json, Value};

use crate::bridge::outbound;

/// A descriptive User-Agent, so upstreams can see who is calling.
const USER_AGENT: &str = "web-fetch-mcp (Cosmonic Desktop example)";

/// Cap on the returned body (~100 KB). Anything beyond this is dropped and the
/// result is flagged `truncated`, so a huge page can't exhaust memory or an
/// agent's context window.
const MAX_BODY_BYTES: usize = 100 * 1024;

/// A fetch failure, rendered as a friendly MCP tool-level error.
pub enum FetchError {
    /// The request itself was malformed (bad URL, unsupported scheme).
    BadRequest(String),
    /// The outbound call couldn't reach the host — most often because the host
    /// is not in the workload's `allowedHosts` egress allowlist.
    Egress { host: String, detail: String },
    /// The exchange started but failed (timeout, oversized body, teardown).
    Transport(String),
}

impl FetchError {
    /// Renders the error as an MCP tool-level error result — the caller sees
    /// the message. (This is not a protocol error; the request was valid.)
    pub fn into_tool_result(self) -> CallToolResult {
        let text = match self {
            FetchError::BadRequest(detail) => detail,
            FetchError::Egress { host, detail } => format!(
                "Couldn't reach {host} — it may not be in this workload's egress allowlist \
                 (allowedHosts). Add it to the manifest to grant access. (details: {detail})"
            ),
            FetchError::Transport(detail) => {
                format!("The fetch didn't complete: {detail}.")
            }
        };
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

/// `fetch_url` implementation: GET a URL and return its (bounded) contents.
///
/// `format` is `"text"` (default) — HTML stripped to readable plain text — or
/// `"raw"` — the body returned unchanged (still truncated).
pub async fn fetch_url(url: &str, format: Option<&str>) -> Result<Value, FetchError> {
    let host = host_of(url)
        .ok_or_else(|| FetchError::BadRequest(format!("'{url}' is not a valid URL.")))?;

    // Only http/https; the outbound client speaks nothing else, and it keeps
    // the tool from being coaxed into file:// or other schemes.
    let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(FetchError::BadRequest(format!(
            "'{url}' must be an http:// or https:// URL."
        )));
    }

    let raw = matches!(format.map(str::trim), Some("raw"));

    let request = http::Request::get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "*/*")
        .body(bytes::Bytes::new())
        .map_err(|err| FetchError::BadRequest(format!("couldn't build request for {url}: {err}")))?;

    let response = outbound::fetch(request).await.map_err(|err| match err {
        // A host missing from allowedHosts surfaces here as a wasi:http error;
        // so do DNS/TLS failures. Point the reader at the allowlist, the most
        // common and most actionable cause on Cosmonic Desktop.
        outbound::Error::Wasi(detail) => FetchError::Egress {
            host: host.clone(),
            detail,
        },
        other => FetchError::Transport(other.to_string()),
    })?;

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();

    let body = response.into_body();
    let total_len = body.len();
    let truncated = total_len > MAX_BODY_BYTES;
    let slice = if truncated { &body[..MAX_BODY_BYTES] } else { &body[..] };

    let content = if is_binary(&content_type) {
        format!(
            "[binary content: {} ({} bytes){}] — this tool returns text, not raw bytes.",
            if content_type.is_empty() { "unknown type" } else { &content_type },
            total_len,
            if truncated { ", truncated" } else { "" }
        )
    } else {
        // Lossy UTF-8: the cut at MAX_BODY_BYTES may split a multi-byte char,
        // and upstreams aren't guaranteed to send valid UTF-8.
        let text = String::from_utf8_lossy(slice);
        if !raw && is_html(&content_type) {
            html_to_text(&text)
        } else {
            text.into_owned()
        }
    };

    Ok(json!({
        "url": url,
        "status": status,
        "content_type": content_type,
        "truncated": truncated,
        "content": content,
    }))
}

/// Extracts the host (no scheme, userinfo, port, or path) from a URL, for use
/// in error messages. Returns `None` if there's no host.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip any userinfo (`user:pass@host`) and port.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_owned())
}

/// True when the content type is non-textual (image, audio, video, fonts,
/// archives, generic binary). Text, JSON, XML, HTML, SVG, and JavaScript are
/// all treated as text.
fn is_binary(content_type: &str) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    if ct.is_empty() {
        // No content type: assume text and let UTF-8 lossy handle the rest.
        return false;
    }
    if ct.starts_with("text/") {
        return false;
    }
    let textish = [
        "json",
        "xml",
        "html",
        "javascript",
        "ecmascript",
        "x-www-form-urlencoded",
        "svg",
        "csv",
        "yaml",
        "+json",
        "+xml",
    ];
    if textish.iter().any(|marker| ct.contains(marker)) {
        return false;
    }
    // application/* and everything else without a text marker: treat as binary.
    true
}

/// True when the content type indicates HTML.
fn is_html(content_type: &str) -> bool {
    content_type.to_ascii_lowercase().contains("html")
}

/// Strips HTML tags to readable plain text: drops `<script>`/`<style>` bodies
/// and comments, removes remaining tags (inserting a space so words don't
/// merge), decodes a handful of common entities, and collapses whitespace.
///
/// Deliberately lightweight and dependency-free — a full HTML parser is
/// unnecessary for turning a page into readable text and heavier than this
/// example warrants.
fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    // `rest`/`lrest` stay byte-aligned: ASCII-lowercasing preserves length and
    // char boundaries, and every slice below is cut at an index returned by
    // `find` on an ASCII pattern.
    let mut rest = html;
    let mut lrest = lower.as_str();

    while let Some(pos) = rest.find('<') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        lrest = &lrest[pos..];

        let (open, close) = if lrest.starts_with("<script") {
            ("<script", "</script>")
        } else if lrest.starts_with("<style") {
            ("<style", "</style>")
        } else if lrest.starts_with("<!--") {
            ("<!--", "-->")
        } else {
            ("", "")
        };

        if !open.is_empty() {
            // Skip the whole element (or comment) up to and including its close.
            match lrest.find(close) {
                Some(end) => {
                    let adv = end + close.len();
                    rest = &rest[adv..];
                    lrest = &lrest[adv..];
                }
                None => {
                    rest = "";
                    lrest = "";
                }
            }
            out.push(' ');
            continue;
        }

        // A plain tag: drop through the matching '>'.
        match rest.find('>') {
            Some(end) => {
                rest = &rest[end + 1..];
                lrest = &lrest[end + 1..];
                out.push(' ');
            }
            None => {
                rest = "";
                lrest = "";
            }
        }
    }
    out.push_str(rest);

    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decodes the handful of HTML entities common in body text. Not exhaustive —
/// enough to keep readable text from showing raw `&amp;` and friends.
fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
}
