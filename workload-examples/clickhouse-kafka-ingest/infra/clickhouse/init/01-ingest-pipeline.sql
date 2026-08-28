-- Kafka -> ClickHouse ingestion, entirely in SQL.
--
-- Three kinds of object, and the distinction is the whole idea:
--
--   1. A Kafka engine table. This stores nothing. It is a consumer group
--      description - brokers, topic, format - that ClickHouse polls in the
--      background. SELECTing from it directly consumes messages and destroys
--      them, so nothing below ever does.
--   2. MergeTree tables. Real storage, real indexes, what you query.
--   3. MATERIALIZED VIEWs. An insert trigger: every batch the Kafka engine
--      pulls runs through the view's SELECT, and the result is inserted into
--      the target table. This is the moving part. There is no external
--      orchestrator and no separate compute cluster.
--
-- Attaching a second MATERIALIZED VIEW to the same queue is how you fan one
-- topic out to several destinations - the raw table and the per-minute
-- rollup below both read the same batch, in the same pass.

CREATE DATABASE IF NOT EXISTS analytics;

-- ---------------------------------------------------------------------------
-- 1. The queue. Not storage.
-- ---------------------------------------------------------------------------
--
-- Column types here describe how to *parse* each message, not how to store
-- it. A message that does not match is a parse failure, and by default one
-- bad message stalls the consumer and retries forever. `kafka_handle_error_mode
-- = 'stream'` changes that: failures are surfaced as rows with the `_error`
-- virtual column set, so they can be routed to a dead-letter table instead of
-- blocking the topic. That single setting is the difference between a
-- pipeline that survives a bad producer deploy and one that does not.

CREATE TABLE IF NOT EXISTS analytics.events_queue
(
    event_id      String,
    occurred_at   DateTime64(3, 'UTC'),
    session_id    String,
    user_id       String,
    event_type    LowCardinality(String),
    path          String,
    referrer      String,
    country       LowCardinality(String),
    device        LowCardinality(String),
    revenue_cents UInt32,
    properties    Map(String, String)
)
ENGINE = Kafka
SETTINGS
    kafka_broker_list = 'redpanda:9092',
    kafka_topic_list = 'clickstream.events',
    -- The consumer group. Scaling out means adding replicas that share this
    -- name, not sharding the SQL.
    kafka_group_name = 'clickhouse-clickstream',
    kafka_format = 'JSONEachRow',
    -- One consumer per table, up to the topic's partition count. Raising this
    -- is the first lever for ingest throughput.
    kafka_num_consumers = 1,
    -- A batch flushes when it hits either bound, whichever comes first.
    -- Smaller values mean fresher data and more, smaller MergeTree parts;
    -- ClickHouse would much rather have fewer, larger inserts.
    kafka_max_block_size = 100000,
    kafka_flush_interval_ms = 1000,
    kafka_poll_max_batch_size = 10000,
    -- Route parse failures to a stream instead of stalling the consumer.
    kafka_handle_error_mode = 'stream',
    -- The producer sends ISO 8601 timestamps ("2026-08-06T23:00:00.123Z").
    -- ClickHouse's default datetime parser does not accept the T/Z form and
    -- would reject every row; 'best_effort' does.
    date_time_input_format = 'best_effort',
    -- A producer adding a field it forgot to tell you about should not take
    -- the pipeline down.
    input_format_skip_unknown_fields = 1;

