# Golden Kafka templates for Cosmonic

Clone-and-customize starting points for Kafka workloads on Cosmonic, built on
the `cosmonic:kafka@0.5.0` interface. Cosmonic Control 0.11.0 (wasmCloud 2.9)
ships the Kafka plugin that serves it, and Cosmonic Desktop compiles in the
same plugin. Like
[mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs),
each template is a self-contained project: the source, a `wkg.lock` that pins
the WIT it fetches, a Cosmonic `workload.yaml`, and a Kubernetes
`deploy/workload-deployment.yaml`.

Every manifest points at a prebuilt component published from this directory,
so you can deploy a pattern and watch it work before you build anything.

To start from one of these without cloning the repository:

```shell
wash new https://github.com/cosmonic-labs/awesome-cosmonic \
  --subfolder components/kafka/rust/kafka-handler-consumer
```

There are four patterns, all in Rust:

```text
components/kafka/
  rust/   http-kafka-producer | kafka-handler-consumer | kafka-pull-service | kafka-transactional
```

Start with **kafka-handler-consumer** for serverless event processing. The host
keeps the Kafka consumer and its consumer-group membership stable, while
component instances scale with the partition work assigned to them and drop
to zero when idle.

Choose a Service only when the component must own a consumer session or a
transaction. A Service is a long-running component that exports
`wasi:cli/run` and runs for the life of the workload, much like a container's
main process. Don't pick the pull template just to publish output: a handler
can import the binding's producer without owning a Kafka client.

The guidance below follows the 0.5.0 operator contract and broker-backed
tests. The templates were built with `wash 2.7.0` and tested live against
Control 0.11.0. Building them needs Rust 1.88 or newer (the floor set by the
pinned `wit-bindgen` dependencies).

## Which pattern do I want?

| Template | Shape | Delivery guarantee | Use when | Don't use when |
|---|---|---|---|---|
| **kafka-handler-consumer** (recommended) | The host pushes partition-ordered batches to elastic component instances | At least once; dead-letter topic (DLQ) on `permanent` errors | Serverless event processing, including consume, transform and produce. The host owns polling, offsets, retries, DLQ delivery and stable group membership, while guest instances scale on their own | You need direct session control (assign, pause, seek, rebalance events, arbitrary commits), or output and offsets must commit in one Kafka transaction |
| **http-kafka-producer** | An HTTP request calls the binding's `send` or `send-batch` | Broker acknowledgement per record (`acks` is configurable) | An API, webhook or UI event needs to publish records; ingest gateways; request-scoped writes | The workload is driven by Kafka records rather than HTTP |
| **kafka-pull-service** | A long-running Service owns a pull-consumer session and uses the host-owned producer | At least once; you route your own DLQ | You need subscribe or assign control, rebalance events, pause, resume or seek, guest-paced pulls, or explicit commits | Ordinary elastic event processing; the handler holds less state and scales without changing group membership |
| **kafka-transactional** | A pull Service plus transactions: outputs and input offsets commit atomically | Exactly once (read-process-write) | Money, inventory, dedup-sensitive enrichment: anywhere a replayed or half-applied batch is unacceptable and downstream consumers read with `read_committed` | Throughput matters more than duplicates (each transaction costs round trips), or side effects leave Kafka. A database write is not covered by the transaction, so at-least-once delivery with idempotent writes is the honest design there |

## What changed in 0.5.0

Kafka clients are binding-scoped capabilities. A binding is the workload's
`cosmonic:kafka` entry under `hostInterfaces`; it fixes which broker,
credential, group and topics the component may use. Producer functions are
called directly, with no guest-owned producer resource and no `open`. A pull
consumer calls `consumer.open()` with no arguments. Broker, credential, group,
limits and topic policy come only from `hostInterfaces`, and the guest cannot
choose them. Transactional publishing uses the separate `transaction`
interface, and its sends and offset enlistment go through the transaction
resource that `begin` returns.

## Where the broker and credentials are configured

In the workload's own `cosmonic:kafka` entry under `hostInterfaces`, which
every manifest here shows. That entry is the binding: `bootstrap.servers`,
`security.protocol`, the SASL or TLS material, the topic grant and the handler
keys all live there. The component never sees a broker address in its code.

The binding is also the security boundary. Properties that load host code,
read host files, run commands or disable transport protections stay with the
host and are rejected in workload configuration.

Keep secrets out of the manifest. `config` holds plain values, `configFrom`
pulls a ConfigMap and `secretFrom` pulls a Secret. The three merge in that
order, so a password reaches librdkafka (the Kafka client library the host
uses) without appearing in the manifest:

```yaml
hostInterfaces:
  - namespace: cosmonic
    package: kafka
    version: 0.5.0
    interfaces: [producer]
    config:
      bootstrap.servers: my-kafka.kafka.svc.cluster.local:9092
      security.protocol: SASL_SSL
      sasl.mechanism: SCRAM-SHA-512
      topics: "demo.events"
    secretFrom:
      - name: kafka-credentials # sasl.username, sasl.password, ssl.ca.pem, ...
```

On Cosmonic Desktop a secret's key must be an environment-variable name, so
register it as `SASL_PASSWORD` (or `SASL_USERNAME`, and so on). Desktop maps
it to `sasl.password` before the Kafka client sees it.

