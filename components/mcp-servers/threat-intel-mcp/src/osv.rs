//! OSV (Open Source Vulnerabilities) API client over `wasi:http` via the
//! bridge's outbound client, bounded by the workload's `allowedHosts` egress
//! allowlist (the one host is `api.osv.dev`).
//!
//! The OSV API is public and needs no API key. Two operations back the two
//! tools: a `POST /v1/query` that matches a package (and optional version)
//! against the database, and a `GET /v1/vulns/{id}` that fetches one advisory
//! by its OSV id or alias. Both return the OSV vulnerability schema
//! (<https://ossf.github.io/osv-schema/>), which this module trims to the
//! fields useful to an agent.

use serde_json::{json, Value};

use crate::bridge::outbound;

/// The only host this client reaches (also the workload's `allowedHosts`).
const BASE_URL: &str = "https://api.osv.dev";

/// Descriptive User-Agent sent on every request.
const USER_AGENT: &str = "threat-intel-mcp (Cosmonic Desktop example)";

/// Cap on the `details` markdown returned by `get_vulnerability` (~4 KB), so a
/// long advisory write-up can't exhaust an agent's context window.
const MAX_DETAILS_BYTES: usize = 4 * 1024;

/// Max reference URLs surfaced per advisory.
const MAX_REFERENCES: usize = 8;

/// Max affected-version-range summaries surfaced per advisory in a lookup.
const MAX_RANGE_LINES: usize = 20;

/// An OSV client error, surfaced to the model as a friendly tool-level error.
pub enum OsvError {
    /// The vulnerability id does not exist (HTTP 404).
    NotFound(String),
    /// The request failed to build, couldn't reach the host, or the API
    /// returned another non-2xx status.
    Request(String),
}

impl OsvError {
    /// Renders the error as an MCP tool-level error result (not a protocol
    /// error — the request was valid, the upstream just said no).
    pub fn into_tool_result(self) -> rmcp::model::CallToolResult {
        use rmcp::model::{CallToolResult, ContentBlock};
        let text = match self {
            OsvError::NotFound(id) => format!(
                "No such vulnerability id: '{id}' is not in the OSV database. Check the \
                 identifier — OSV accepts an OSV id or an alias such as GHSA-…, CVE-…, \
                 RUSTSEC-…, PYSEC-…, or GO-…."
            ),
            OsvError::Request(detail) => format!("OSV request failed: {detail}"),
        };
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

/// Percent-encodes a URL path segment (unreserved set `A-Za-z0-9-_.~`); every
/// other byte becomes `%XX`. OSV ids are already URL-safe, but a caller could
/// pass anything.
fn encode_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Sends a prepared request, maps transport/status errors to friendly
/// [`OsvError`]s, and decodes a successful JSON body. `what` names the resource
/// for a 404 message.
async fn send(request: http::Request<bytes::Bytes>, what: &str) -> Result<Value, OsvError> {
    let response = outbound::fetch(request).await.map_err(|err| match err {
        // A host missing from allowedHosts (or DNS/TLS failure) surfaces here;
        // api.osv.dev is the only host this tool ever needs.
        outbound::Error::Wasi(detail) => OsvError::Request(format!(
            "couldn't reach api.osv.dev — it may not be in this workload's egress allowlist \
             (allowedHosts). (details: {detail})"
        )),
        other => OsvError::Request(other.to_string()),
    })?;

    let status = response.status();
    let body = response.into_body();

    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|err| OsvError::Request(format!("decoding response: {err}")));
    }

    // Non-2xx: pull OSV's JSON `message` for detail when present.
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());

    match status.as_u16() {
        404 => Err(OsvError::NotFound(what.to_owned())),
        code => Err(OsvError::Request(format!("HTTP {code}: {message}"))),
    }
}

/// GETs an OSV endpoint (absolute path) and returns the decoded JSON body.
async fn get(path: &str, what: &str) -> Result<Value, OsvError> {
    let url = format!("{BASE_URL}{path}");
    let request = http::Request::get(&url)
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .body(bytes::Bytes::new())
        .map_err(|err| OsvError::Request(format!("building request: {err}")))?;
    send(request, what).await
}

