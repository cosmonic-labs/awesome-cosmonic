# Golden Kafka templates for Cosmonic

Clone-and-customize starting points for building Kafka workloads on Cosmonic
(`cosmonic:kafka@0.3.0` on wasmCloud 2.8), in the style of
[mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
each template is a self-contained project — source, a `wkg.lock` pinning the
WIT it fetches, a Cosmonic `workload.yaml`, and a Kubernetes
`deploy/workload-deployment.yaml`.

Every manifest points at a prebuilt component published from this directory,
so a pattern can be deployed and watched before any of it is built locally.

To start from one of these without cloning the repository:

```console
wash new https://github.com/cosmonic-labs/awesome-cosmonic \
  --subfolder components/kafka/rust/kafka-handler-consumer
```

Four patterns, in Rust:

```
components/kafka/
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

## Where the broker and credentials are configured

In the workload's own `cosmonic:kafka` entry under `hostInterfaces`, which is
what every manifest here shows. That entry is the binding: `bootstrap.servers`,
`security.protocol`, the SASL or TLS material, the topic grant, and the
handler keys all live there, and the component never sees a broker address in
its code.

The plugin claims none of those keys deliberately. It owns the ones that would
hand the *host process* a capability (see the rule below), and leaves the
connection, the credential and the topics to the workload — which is what lets
two workloads on one host reach different clusters as different principals.

Secrets do not belong inline. `config` is for plain values, `configFrom` pulls
a ConfigMap, and `secretFrom` pulls a Secret; the three merge in that order,
so a password reaches librdkafka without appearing in the manifest:

```yaml
hostInterfaces:
  - namespace: cosmonic
    package: kafka
    version: 0.3.0
    interfaces: [producer, types]
    config:
      bootstrap.servers: my-kafka.kafka.svc.cluster.local:9092
      security.protocol: SASL_SSL
      sasl.mechanism: SCRAM-SHA-512
      topics: "demo.events"
    secretFrom:
      - kafka-credentials      # sasl.username, sasl.password, ssl.ca.pem, ...
```

Whatever that entry sets wins over anything the component passes to
`producer.open`/`consumer.open`, so a guest cannot redirect itself at another
broker or substitute its own credential. A guest's own config is only consulted
for keys the entry leaves unset.

**An operator can take this over.** A host's plugin configuration accepts the
same `config`/`configFrom`/`secretFrom`, plus named `bindings` a workload
selects by label. Two settings decide how much a workload may still say:
`hostOwnedKeys` claims additional keys for the host, and `workloadConfig`
(`deny` by default) fails the deploy of a workload that sets a host-owned key
or widens a grant declared for it. A platform team that owns the cluster puts
the broker and credentials there once, and workloads name only the label and
the topics they need. These templates take the self-contained route instead,
so each runs on its own.

### Rules that apply to every pattern

- **`handler.topics` and `topics` are different keys.** `handler.topics` is
  what the host subscribes to and dispatches from; `topics` is the grant for
  the component's own `producer`/`consumer` calls. They are separate because a
  handler that reads one topic and writes another cannot express that with one
  key. Grant exactly what the workload touches; a pure handler needs no
  `topics` at all.
- **The binding entry beats the guest** (`bootstrap.servers`, creds, the topic
  grant, `handler.group.id`, `transactional.id`) — a component cannot override
  what the manifest sets. Put policy in the manifest, not the code.
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
