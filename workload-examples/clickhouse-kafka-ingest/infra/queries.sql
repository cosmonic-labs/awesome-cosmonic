-- Exploration and troubleshooting for the ingest pipeline. Paste into
-- clickhouse-client:
--
--   docker compose exec clickhouse \
--     clickhouse-client --user analytics --password analytics

-- ---------------------------------------------------------------------------
-- Is it working?
-- ---------------------------------------------------------------------------

-- Consumer health. The first place to look when rows stop arriving: it shows
-- the partition assignment, the last poll, and the 10 most recent exceptions
-- per consumer. An empty exceptions array and a recent last_poll_time is what
-- healthy looks like.
SELECT
    table,
    assignments.partition_id AS partitions,
    num_messages_read,
    num_rebalance_assignments,
    last_poll_time,
    exceptions.text
FROM system.kafka_consumers
FORMAT Vertical;

-- End-to-end lag. `event_lag` is how far behind the newest event is;
-- `pipeline_lag` is how long the newest row spent getting from Kafka into
-- storage. A growing pipeline_lag means ClickHouse is not keeping up.
SELECT
    count()                                          AS rows,
    max(occurred_at)                                 AS newest_event,
    dateDiff('second', max(occurred_at), now())      AS event_lag_seconds,
    dateDiff('second', max(ingested_at), now())      AS pipeline_lag_seconds
FROM analytics.events;

-- Dead letters. Non-zero here with a healthy consumer means a producer is
-- sending something the schema does not accept.
SELECT seen_at, partition, offset, error, raw_message
FROM analytics.events_errors
ORDER BY seen_at DESC
LIMIT 10
FORMAT Vertical;

-- ---------------------------------------------------------------------------
-- Reading the data
-- ---------------------------------------------------------------------------

-- The rollup, merged. AggregatingMergeTree collapses rows sharing an ORDER BY
-- key in the background, so always read aggregate states through *Merge and
-- GROUP BY - never assume one row per key.
SELECT
    minute,
    sum(events)         AS events,
    uniqMerge(sessions) AS sessions,
    sum(revenue_cents) / 100 AS revenue
FROM analytics.events_per_minute
WHERE minute >= now() - INTERVAL 1 HOUR
GROUP BY minute
ORDER BY minute DESC;

-- Funnel by session. The kind of query the raw table exists for: the rollup
-- cannot answer it because it has already thrown away session sequencing.
SELECT
    event_type,
    uniq(session_id) AS sessions
FROM analytics.events
WHERE occurred_at >= now() - INTERVAL 1 HOUR
GROUP BY event_type
ORDER BY sessions DESC;

-- Enrichment done in the view, not the producer: referrer_host was derived
-- from the raw referrer URL by domain() at ingest time.
SELECT referrer_host, count() AS events, uniq(session_id) AS sessions
FROM analytics.events
WHERE referrer_host != ''
GROUP BY referrer_host
ORDER BY events DESC;

-- ---------------------------------------------------------------------------
-- Verifying pipeline properties
-- ---------------------------------------------------------------------------

-- Duplicate detection. Delivery is at-least-once, so a rebalance can redeliver
-- a message. Because the view carried the Kafka coordinates through, this is
-- answerable at all - any event_id appearing at two different offsets was
-- delivered twice.
SELECT event_id, count() AS copies, groupArray((kafka_partition, kafka_offset)) AS at
FROM analytics.events
GROUP BY event_id
HAVING copies > 1
ORDER BY copies DESC
LIMIT 20;

-- Partition balance. The gateway keys by session_id, so a heavily skewed
-- distribution means a few sessions dominate the traffic.
SELECT kafka_partition, count() AS events, uniq(session_id) AS sessions
FROM analytics.events
GROUP BY kafka_partition
ORDER BY kafka_partition;

-- Part count per table. MergeTree merges in the background; a persistently
-- high count means inserts are arriving too small and too often, which is what
-- kafka_max_block_size and kafka_flush_interval_ms control.
SELECT table, count() AS parts, sum(rows) AS rows, formatReadableSize(sum(bytes_on_disk)) AS size
FROM system.parts
WHERE database = 'analytics' AND active
GROUP BY table
ORDER BY parts DESC;

-- ---------------------------------------------------------------------------
-- Operating on the pipeline
-- ---------------------------------------------------------------------------

-- Pause and resume consumption without dropping anything. Useful when the
-- target table needs a schema change: detach the view, migrate, reattach.
-- Kafka retains the messages, so consumption picks up where it stopped.
--   DETACH TABLE analytics.events_mv;
--   ATTACH TABLE analytics.events_mv;

-- Replay from the beginning. The consumer group offset lives in Kafka, so
-- resetting means changing kafka_group_name (or resetting offsets with rpk).
--   ALTER TABLE analytics.events_queue MODIFY SETTING kafka_group_name = 'clickhouse-clickstream-v2';

-- Backfill a new rollup from data already in storage. A MATERIALIZED VIEW only
-- sees rows inserted after it is created, so a new view over historical data
-- needs an explicit INSERT SELECT from the MergeTree table.
--   INSERT INTO analytics.events_per_minute
--   SELECT toStartOfMinute(occurred_at), event_type, country,
--          count(), sum(revenue_cents), uniqState(session_id)
--   FROM analytics.events
--   WHERE occurred_at < now() - INTERVAL 1 HOUR
--   GROUP BY 1, 2, 3;