/// POSTs a JSON body to an OSV endpoint and returns the decoded JSON response.
async fn post_json(path: &str, json_body: &Value, what: &str) -> Result<Value, OsvError> {
    let url = format!("{BASE_URL}{path}");
    let encoded = serde_json::to_vec(json_body)
        .map_err(|err| OsvError::Request(format!("encoding request: {err}")))?;
    let request = http::Request::post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .body(bytes::Bytes::from(encoded))
        .map_err(|err| OsvError::Request(format!("building request: {err}")))?;
    send(request, what).await
}

/// `lookup_package_vulnerabilities` — POST `/v1/query` with a package (and
/// optional version) and summarize the matching advisories.
pub async fn lookup_package_vulnerabilities(
    ecosystem: &str,
    package: &str,
    version: Option<&str>,
) -> Result<Value, OsvError> {
    let mut req_body = json!({
        "package": {
            "name": package,
            "ecosystem": ecosystem,
        }
    });
    if let Some(v) = version.map(str::trim).filter(|v| !v.is_empty()) {
        req_body["version"] = json!(v);
    }

    let body = post_json("/v1/query", &req_body, "package").await?;

    let items: Vec<Value> = body
        .get("vulns")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(vuln_summary).collect())
        .unwrap_or_default();

    if items.is_empty() {
        let where_ = match version {
            Some(v) => format!("{package} version {v} ({ecosystem})"),
            None => format!("{package} ({ecosystem})"),
        };
        return Ok(json!({
            "ecosystem": ecosystem,
            "package": package,
            "version": version,
            "vulnerable": false,
            "count": 0,
            "message": format!("No known vulnerabilities for {where_} in the OSV database."),
        }));
    }

    Ok(json!({
        "ecosystem": ecosystem,
        "package": package,
        "version": version,
        "vulnerable": true,
        "count": items.len(),
        "vulnerabilities": items,
    }))
}

/// `get_vulnerability` — GET `/v1/vulns/{id}` and return the trimmed record.
pub async fn get_vulnerability(id: &str) -> Result<Value, OsvError> {
    let id = id.trim();
    let path = format!("/v1/vulns/{}", encode_path(id));
    let v = get(&path, id).await?;
    Ok(vuln_full(&v))
}

/// Compact per-advisory projection used by `lookup_package_vulnerabilities`.
fn vuln_summary(v: &Value) -> Value {
    json!({
        "id": v.get("id"),
        "summary": v.get("summary"),
        "aliases": v.get("aliases").cloned().unwrap_or(Value::Null),
        "severity": severity(v),
        "affected_ranges": range_lines(v, MAX_RANGE_LINES),
        "references": reference_urls(v, MAX_REFERENCES),
    })
}

/// Full per-advisory projection used by `get_vulnerability`.
fn vuln_full(v: &Value) -> Value {
    json!({
        "id": v.get("id"),
        "summary": v.get("summary"),
        "details": truncate_details(v.get("details").and_then(Value::as_str)),
        "aliases": v.get("aliases").cloned().unwrap_or(Value::Null),
        "severity": severity(v),
        "affected": affected(v),
        "references": reference_urls(v, MAX_REFERENCES),
        "published": v.get("published"),
        "modified": v.get("modified"),
    })
}

/// Projects the top-level `severity` array to `{type, score}` entries (the
/// CVSS vector/score when present). OSV also nests severity under each
/// `affected` entry; the top-level array is the advisory-wide rating.
fn severity(v: &Value) -> Value {
    let mut entries: Vec<Value> = v
        .get("severity")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|s| json!({ "type": s.get("type"), "score": s.get("score") }))
                .collect()
        })
        .unwrap_or_default();

    // Fall back to any per-affected severity when the advisory has no
    // top-level rating (common for GHSA-sourced records).
    if entries.is_empty() {
        if let Some(arr) = v.get("affected").and_then(Value::as_array) {
            for a in arr {
                if let Some(sev) = a.get("severity").and_then(Value::as_array) {
                    for s in sev {
                        entries.push(json!({ "type": s.get("type"), "score": s.get("score") }));
                    }
                }
            }
        }
    }

    if entries.is_empty() {
        Value::Null
    } else {
        Value::Array(entries)
    }
}

