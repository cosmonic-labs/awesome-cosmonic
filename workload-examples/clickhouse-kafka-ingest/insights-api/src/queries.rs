//! The read side.
//!
//! Note which table each query hits. The dashboard's time series and
//! breakdowns come from `events_per_minute`, the pre-aggregated rollup, so they
//! never scan raw events - that is the payoff for the second MATERIALIZED
//! VIEW. Only "recent events" touches `analytics.events`, bounded by LIMIT.
//!
//! Every constant here is a `&'static str` with nothing interpolated.
//!
//! One ClickHouse-specific trap worth knowing, because it is silent until it
//! is not: unlike standard SQL, a SELECT alias is visible in WHERE, GROUP BY,
//! and ORDER BY. So `toString(minute) AS minute` makes `WHERE minute >= now()
//! - INTERVAL 30 MINUTE` compare a String to a DateTime, and
//! `ORDER BY occurred_at` sorts lexicographically instead of chronologically.
//! Nothing below wraps a column in `toString` and reuses its name; the JSON
//! format already renders DateTime as a string.

/// Totals over the retained window, plus ingestion lag. `ingest_lag_seconds`
/// is the gap between the newest row's event time and now: the number to watch
/// to know whether the pipeline is keeping up.
pub(crate) const TOTALS: &str = r#"
SELECT
    count()            AS events,
    uniq(session_id)   AS sessions,
    sum(revenue_cents) AS revenue_cents,
    ifNull(dateDiff('second', max(occurred_at), now()), 0) AS ingest_lag_seconds
FROM analytics.events
FORMAT JSON
"#;

/// Time series for the chart, read from the rollup.
pub(crate) const PER_MINUTE: &str = r#"
SELECT
    minute,
    sum(events)         AS events,
    uniqMerge(sessions) AS sessions,
    sum(revenue_cents)  AS revenue_cents
FROM analytics.events_per_minute
WHERE minute >= now() - INTERVAL 30 MINUTE
GROUP BY minute
ORDER BY minute
FORMAT JSON
"#;

pub(crate) const BY_EVENT_TYPE: &str = r#"
SELECT
    event_type,
    sum(events)        AS events,
    sum(revenue_cents) AS revenue_cents
FROM analytics.events_per_minute
WHERE minute >= now() - INTERVAL 60 MINUTE
GROUP BY event_type
ORDER BY events DESC
LIMIT 10
FORMAT JSON
"#;

pub(crate) const BY_COUNTRY: &str = r#"
SELECT
    country,
    sum(events)         AS events,
    uniqMerge(sessions) AS sessions
FROM analytics.events_per_minute
WHERE minute >= now() - INTERVAL 60 MINUTE
GROUP BY country
ORDER BY events DESC
LIMIT 10
FORMAT JSON
"#;

/// The only query against raw events. Shows the Kafka coordinates carried
/// through by the materialized view, which is what makes a row traceable back
/// to the message it came from.
pub(crate) const RECENT: &str = r#"
SELECT
    occurred_at,
    event_type,
    path,
    country,
    device,
    revenue_cents,
    referrer_host,
    kafka_partition,
    kafka_offset
FROM analytics.events
ORDER BY occurred_at DESC
LIMIT 15
FORMAT JSON
"#;

/// Dead letters. A non-zero count here with a healthy consumer is the signal
/// that a producer is sending something the schema does not accept.
pub(crate) const ERRORS: &str = r#"
SELECT
    seen_at,
    partition,
    offset,
    substring(error, 1, 160)       AS error,
    substring(raw_message, 1, 200) AS raw_message
FROM analytics.events_errors
ORDER BY seen_at DESC
LIMIT 5
FORMAT JSON
"#;

/// Consumer health straight from ClickHouse's own introspection. This is the
/// first place to look when rows stop arriving: it reports the partition
/// assignment, the last poll, and the most recent exception per consumer.
///
/// `assignments.*` and `exceptions.*` are nested Array columns, so the last
/// exception is the last element of `exceptions.text` rather than a scalar
/// column. `arrayElement(arr, -1)` indexes from the end and yields '' for an
/// empty array, which is exactly the healthy case.
pub(crate) const CONSUMERS: &str = r#"
SELECT
    table,
    consumer_id,
    assignments.partition_id          AS partitions,
    num_messages_read,
    num_rebalance_assignments         AS rebalance_assignments,
    is_currently_used                 AS active,
    last_poll_time,
    arrayElement(exceptions.text, -1) AS last_exception
FROM system.kafka_consumers
ORDER BY table, consumer_id
FORMAT JSON
"#;
