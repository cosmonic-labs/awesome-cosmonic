//! The event contract, and a generator for synthetic traffic.
//!
//! Field names and types here have to line up with the column definitions on
//! `analytics.events_queue`, because those columns *are* the parser. A
//! mismatch does not fail at build time; it shows up as a row in
//! `analytics.events_errors`.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::rng::Rng;

/// What the producer puts on the topic.
///
/// `occurred_at` is serialized as RFC 3339, which is why the Kafka table sets
/// `date_time_input_format = 'best_effort'`.
#[derive(Debug, Serialize)]
pub(crate) struct Event {
    pub(crate) event_id: String,
    pub(crate) occurred_at: String,
    pub(crate) session_id: String,
    pub(crate) user_id: String,
    pub(crate) event_type: String,
    pub(crate) path: String,
    pub(crate) referrer: String,
    pub(crate) country: String,
    pub(crate) device: String,
    pub(crate) revenue_cents: u32,
    pub(crate) properties: BTreeMap<String, String>,
}

/// What a caller may send to `POST /events`. Every field is optional except
/// `event_type`: the gateway fills in identifiers and the receipt timestamp
/// so a browser or curl one-liner can post something minimal.
#[derive(Debug, Deserialize)]
pub(crate) struct EventInput {
    pub(crate) event_type: String,
    #[serde(default)]
    pub(crate) event_id: Option<String>,
    #[serde(default)]
    pub(crate) occurred_at: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) user_id: Option<String>,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) referrer: Option<String>,
    #[serde(default)]
    pub(crate) country: Option<String>,
    #[serde(default)]
    pub(crate) device: Option<String>,
    #[serde(default)]
    pub(crate) revenue_cents: Option<u32>,
    #[serde(default)]
    pub(crate) properties: BTreeMap<String, String>,
}

/// Accepts a bare object, a bare array, or `{"events": [...]}` so the endpoint
/// is pleasant from both a browser and a shell.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum EventPayload {
    One(EventInput),
    Many(Vec<EventInput>),
    Wrapped { events: Vec<EventInput> },
}

impl EventPayload {
    pub(crate) fn into_vec(self) -> Vec<EventInput> {
        match self {
            Self::One(e) => vec![e],
            Self::Many(v) | Self::Wrapped { events: v } => v,
        }
    }
}

/// Rejects what the gateway can catch cheaply. Anything subtler is the
/// dead-letter table's job: validating here and in ClickHouse both is
/// duplicated logic that drifts.
pub(crate) fn validate(input: &EventInput) -> Result<(), String> {
    if input.event_type.trim().is_empty() {
        return Err("event_type must not be empty".into());
    }
    if input.event_type.len() > 64 {
        return Err("event_type must be 64 characters or fewer".into());
    }
    if let Some(country) = &input.country
        && !country.is_empty()
        && country.len() != 2
    {
        return Err("country must be a 2-letter code".into());
    }
    Ok(())
}

impl EventInput {
    /// Fills the gaps the caller left. `now_ms` is passed in rather than read
    /// from the clock so a whole batch shares one receipt time.
    pub(crate) fn into_event(self, rng: &mut Rng, now_ms: i64) -> Event {
        Event {
            event_id: self.event_id.unwrap_or_else(|| rng.uuid_v4()),
            occurred_at: self.occurred_at.unwrap_or_else(|| iso8601(now_ms)),
            session_id: self.session_id.unwrap_or_else(|| rng.id("sess")),
            user_id: self.user_id.unwrap_or_else(|| rng.id("user")),
            event_type: self.event_type,
            path: self.path.unwrap_or_else(|| "/".into()),
            referrer: self.referrer.unwrap_or_default(),
            country: self.country.unwrap_or_else(|| "US".into()),
            device: self.device.unwrap_or_else(|| "unknown".into()),
            revenue_cents: self.revenue_cents.unwrap_or(0),
            properties: self.properties,
        }
    }
}

/// Formats unix milliseconds as `2026-08-06T23:30:00.123Z`.
pub(crate) fn iso8601(millis: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(millis)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp_nanos(0))
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

const EVENT_TYPES: &[(&str, u32)] = &[
    // (type, revenue in cents when it fires). Weighted by position below.
    ("page_view", 0),
    ("search", 0),
    ("add_to_cart", 0),
    ("checkout", 4_999),
    ("purchase", 4_999),
];
const PATHS: &[&str] = &["/", "/pricing", "/docs", "/blog/wasm-at-the-edge", "/cart"];
const REFERRERS: &[&str] = &[
    "https://news.ycombinator.com/item?id=42",
    "https://www.google.com/search?q=webassembly",
    "https://bsky.app/profile/example",
    "",
];
const COUNTRIES: &[&str] = &["US", "DE", "GB", "JP", "BR", "IN"];
const DEVICES: &[&str] = &["mobile", "desktop", "tablet"];

/// Synthesizes a plausible clickstream. Sessions repeat across events in a
/// batch (a small pool of session ids) so `uniq(session_id)` in the rollup has
/// something to actually deduplicate.
pub(crate) fn synthesize(rng: &mut Rng, now_ms: i64, index: usize) -> Event {
    // Skew toward page views: the tail types are the interesting ones and
    // should stay a minority.
    let type_idx = match rng.below(100) {
        0..=59 => 0,
        60..=74 => 1,
        75..=89 => 2,
        90..=96 => 3,
        _ => 4,
    };
    let (event_type, base_revenue) = EVENT_TYPES.get(type_idx).copied().unwrap_or(("page_view", 0));

    // Spread events over the last 5 minutes so the per-minute rollup has more
    // than one bucket to show.
    let offset_ms = i64::from(rng.below(5 * 60 * 1000));

    Event {
        event_id: rng.uuid_v4(),
        occurred_at: iso8601(now_ms - offset_ms),
        session_id: format!("sess-{:04}", rng.below(200)),
        user_id: format!("user-{:04}", rng.below(80)),
        event_type: event_type.to_string(),
        path: pick(PATHS, rng).to_string(),
        referrer: pick(REFERRERS, rng).to_string(),
        country: pick(COUNTRIES, rng).to_string(),
        device: pick(DEVICES, rng).to_string(),
        revenue_cents: if base_revenue == 0 {
            0
        } else {
            base_revenue + rng.below(5_000)
        },
        properties: BTreeMap::from([
            ("ab_bucket".to_string(), pick(&["a", "b"], rng).to_string()),
            ("batch_index".to_string(), index.to_string()),
        ]),
    }
}

fn pick<'a>(choices: &'a [&'a str], rng: &mut Rng) -> &'a str {
    let i = rng.below(choices.len() as u32) as usize;
    choices.get(i).copied().unwrap_or_default()
}