/// Flattens every affected package's version ranges into readable one-line
/// summaries, capped at `max`.
fn range_lines(v: &Value, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let Some(arr) = v.get("affected").and_then(Value::as_array) else {
        return out;
    };
    for a in arr {
        let name = a
            .pointer("/package/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let eco = a
            .pointer("/package/ecosystem")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(ranges) = a.get("ranges").and_then(Value::as_array) else {
            continue;
        };
        for r in ranges {
            let mut introduced: Option<&str> = None;
            let mut fixed: Option<&str> = None;
            let mut last_affected: Option<&str> = None;
            if let Some(events) = r.get("events").and_then(Value::as_array) {
                for e in events {
                    if let Some(x) = e.get("introduced").and_then(Value::as_str) {
                        introduced = Some(x);
                    }
                    if let Some(x) = e.get("fixed").and_then(Value::as_str) {
                        fixed = Some(x);
                    }
                    if let Some(x) = e.get("last_affected").and_then(Value::as_str) {
                        last_affected = Some(x);
                    }
                }
            }
            let mut line = format!("{eco}/{name}: >= {}", introduced.unwrap_or("0"));
            if let Some(f) = fixed {
                line.push_str(&format!(", fixed in {f}"));
            } else if let Some(l) = last_affected {
                line.push_str(&format!(", last affected {l}"));
            } else {
                line.push_str(", no fixed version listed");
            }
            out.push(line);
            if out.len() >= max {
                return out;
            }
        }
    }
    out
}

/// Structured affected-packages projection used by `get_vulnerability`:
/// ecosystem, package, fixed versions, and the version-range summaries.
fn affected(v: &Value) -> Vec<Value> {
    let Some(arr) = v.get("affected").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .map(|a| {
            let mut fixed: Vec<Value> = Vec::new();
            if let Some(ranges) = a.get("ranges").and_then(Value::as_array) {
                for r in ranges {
                    if let Some(events) = r.get("events").and_then(Value::as_array) {
                        for e in events {
                            if let Some(f) = e.get("fixed") {
                                fixed.push(f.clone());
                            }
                        }
                    }
                }
            }
            json!({
                "ecosystem": a.pointer("/package/ecosystem").cloned().unwrap_or(Value::Null),
                "package": a.pointer("/package/name").cloned().unwrap_or(Value::Null),
                "fixed_versions": fixed,
                "ranges": range_lines_for_affected(a),
            })
        })
        .collect()
}

/// Range summaries for a single affected entry (used by [`affected`]).
fn range_lines_for_affected(a: &Value) -> Vec<String> {
    // Reuse range_lines by wrapping the single entry in a synthetic advisory.
    let wrapper = json!({ "affected": [a] });
    range_lines(&wrapper, MAX_RANGE_LINES)
}

/// Up to `max` reference URLs from the advisory's `references` array.
fn reference_urls(v: &Value, max: usize) -> Vec<Value> {
    v.get("references")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r.get("url").cloned())
                .take(max)
                .collect()
        })
        .unwrap_or_default()
}

/// Truncates the advisory `details` markdown to [`MAX_DETAILS_BYTES`], marking
/// the cut. Returns [`Value::Null`] when there are no details.
fn truncate_details(details: Option<&str>) -> Value {
    match details {
        None => Value::Null,
        Some(d) if d.len() <= MAX_DETAILS_BYTES => Value::String(d.to_owned()),
        Some(d) => {
            // Cut on a char boundary at or below the byte cap.
            let mut end = MAX_DETAILS_BYTES;
            while end > 0 && !d.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(format!("{}… [truncated]", &d[..end]))
        }
    }
}
