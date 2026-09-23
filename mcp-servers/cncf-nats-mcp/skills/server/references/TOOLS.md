# Tools

25 tools. Argument shapes come from each tool's JSON schema in `tools/list` —
this file is for *choosing* between them and reading their output.

## Diagnostics

These sample twice across `sample_ms` (default 2000, clamped to 200–15000) and
report change, not a snapshot.

| Tool | Use it when |
|---|---|
| `nats_diagnose` | You do not yet know what is wrong. Returns `findings[]` with `severity` (`critical`/`warning`/`info`), a stable `code`, `evidence`, and `remedy`; plus `capability` describing what could be checked. Pass `streams` / `consumers` (`"stream/consumer"`) / `buckets` to scope it, or nothing to use the best available discovery. |
| `jetstream_stream_rate` | "Is this stream actually moving?" Rates come from the **sequence** delta, so retention discarding from the tail cannot mask live ingest. `stored_delta` goes negative when retention outpaces publishers. |
| `jetstream_consumer_lag` | "Is this consumer keeping up?" Returns a `verdict` plus an `interpretation` sentence. |
| `nats_check_access` | "What am I allowed to touch?" Probes streams and buckets, returns granted/denied with the grant key to widen. |

`jetstream_consumer_lag` verdicts:

- `caught-up` — nothing pending.
- `draining` — backlog shrinking; `drain_estimate` says how long.
- `working` — backlog steady with messages in flight; keeping pace exactly.
- `falling-behind` — backlog grew during sampling. Will not recover alone.
- `stalled` — messages pending, none in flight, count not moving. Nothing is
  fetching.

Finding codes you may see:

| Code | Meaning |
|---|---|
| `unbounded-stream` | No `max_age`, `max_msgs` or `max_bytes`. Grows until the disk fills. `critical` when ingesting; carries `time_to_fill_estimate` when a limit is known. Needs monitoring. |
| `stream-no-pruning-observed` | Binding-level **inference**: `first_sequence` has never advanced, so nothing has ever been discarded. Confirm with `nats stream info`. |
| `stream-no-consumers` | Taking messages with nothing reading. Normal for replay/audit streams and for core-NATS live paths — check retention is bounded if so. |
| `consumer-backlog-growing` | `num_pending` rose during sampling. |
| `consumer-ack-saturated` | `num_ack_pending` near `max_ack_pending`; at 100% delivery stops entirely. |
| `consumer-redeliveries` | Messages delivered and never acked — a failing handler, or `ack_wait` shorter than real handling time. |
| `consumer-max-deliver-risk` | Redeliveries against a finite `max_deliver`: those messages are on a path to being dropped. |
| `consumer-idle-with-backlog` | Pending work, nothing pulling, count flat. Usually a stopped worker. |
| `kv-bucket-unbounded-growth` | Key count growing with no TTL. Fine for a fixed-key config store, wrong for a cache or log. |
| `slow-consumers` | The server has cut clients off for reading too slowly. Those clients **lost messages**. |
| `jetstream-storage-pressure` | File storage near its limit; publishes start failing at 100%. |
| `server-unhealthy` | The server's own `/healthz` is not `ok`. |

## Server-wide

Require the optional monitoring endpoint. Without it each returns
`monitor-not-configured` **with the steps to enable it** — surface those rather
than treating it as an error.

| Tool | Returns |
|---|---|
| `nats_server_info` | Version, uptime, health, connection/subscription counts, slow-consumer drops, memory/CPU, JetStream usage against limits. |
| `nats_server_streams` | Every stream: retention config, live state, `bounded` flag, `kv_bucket` when it backs one, and consumers. **The only enumeration available** — the binding has no list call. |
| `nats_connections` | Connected clients with subscription counts and `pending_bytes` (rising = the slow-consumer precursor). |

## Core NATS

| Tool | Notes |
|---|---|
| `nats_publish` | Fire-and-forget. Resolves on write to the connection, **not** on delivery. Subject must be literal. |
| `nats_request` | Request/reply. `no-responders` is immediate and distinct from `timeout`; retrying fails identically until a responder exists. |

## JetStream

| Tool | Notes |
|---|---|
| `jetstream_publish` | Durable, returns stream + sequence. `msg_id` gives idempotency within the stream's duplicate window. |
| `jetstream_scan` | **Preferred read.** No consumer, no acks. Sequences may gap where messages sit on subjects outside the grant. |
| `jetstream_get_message` | One message by sequence. Non-destructive. |
| `jetstream_stream_info` | Configured subjects, counts, first/last sequence, consumer count. No retention config — that is a monitoring-only field. |
| `jetstream_list_subjects` | Subjects the stream *holds* (vs `stream_info`'s configured patterns), with counts. Capped at 1000. |
| `jetstream_consumer_info` | Filters, provisioned limits, live counters. Check before sizing a fetch. |
| `jetstream_fetch` | **Mutates consumer state.** `settle: "ack"` consumes permanently; `"none"` stalls for `ack_wait`. Above `max_request_batch` the fetch is refused outright, not truncated. |

## Key/Value

| Tool | Notes |
|---|---|
| `kv_get` | `key-not-found` covers absent, deleted and purged alike. |
| `kv_put` | Last-write-wins. Will clobber a concurrent writer. |
| `kv_create` | Fails rather than overwriting. |
| `kv_update` | Compare-and-swap. A conflict returns `current_revision`, so retry needs no re-read. Use this for read-modify-write. |
| `kv_delete` / `kv_purge` | Tombstone (history kept) / history destroyed, irreversible. |
| `kv_keys` | Capped at 1000; `truncated: true` means narrow the filter, not that the bucket is small. |
| `kv_history` | Every retained revision oldest-first, including tombstones. Depth is the bucket's `history`. |
| `kv_status` | Values, history depth, TTL, size. |

## Payloads

Bodies round-trip as `payload`/`value` (UTF-8) or `payload_base64`/
`value_base64` (anything else). Results over 64 KiB are truncated and say so
with `truncated: true` — a clipped payload is always distinguishable from a
short one.
