# clickhouse-kafka-ingest

A streaming analytics pipeline where the ingestion tier is SQL. Events arrive
over HTTP at a WebAssembly component, land on a Kafka topic, and ClickHouse
pulls them off and transforms them with a `MATERIALIZED VIEW`. A second
component serves a live dashboard over the result.

Between the topic and the queryable table there is essentially no application
code at all where the entire transform is
[`infra/clickhouse/init/01-ingest-pipeline.sql`](infra/clickhouse/init/01-ingest-pipeline.sql),
and it runs inside ClickHouse.

> **Status:** working example, tested locally. Not a hardened production
> deployment — see [What this example does not do](#what-this-example-does-not-do).

## The idea

| Object | Engine | What it actually is |
| --- | --- | --- |
| `events_queue` | `Kafka` | **Not storage.** A consumer group description: brokers, topic, format. ClickHouse polls it in the background. |
| `events` | `MergeTree` | Real storage. Indexed, partitioned, what you query. |
| `events_mv` | `MATERIALIZED VIEW ... TO` | An **insert trigger**. Its `SELECT` runs on every batch the Kafka engine pulls; the result is inserted into `events`. |

Nothing schedules the view. There is no orchestrator to deploy, no checkpoint
store to operate, and no separate compute tier to size. Ingestion scales by
raising `kafka_num_consumers` or adding replicas to the consumer group.

```
                    ┌───────────────────────── WebAssembly components ─────┐
  HTTP POST  ──────▶│  event-gateway                                        │
  /events           │  validate, enrich, produce                            │
                    └──────────────────────────┬────────────────────────────┘
                                               │ HTTP Proxy (POST /topics/…)
                                               ▼
                                    ┌────────────────────┐
                                    │ Redpanda           │  topic:
                                    │ (Kafka API)        │  clickstream.events
                                    └──────────┬─────────┘
                                               │ consumer group
   ┌─────────────────────── ClickHouse ────────▼──────────────────────────┐
   │  events_queue  (Kafka engine — polls, parses, no storage)            │
   │        │                                                             │
   │        ├── events_mv ─────────────▶ events            (MergeTree)    │
   │        ├── events_per_minute_mv ──▶ events_per_minute (Aggregating)  │
   │        └── events_errors_mv ──────▶ events_errors     (dead letters) │
   └────────────────────────────────────────┬─────────────────────────────┘
                                            │ HTTP interface :8123
                    ┌───────────────────────▼─────────────────────────────┐
                    │  insights-api — dashboard + JSON API                 │
                    └──────────────────────────────────────────────────────┘
```

Three views read the *same* batch in the same pass. That fan-out is the reason
to reach for this pattern: one topic feeds the raw table, a pre-aggregated
rollup, and a dead-letter table without reading Kafka three times.

## Why the components talk HTTP to Kafka, not the Kafka protocol

The Kafka wire protocol needs a long-lived TCP connection, cluster metadata,
and a partitioner.

Instead `event-gateway` produces through the **Kafka HTTP Proxy** —
pandaproxy in Redpanda, the REST Proxy in Confluent — so producing is one
`POST` and the only capability the component needs is outbound HTTP, already
constrained by `allowedHosts`. Same story on the read side: ClickHouse's HTTP
interface on 8123 means `insights-api` needs no database driver.

The tradeoff is real: per-request HTTP costs more than a batched native
producer, and the proxy is another hop to run. For a browser-facing collector,
where events arrive as HTTP anyway, it is the natural shape. For a
high-throughput internal pipeline, put a native producer in front of the topic
and leave the ClickHouse side exactly as it is.

## What you need

- [`wash`](https://wasmcloud.com/docs/installation) 2.6 or later (tested on
  2.6.1). It must serve `wasi:http/handler@0.3.0`; 2.6.1 does.
- Rust with the `wasm32-wasip2` target — yes, p2, see
  [wasip3 components](#these-are-wasip3-components). Both components pin
  stable via `rust-toolchain.toml`.
- Docker with Compose v2

## Run it

**1. Start the backing services.** Redpanda, the topic, and ClickHouse with
the whole pipeline applied on first boot:

```console
cd infra
docker compose up -d
```

**2. Start the components**, each in its own terminal:

```console
cd event-gateway && wash dev     # :8000
```

```console
cd insights-api && wash dev      # :8001
```

**3. Generate traffic** and watch it land:

```console
curl -X POST "localhost:8000/events/simulate?count=500"
```

Open <http://localhost:8001> for the dashboard, or ask directly:

```console
curl -s localhost:8001/api/overview | jq .totals
```

Rows appear within `kafka_flush_interval_ms` (1s here). Send your own event:

```console
curl -X POST localhost:8000/events \
  -H 'content-type: application/json' \
  -d '{"event_type":"purchase","path":"/checkout","country":"de","revenue_cents":2599}'
```

## Watch the dead-letter path work

This is the part worth seeing, because it is what separates a pipeline that
survives a bad producer deploy from one that does not. By default, one
unparseable message stalls a ClickHouse Kafka consumer and it retries forever.
`kafka_handle_error_mode = 'stream'` turns failures into rows instead.

Put something malformed on the topic, bypassing the gateway's validation:

```console
curl -X POST localhost:18082/topics/clickstream.events \
  -H 'Content-Type: application/vnd.kafka.json.v2+json' \
  -d '{"records":[{"value":{"event_id":"oops","occurred_at":"2026-01-01T00:00:00Z",
       "session_id":"s","user_id":"u","event_type":"checkout","path":"/x",
       "referrer":"","country":"us","device":"web","revenue_cents":"free",
       "properties":{}}}]}'
```

```console
curl -s localhost:8001/api/errors | jq '.errors[0].error'
```

The message is captured with its raw bytes and the parse error, the consumer
keeps going, and good events flowing at the same time are unaffected.

## Poke at it with SQL

The interesting part is the database, so go look:

```console
docker compose -f infra/docker-compose.yaml exec clickhouse \
  clickhouse-client --user analytics --password analytics
```

[`infra/queries.sql`](infra/queries.sql) has a set to start from. The one to
remember when something looks wrong:

```sql
-- Is the consumer alive, what is it assigned, and what did it last fail on?
SELECT table, assignments.partition_id, num_messages_read,
       last_poll_time, exceptions.text
FROM system.kafka_consumers;
```

## The components

Both are Rust wasip3 components exporting `wasi:http/handler@0.3.0`, and hold
no state.

**`event-gateway`** (`:8000`) — validates, fills in identifiers and
timestamps, and produces to the topic keyed by `session_id` so one session's
events stay ordered on one partition.

| Route | |
| --- | --- |
| `POST /events` | one event, an array, or `{"events": [...]}` |
| `POST /events/simulate?count=N` | synthesize N plausible events |
| `GET /healthz` | resolved configuration |

**`insights-api`** (`:8001`) — serves the dashboard and queries ClickHouse
over HTTP. Its headline numbers and time series read the *rollup* table, never
raw events; only the "recent events" panel touches `analytics.events`.

| Route | |
| --- | --- |
| `GET /` | dashboard |
| `GET /api/overview` | totals, time series, breakdowns, consumer health |
| `GET /api/recent` | newest raw events |
| `GET /api/errors` | dead-lettered messages |
| `GET /healthz` | liveness, including whether ClickHouse answers |

Neither component has a broker address or a password compiled in. Everything
comes from `wasi:config/store`, which is `workload.config` in
`.wash/config.yaml` locally and a ConfigMap or Secret in
[`manifests/`](event-gateway/manifests/workloaddeployment.yaml). The same
`.wasm` runs in both places.

## Deploying to Cosmonic Control

Push each component and apply its manifest:

```console
cd event-gateway
wash build
wash oci push <registry>/event-gateway:0.1.0 target/wasm32-wasip2/release/event_gateway.wasm
kubectl apply -f manifests/workloaddeployment.yaml
```

The manifests assume Redpanda and ClickHouse are already running in-cluster
and referenced by service DNS; edit the ConfigMaps to match. The ClickHouse
password comes from a Secret:

```console
kubectl create secret generic clickhouse-credentials \
  --from-literal=CLICKHOUSE_PASSWORD='...'
```

## These are wasip3 components

Both components target **WASI 0.3**. The entrypoint is
`wasi:http/handler@0.3.0` — a single `async fn handle` that returns a response
— and outbound calls go through `wasi:http/client@0.3.0`, whose `send` is an
`async func` in WIT. Bodies are native component-model `stream<u8>` values.

Concretely, no `wasi:io` appears anywhere on the HTTP path. There is no
`response-outparam` to write into, no pollable to drive, and no `wasi:io`
stream wrapper around a body. A ClickHouse query is `client::send(request)
.await`. A response body is a stream whose write end is owned by a
`spawn_local` task, so headers go out while the body is still being written.

You will still see `wasi:io@0.2.x` in the import list. That is the Rust
standard library's stdio, not the HTTP path — `eprintln!` reaches the host
through wasi-libc. It disappears when std does, which is what the
`wasm32-wasip3` target will eventually deliver.

Three things about this that are easy to get wrong:

**The build target is still `wasm32-wasip2`.** That is not a mistake.
`wasm32-wasip3` is a tier-3 target with no prebuilt std, so today a p3
component is one whose *world* is p3 while std and libc remain p2. Check the
world, not the target triple:

```console
wash inspect target/wasm32-wasip2/release/event_gateway.wasm | grep 0.3.0
```

```text
import wasi:http/types@0.3.0
import wasi:http/client@0.3.0
export wasi:http/handler@0.3.0
```

**Not every interface has a 0.3 release yet.** `wasi:config` does not, so both
components still import `wasi:config/store@0.2.0-rc.1`. Mixed-version imports
are the expected state of the p2 → p3 transition, not a smell.

**`wstd` is not used.** It is a p2 async runtime built on `wasi:io`; the p3
work is done by `wit-bindgen`'s async codegen, which needs the `async-spawn`
and `inter-task-wakeup` features. One consequence worth knowing: that codegen
emits `unsafe` blocks into your crate, so a crate-level `unsafe_code = "deny"`
lint will not compile.

Both components pin stable in `rust-toolchain.toml`. p3 builds fine on stable,
and pinning also sidesteps a nightly bug in the *p2* path: built with a recent
nightly, a `wstd`-based p2 component compiles and instantiates but never
answers a request, with no trap and no log line. That reproduces on the stock
wasmCloud `http-hello-world` template, so it is not specific to this code —
but it is not something these p3 components go through anymore either.

## Things that will bite you

Collected from building this, all of them worth knowing before you copy the
SQL:

- **`SELECT` aliases are visible in `WHERE` and `ORDER BY`.** Unlike standard
  SQL. So `toString(minute) AS minute` makes `WHERE minute >= now() - INTERVAL
  30 MINUTE` compare a String to a DateTime and fail, and `toString(t) AS t
  ... ORDER BY t` silently sorts lexicographically. Do not shadow a column
  with a converted version of itself.
- **ISO 8601 timestamps need `date_time_input_format = 'best_effort'`.**
  ClickHouse's default parser rejects `2026-08-06T23:30:00.123Z` outright, so
  every row fails until you set it.
- **`SELECT`ing from a Kafka engine table consumes messages** and destroys
  them. It is a queue, not a table. Query the `MergeTree` target instead.
- **Delivery is at-least-once.** A rebalance can redeliver. This example
  carries `_partition` and `_offset` into the target table so duplicates are
  detectable; deduplicate with `ReplacingMergeTree` if you need to.
- **Batch size is a real tuning knob.** `kafka_max_block_size` and
  `kafka_flush_interval_ms` trade freshness against part count, and ClickHouse
  would much rather have fewer, larger inserts.
- **`system.kafka_consumers` is the first place to look** when rows stop
  arriving, before the server log.

## What this example does not do

Deliberately out of scope, and each one matters before this goes anywhere real:

- **No authentication on the gateway.** `POST /events` is open, and CORS is
  wide open so the dashboard can drive it. A real collector needs an API key
  or origin allowlist at minimum.
- **No TLS or SASL to the broker.** Plaintext throughout.
- **No exactly-once.** See at-least-once above.
- **No schema registry.** The event contract lives in two places — the Rust
  struct and the Kafka table's column list — and they are kept in sync by
  hand. A registry with Avro or Protobuf is the answer at scale.
- **Single-node everything.** One Redpanda broker, one ClickHouse server, no
  replication.
- **`CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT: 1`** in compose is a local
  convenience, not a production setting.

## Layout

```
infra/
  docker-compose.yaml            Redpanda + topic + ClickHouse
  clickhouse/init/               applied on first boot — the pipeline itself
  clickhouse/config/             consumer logging
  queries.sql                    exploration and troubleshooting
event-gateway/                   HTTP in, Kafka out
insights-api/                    ClickHouse in, dashboard out
```
