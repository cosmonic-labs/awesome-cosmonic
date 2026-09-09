# Golden Kafka templates for Cosmonic Desktop

Clone-and-customize starting points for building Kafka workloads on Cosmonic
(`cosmonic:kafka@0.3.0` on wasmCloud 2.8), in the style of
[mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
each template is a self-contained project — source, a `wkg.lock` pinning the
WIT it fetches, a Cosmonic Desktop `workload.yaml`, and a Kubernetes
`deploy/workload-deployment.yaml`.

Four patterns, in Rust:

```
templates/kafka/
  rust/   http-kafka-producer | kafka-handler-consumer | kafka-pull-service | kafka-transactional
```

Every design guideline and number cited below was measured on Kubernetes
against a real broker, not estimated.

## Which pattern do I want?

| Template | Shape | Delivery guarantee | Use when… | Don't use when… |
|---|---|---|---|---|
| **http-kafka-producer** | HTTP request → produce (`send` / `send-batch`) | broker ack per record (`acks` configurable) | An API, webhook, or UI event needs to land records on a topic; ingest gateways; request-scoped writes | You produce continuously at high rate — an open-per-request producer caps near ~10 req/s per instance; use batching (30k+ records/s measured) or a Service holding one producer |
| **kafka-handler-consumer** | Host pushes each record to `handle` (push mode) | at-least-once; DLQ on `permanent` errors | Simple per-record processing with no cross-record state: filters, validators, notifiers, sinks. The host owns the consumer, offsets, retries, and DLQ — least code, hardest to hold wrong | The work per record must produce to Kafka (client bootstrap per record measured ~1200× a plain dispatch — use the pull service), you need batching, cross-record state, or your own commit policy |
| **kafka-pull-service** | Long-running Service owns consumer + producer, batches, commits explicitly | at-least-once; your own DLQ routing | The workhorse: consume→transform→produce pipelines, aggregation windows, anything needing batching (`send-batch`), custom offset/commit policy, or a long-lived producer | You need exactly-once (see transactional) or truly trivial per-record work with no produce (handler is less code) |
| **kafka-transactional** | Pull service + transactions: outputs and input offsets commit atomically | exactly-once (read-process-write) | Money, inventory, dedup-sensitive enrichment — anywhere a replayed or half-applied batch is unacceptable and downstream reads `read_committed` | Throughput matters more than duplicates (txn round trips cost); side effects leave Kafka (a DB write isn't covered by the transaction — then at-least-once + idempotent writes is the honest design) |

### Rules that apply to every pattern

- **`handler.topics` and `topics` are different keys.** `handler.topics` is
  what the host subscribes to and dispatches from; `topics` is the grant for
  the component's own `producer`/`consumer` calls. They are separate because a
  handler that reads one topic and writes another cannot express that with one
  key. Grant exactly what the workload touches; a pure handler needs no
  `topics` at all.
- **The host pins whatever the manifest sets** (`bootstrap.servers`, creds,
  the topic grant, `handler.group.id`, `transactional.id`) — guests cannot
  override it. Put policy in the manifest, not the code.
- **Some keys are the host's and are refused to a workload:** anything that
  loads native code (`plugin.library.paths`, `ssl.engine.location`,
  `ssl.providers`), reads a host file by path (the `ssl.*.location` keys,
  `https.ca.location`, `sasl.kerberos.keytab`), runs a host command or reaches
  a host-chosen URL (`sasl.kerberos.kinit.cmd`,
  `sasl.oauthbearer.token.endpoint.url`), or weakens and floods the host
  (`enable.ssl.certificate.verification`,
  `ssl.endpoint.identification.algorithm`, `debug`,
  `statistics.interval.ms`). Credentials themselves are yours to set — pass
  TLS material inline as `ssl.ca.pem` and friends, not as file paths.
- **Handlers: `handler.group.id` is required and never derived.** Nothing the
  host can see is scoped to an installation, so a derived group would be
  identical across two installations of the same manifest and a shared broker
  would split the records between them. The deploy fails without it. Note it
  is not spelled `group.id`: that key pins the group for a consumer the guest
  opens itself, which is a different thing.
- **Handlers: always configure `dead-letter.topic`.** It is required, for the
  same reason: past the redelivery cap a permanently failed record has to go
  somewhere, and the alternative is stalling the partition. Treat malformed
  input as `permanent`, never panic.
- **Never open a Kafka client per record.** ~100 ms each (DNS + TCP +
  metadata), measured ~1200× the cost of a plain handler dispatch — and at
  high concurrency client churn can exhaust host fds/threads
  (`CritSysRes`), affecting neighbor workloads.
- **On Kubernetes, add `broker.address.family: v4`** (and check cluster DNS
  health): a degraded AAAA path silently added 12 s to every client
  bootstrap in testing.
- **Scaling:** handler & pull throughput scale with partitions × replicas —
  that product is the ceiling, since a partition is only ever assigned to one
  group member. Handler dispatch runs one loop per assigned partition, so
  `poolSize` and `maxConcurrency` do matter there now: they decide how many of
  those concurrent calls land on warm instances and how many share one. Don't
  combine them on client-per-request producers (measured 14× regression);
  prefer `poolSize` + batching.
- **Handlers get a batch, not a record.** `handle` is called with up to
  `handler.batch.size` records (default 100, max 10000) from one partition,
  in offset order, capped around 1 MiB. Process in order and return
  `Ok(Some(<last handled offset>))` when one fails part way through: the host
  keeps that progress and redelivers from the failing record, which then
  arrives alone and gets a verdict of its own. Returning an error instead
  throws away the whole batch's work.
- **A slow handler wants `max.poll.interval.ms`, not a smaller pool.** The
  per-call deadline derives from it (the interval less a minute, so ten
  minutes by default), and the deadline covers the whole batch. Raising it is
  the one knob; the host serves the group's poll timer independently, so a
  long call no longer risks eviction.
- **Rebalancing is `cooperative-sticky` by default,** so adding or removing
  replicas moves only the partitions that change hands. Every member of a
  group must agree on the strategy, so don't mix it across deployments that
  share a `handler.group.id`.

## Toolchain

The templates use `wit-bindgen 0.58` async (WASI P3) and compile with stock
`cargo build --target wasm32-wasip2`, which is also what each template's
`.wash/config.yaml` runs under `wash build`.

Rust only for now. `cosmonic:kafka@0.3.0` declares every function `async
func`, which needs a toolchain that can bind the p3 async ABI — TinyGo tops
out at WASI P2 and cannot. Go via `componentize-go` is the candidate; a Go
set was started and removed here rather than shipped half-built, since a
template that does not compile is worse than one that does not exist.
