# Golden Kafka templates for Cosmonic Desktop

Clone-and-customize starting points for building Kafka workloads on Cosmonic
(`cosmonic:kafka@0.3.0` on wasmCloud 2.8), in the style of
[mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
each template is a self-contained project — source, vendored WIT, a Cosmonic
Desktop `workload.yaml`, and a Kubernetes `deploy/workload-deployment.yaml`.

The same four patterns exist in both languages:

```
templates/
  rust/   http-kafka-producer | kafka-handler-consumer | kafka-pull-service | kafka-transactional
  go/     http-kafka-producer | kafka-handler-consumer | kafka-pull-service | kafka-transactional
```

Every design guideline and number cited below was measured in this repo's
k8s performance campaign (`../k8s-perf/RESULTS/REPORT.md`).

## Which pattern do I want?

| Template | Shape | Delivery guarantee | Use when… | Don't use when… |
|---|---|---|---|---|
| **http-kafka-producer** | HTTP request → produce (`send` / `send-batch`) | broker ack per record (`acks` configurable) | An API, webhook, or UI event needs to land records on a topic; ingest gateways; request-scoped writes | You produce continuously at high rate — an open-per-request producer caps near ~10 req/s per instance; use batching (30k+ records/s measured) or a Service holding one producer |
| **kafka-handler-consumer** | Host pushes each record to `handle` (push mode) | at-least-once; DLQ on `permanent` errors | Simple per-record processing with no cross-record state: filters, validators, notifiers, sinks. The host owns the consumer, offsets, retries, and DLQ — least code, hardest to hold wrong | The work per record must produce to Kafka (client bootstrap per record measured ~1200× a plain dispatch — use the pull service), you need batching, cross-record state, or your own commit policy |
| **kafka-pull-service** | Long-running Service owns consumer + producer, batches, commits explicitly | at-least-once; your own DLQ routing | The workhorse: consume→transform→produce pipelines, aggregation windows, anything needing batching (`send-batch`), custom offset/commit policy, or a long-lived producer | You need exactly-once (see transactional) or truly trivial per-record work with no produce (handler is less code) |
| **kafka-transactional** | Pull service + transactions: outputs and input offsets commit atomically | exactly-once (read-process-write) | Money, inventory, dedup-sensitive enrichment — anywhere a replayed or half-applied batch is unacceptable and downstream reads `read_committed` | Throughput matters more than duplicates (txn round trips cost); side effects leave Kafka (a DB write isn't covered by the transaction — then at-least-once + idempotent writes is the honest design) |

### Rules that apply to every pattern

- **`topics` in the workload's kafka config is both the grant and (for
  handlers) the subscription list.** Grant exactly what the workload touches.
- **The host pins whatever the manifest sets** (`bootstrap.servers`, creds,
  `group.id`, `transactional.id`) — guests cannot override it. Put policy in
  the manifest, not the code.
- **Set an explicit, stable `group.id`** — the derived default embeds the
  workload identity and collides/reshuffles in ways you don't want.
- **Handlers: always configure `dead-letter.topic`.** A `permanent` error
  without a DLQ stalls the partition forever; a deterministically trapping
  record wedges it either way — treat malformed input as `permanent`, never
  panic.
- **Never open a Kafka client per record.** ~100 ms each (DNS + TCP +
  metadata), measured ~1200× the cost of a plain handler dispatch — and at
  high concurrency client churn can exhaust host fds/threads
  (`CritSysRes`), affecting neighbor workloads.
- **On Kubernetes, add `broker.address.family: v4`** (and check cluster DNS
  health): a degraded AAAA path silently added 12 s to every client
  bootstrap in testing.
- **Scaling:** handler & pull throughput scale with partitions × replicas.
  `poolSize`/`maxConcurrency` only affect HTTP-triggered (P3) components —
  and don't combine them on client-per-request producers (measured 14×
  regression); prefer `poolSize` + batching.

## Go vs Rust

Both languages were benchmarked pattern-by-pattern on Kubernetes — the comparison table
(throughput, memory, trade-offs per template) lives in `../k8s-perf/RESULTS/REPORT.md`.
Short version: pick by pattern first; Go is within ~30% of Rust for the service patterns and
HTTP producers (budget 2–3× memory), while high-rate handler components (thousands of
records/s on one component) need Rust.

### Toolchains

The Rust templates use `wit-bindgen 0.58` async (WASI P3) and compile with
stock `cargo build --target wasm32-wasip2`. The Go templates target the same
worlds via standard Go + `componentize-go` (TinyGo tops out at WASI P2 and
cannot bind `cosmonic:kafka@0.3.0`'s async interfaces) — see `go/README.md`
for toolchain status and versions.