The component cannot pass connection or client properties at runtime, so it
cannot redirect itself to another broker or swap in another credential.

### Rules that apply to every pattern

- **`handler.topics` selects the subscription; `topics` grants access.** The
  grant must contain every subscribed topic, the dead-letter topic, and every
  topic named in producer calls. Grant exactly what the workload touches.
- **The binding fixes Kafka authority.** Broker, credentials, groups, client
  policy, transaction IDs and topic grants are never guest inputs.
- **Some keys belong to the host and are refused to a workload:** anything
  that loads native code (`plugin.library.paths`, `ssl.engine.location`,
  `ssl.providers`), reads a host file by path (the `ssl.*.location` keys,
  `https.ca.location`, `sasl.kerberos.keytab`), runs a host command or reaches
  a host-chosen URL (`sasl.kerberos.kinit.cmd`,
  `sasl.oauthbearer.token.endpoint.url`), or weakens or floods the host
  (`enable.ssl.certificate.verification`,
  `ssl.endpoint.identification.algorithm`, `debug`,
  `statistics.interval.ms`). Credentials themselves are yours to set: pass TLS
  material inline as `ssl.ca.pem` and similar keys, not as file paths.
- **Handlers: `handler.group.id` is required and never derived.** Nothing the
  host can see is unique to one installation, so a derived group would be the
  same for two installations of the same manifest, and a shared broker would
  split the records between them. The deploy fails without it. Note the
  spelling: `consumer.group.id` configures a pull consumer, not a handler.
- **Handlers: always configure `dead-letter.topic`.** It is required for the
  same reason. Past the redelivery cap, a permanently failed record has to go
  somewhere, and the alternative is a stalled partition. Treat malformed input
  as `permanent`; never panic.
- **Producer calls reuse the binding's native client.** Don't open a pull
  consumer per invocation. A pull consumer is a stateful session meant for a
  long-lived Service.
- **Scale handler compute with the component pool.** Per replica, useful
  concurrency is `min(assigned partitions, 64, poolSize × maxConcurrency)`.
  Growing the pool does not add group members or trigger a Kafka rebalance.
  Add workload replicas for availability or to spread group members across
  more hosts.
- **Don't keep required state in a handler instance.** Calls may land on
  different instances, and idle instances may be reclaimed. Store durable
  state externally and make side effects idempotent.
- **Handlers get a batch, not a record.** `handle` receives up to
  `handler.batch.size` records (default 100, maximum 10,000) from one
  partition, in offset order, capped at about 1 MiB. Process them in order.
  If one fails part way through, return `Ok(Some(<last handled offset>))`:
  the host keeps that progress and redelivers from the failing record, which
  then arrives alone and gets a verdict of its own. Returning an error
  instead throws away the work done on the whole batch.
- **A slow handler needs a larger `max.poll.interval.ms`, not a smaller
  pool.** The per-call deadline is that interval minus one minute (ten
  minutes by default), and it covers the whole batch. The host keeps the
  group's poll timer alive on its own, so a long call does not risk eviction
  from the group.
- **Rebalancing is `cooperative-sticky` by default,** so adding or removing
  replicas moves only the partitions that change hands. Every member of a
  group must use the same strategy, so don't mix strategies across
  deployments that share a `handler.group.id`.

## Before you deploy

Point `bootstrap.servers` at your broker and create the topics each manifest
names, unless your broker creates topics automatically:

| Template | Topics |
|---|---|
| kafka-handler-consumer | `demo.events`, `demo.events.dlq` |
| http-kafka-producer | `demo.events` |
| kafka-pull-service | `demo.events`, `demo.enriched`, `demo.events.dlq` |
| kafka-transactional | `demo.events`, `demo.enriched` |

For a local broker on `127.0.0.1:9092`, for example:

```shell
kafka-topics --bootstrap-server 127.0.0.1:9092 --create --if-not-exists --topic demo.events
```

## Operational limits

Native Kafka allocations live outside a component's Wasm memory limit, so the
plugin bounds clients, queues, streams and handler buffers itself. Notable
defaults and ceilings:

| Resource | Limit |
|---|---|
| Producer clients | 64 ordinary or transactional clients per component |
| Handler calls | 64 in flight per component |
| Handler batch | 100 records by default; configurable from 1 to 10,000 |
| Handler record buffers | 128 MiB shared across partitions |
| Pull consumers | 64 per component |
| Pull-consumer buffer | 64 MiB and 512 records |
| Concurrent producer streams | 32 per component |
| Producer stream buffer | 64 MiB and 256 records; about 1 MiB per record |
| Producer queue | 32,768 KiB by default; 102,400 KiB maximum per client property |

Queue limits are not allocated up front, but many bindings can still reserve
a large possible footprint. Grant only the interfaces a workload needs, and
use smaller queue limits on high-density hosts.

## Toolchain

The templates use `wit-bindgen` 0.62 with async (WASI P3) bindings and
compile with a stock `cargo build --target wasm32-wasip2`. Each template's
`.wash/config.yaml` runs that same command under `wash build`.

The templates are Rust only for now. Every function in `cosmonic:kafka@0.5.0`
is an `async func`, which needs a toolchain that can bind the P3 async ABI.
TinyGo stops at WASI P2 and cannot. Go through `componentize-go` is the likely
path, but there is no Go template yet.
