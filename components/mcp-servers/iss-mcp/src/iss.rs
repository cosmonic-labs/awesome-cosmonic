//! Open Notify client: who is in space, and where the ISS is right now, over
//! `wasi:http` via the bridge's outbound client.
//!
//! Endpoints (both GET, JSON, **HTTP only** — Open Notify has no HTTPS):
//! - `http://api.open-notify.org/astros.json`  — people currently in space
//! - `http://api.open-notify.org/iss-now.json` — ISS latitude/longitude
//!
//! Open Notify is a small community service that is occasionally briefly
//! unavailable, so tool errors are phrased for a first-time reader rather than
//! surfacing raw transport detail.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{json, Value};

use crate::bridge::outbound;

/// A descriptive User-Agent, so the upstream can see who is calling.
const USER_AGENT: &str = "iss-mcp (Cosmonic Desktop example)";

/// Base URL for the Open Notify API. The `OPEN_NOTIFY_BASE_URL` override exists
/// for testing against a local fixture.
fn base_url() -> String {
    std::env::var("OPEN_NOTIFY_BASE_URL")
        .unwrap_or_else(|_| "http://api.open-notify.org".to_owned())
}

/// A lookup failure. Open Notify has no per-item "not found" case (the tools
/// take no parameters), so there is a single variant, rendered as a friendly,
/// try-again message.
pub enum IssError {
    Failed(String),
}

impl IssError {
    /// Renders the error as an MCP tool-level error result — the caller sees
    /// the message. (This is not a protocol error; the request was valid.)
    pub fn into_tool_result(self) -> CallToolResult {
        let IssError::Failed(detail) = self;
        let text = format!(
            "Couldn't reach the ISS tracker right now: {detail}. Open Notify is a small \
             community service and is sometimes briefly unavailable — try again in a moment."
        );
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

/// GETs a URL and deserializes the JSON body.
async fn get_json(url: &str) -> Result<Value, IssError> {
    let request = http::Request::get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .body(bytes::Bytes::new())
        .map_err(|err| IssError::Failed(format!("building request: {err}")))?;

    let response = outbound::fetch(request)
        .await
        .map_err(|err| IssError::Failed(format!("GET {url}: {err}")))?;

    match response.status().as_u16() {
        200 => serde_json::from_slice(response.body())
            .map_err(|err| IssError::Failed(format!("couldn't parse the response from {url}: {err}"))),
        other => Err(IssError::Failed(format!(
            "GET {url} returned HTTP {other}"
        ))),
    }
}

/// `who_is_in_space` implementation: the people currently in space, with the
/// spacecraft each is aboard.
pub async fn who_is_in_space() -> Result<Value, IssError> {
    let url = format!("{}/astros.json", base_url());
    let value = get_json(&url).await?;

    let people: Vec<Value> = value
        .get("people")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|person| {
            json!({
                "name": person.get("name").and_then(Value::as_str),
                "craft": person.get("craft").and_then(Value::as_str),
            })
        })
        .collect();

    // Prefer the upstream count, but fall back to the length we actually parsed.
    let number = value
        .get("number")
        .and_then(Value::as_u64)
        .unwrap_or(people.len() as u64);

    Ok(json!({
        "number": number,
        "people": people,
    }))
}

/// `iss_position` implementation: the ISS's current latitude and longitude.
pub async fn iss_position() -> Result<Value, IssError> {
    let url = format!("{}/iss-now.json", base_url());
    let value = get_json(&url).await?;

    let position = value.get("iss_position");
    // Open Notify reports latitude/longitude as JSON *strings*; expose them as
    // numbers so callers get real coordinates.
    let latitude = position
        .and_then(|p| p.get("latitude"))
        .and_then(parse_coord);
    let longitude = position
        .and_then(|p| p.get("longitude"))
        .and_then(parse_coord);
    let (latitude, longitude) = match (latitude, longitude) {
        (Some(lat), Some(lon)) => (lat, lon),
        _ => {
            return Err(IssError::Failed(format!(
                "{url} did not include a usable ISS position"
            )))
        }
    };

    let mut out = json!({
        "latitude": latitude,
        "longitude": longitude,
    });
    if let Some(timestamp) = value.get("timestamp").and_then(Value::as_i64) {
        out["timestamp"] = json!(timestamp);
        if let Some(iso) = iso8601(timestamp) {
            out["timestamp_iso8601"] = json!(iso);
        }
    }
    Ok(out)
}

/// Parses a coordinate that Open Notify sends as a JSON string (e.g. `"38.1"`),
/// tolerating a plain JSON number too.
fn parse_coord(value: &Value) -> Option<f64> {
    match value {
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Formats a Unix timestamp (seconds) as an ISO-8601 UTC string, e.g.
/// `2026-08-14T18:20:00Z`. Self-contained (no chrono dependency) via Howard
/// Hinnant's days-from-civil algorithm.
fn iso8601(secs: i64) -> Option<String> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };

    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}