-- ---------------------------------------------------------------------------
-- 2. Storage: the table you actually query.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS analytics.events
(
    occurred_at     DateTime64(3, 'UTC'),
    ingested_at     DateTime64(3, 'UTC'),
    event_id        String,
    session_id      String,
    user_id         String,
    event_type      LowCardinality(String),
    path            String,
    referrer_host   String,
    country         LowCardinality(String),
    device          LowCardinality(String),
    revenue_cents   UInt32,
    properties      Map(String, String),
    -- Carried over from the Kafka virtual columns. Keeping the offset makes
    -- "did this message land?" answerable, and makes duplicates detectable
    -- after a rebalance - delivery is at-least-once, not exactly-once.
    kafka_topic     LowCardinality(String),
    kafka_partition UInt64,
    kafka_offset    UInt64
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(occurred_at)
-- Order by what you filter on, coarsest first. This is the primary index, and
-- it is the single biggest determinant of query speed.
ORDER BY (event_type, occurred_at, event_id)
TTL toDateTime(occurred_at) + INTERVAL 90 DAY;

-- ---------------------------------------------------------------------------
-- 3. The transform: SELECT that runs on every batch.
-- ---------------------------------------------------------------------------
--
-- `TO analytics.events` makes this an insert trigger rather than a stored
-- query. Nothing schedules it. When the Kafka engine assembles a block, this
-- SELECT runs over that block and the rows land in `events`.
--
-- `_topic`, `_partition`, `_offset`, and `_error` are virtual columns on the
-- Kafka engine, available here and nowhere else downstream - which is why
-- they are copied into real columns now.

CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.events_mv
TO analytics.events
AS
SELECT
    occurred_at,
    now64(3) AS ingested_at,
    event_id,
    session_id,
    user_id,
    event_type,
    path,
    -- Cheap enrichment. Anything expressible in SQL belongs here rather than
    -- in the producer, where changing it means redeploying every client.
    domain(referrer) AS referrer_host,
    upper(country) AS country,
    device,
    revenue_cents,
    properties,
    _topic AS kafka_topic,
    _partition AS kafka_partition,
    _offset AS kafka_offset
FROM analytics.events_queue
WHERE _error = ''
  AND event_id != '';

-- ---------------------------------------------------------------------------
-- 4. Fan-out: a second view over the same batch.
-- ---------------------------------------------------------------------------
--
-- Pre-aggregating at ingest is why this pattern is worth the setup. The
-- dashboard reads a table that is already grouped by minute, so it never
-- scans raw events.
--
-- AggregatingMergeTree merges rows sharing an ORDER BY key in the background.
-- Sums use SimpleAggregateFunction because sum-of-sums is a sum;
-- uniq cannot be summed, so it stores a HyperLogLog sketch as an
-- AggregateFunction state and is read back with uniqMerge.

CREATE TABLE IF NOT EXISTS analytics.events_per_minute
(
    minute        DateTime('UTC'),
    event_type    LowCardinality(String),
    country       LowCardinality(String),
    events        SimpleAggregateFunction(sum, UInt64),
    revenue_cents SimpleAggregateFunction(sum, UInt64),
    sessions      AggregateFunction(uniq, String)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(minute)
ORDER BY (minute, event_type, country)
TTL minute + INTERVAL 1 YEAR;

CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.events_per_minute_mv
TO analytics.events_per_minute
AS
SELECT
    toStartOfMinute(occurred_at) AS minute,
    event_type,
    upper(country) AS country,
    count() AS events,
    sum(revenue_cents) AS revenue_cents,
    uniqState(session_id) AS sessions
FROM analytics.events_queue
WHERE _error = ''
  AND event_id != ''
-- Aggregates the batch before writing, so a 10k-message batch inserts a few
-- dozen rows. The GROUP BY is per batch, not global; AggregatingMergeTree
-- finishes the job at merge time.
GROUP BY minute, event_type, country;

-- ---------------------------------------------------------------------------
-- 5. Dead letters.
-- ---------------------------------------------------------------------------
--
-- The other half of kafka_handle_error_mode = 'stream'. Messages that failed
-- to parse arrive with `_error` set and `_raw_message` holding the original
-- bytes, so they can be inspected and replayed rather than silently dropped.
-- Try it: POST /events with "revenue_cents": "free".

CREATE TABLE IF NOT EXISTS analytics.events_errors
(
    seen_at     DateTime64(3, 'UTC'),
    topic       LowCardinality(String),
    partition   UInt64,
    offset      UInt64,
    raw_message String,
    error       String
)
ENGINE = MergeTree
ORDER BY (seen_at, topic, partition, offset)
TTL toDateTime(seen_at) + INTERVAL 14 DAY;

CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.events_errors_mv
TO analytics.events_errors
AS
SELECT
    now64(3)      AS seen_at,
    _topic        AS topic,
    _partition    AS partition,
    _offset       AS offset,
    _raw_message  AS raw_message,
    _error        AS error
FROM analytics.events_queue
WHERE _error != '';
