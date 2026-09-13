# Golden Kafka templates for Cosmonic

Clone-and-customize starting points for building Kafka workloads on Cosmonic
(`cosmonic:kafka@0.5.0` on Cosmonic Control 0.11.0 and wasmCloud 2.9), in the
style of
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

Start with **kafka-handler-consumer** for serverless event processing. The host
keeps the Kafka consumer and group membership stable while component instances
scale with assigned partition work and can return to zero when idle.

Use a Service only when the component must own session state or a transaction.
Do not choose the pull template just to publish output: a handler can import the
binding-scoped producer without owning a Kafka client.

The guidance below follows the 0.5.0 operator contract and broker-backed tests.
The templates were built with `wash 2.7.0` and tested live against Control
0.11.0. Use Rust 1.85 or newer to build them.

## Which pattern do I want?

| Template | Shape | Delivery guarantee | Use when… | Don't use when… |
|---|---|---|---|---|
| **kafka-handler-consumer** (recommended) | Host pushes partition-ordered batches to elastic component instances | at-least-once; DLQ on `permanent` errors | Serverless event processing, including consume→transform→produce. The host owns polling, offsets, retries, DLQ delivery, and stable group membership while guest instances scale independently | You need direct session control such as assign, pause, seek, rebalance events, or arbitrary commits; or output and offsets must commit in one Kafka transaction |
| **http-kafka-producer** | HTTP request → binding-scoped `send` / `send-batch` | broker ack per record (`acks` configurable) | An API, webhook, or UI event needs to publish records; ingest gateways; request-scoped writes | The workload is driven by Kafka records rather than HTTP |
| **kafka-pull-service** | Long-running Service owns a pull-consumer session and uses a host-owned producer | at-least-once; your own DLQ routing | You need subscribe/assign control, rebalance events, pause/resume/seek, guest-controlled pull pace, or explicit commits | Ordinary elastic event processing; the handler is less stateful and scales without changing group membership |
| **kafka-transactional** | Pull service + transactions: outputs and input offsets commit atomically | exactly-once (read-process-write) | Money, inventory, dedup-sensitive enrichment — anywhere a replayed or half-applied batch is unacceptable and downstream reads `read_committed` | Throughput matters more than duplicates (txn round trips cost); side effects leave Kafka (a DB write isn't covered by the transaction — then at-least-once + idempotent writes is the honest design) |

## What changed in 0.5.0

Kafka clients are binding-scoped capabilities. Producer functions are called
directly; there is no guest-owned producer resource or `open`. A pull consumer
uses `consumer.open()` with no arguments. Broker, credential, group, limits,
and topic policy come only from `hostInterfaces` and cannot be selected by the
guest. Transactional publishing now uses the separate `transaction` interface,
and its sends and offset enlistment run through the returned transaction
resource.

## Where the broker and credentials are configured

In the workload's own `cosmonic:kafka` entry under `hostInterfaces`, which is
what every manifest here shows. That entry is the binding: `bootstrap.servers`,
`security.protocol`, the SASL or TLS material, the topic grant, and the
handler keys all live there, and the component never sees a broker address in
its code.

The binding is also the security boundary. Properties that load host code,
read host files, run commands, or disable transport protections remain
host-owned and are rejected in workload configuration.

Secrets do not belong inline. `config` is for plain values, `configFrom` pulls
a ConfigMap, and `secretFrom` pulls a Secret; the three merge in that order,
so a password reaches librdkafka without appearing in the manifest:

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

The component cannot pass connection or client properties at runtime, so it
cannot redirect itself to another broker or substitute another credential.

### Rules that apply to every pattern

- **`handler.topics` selects the subscription; `topics` grants access.** The
  grant must contain every subscription, dead-letter topic, and topic named by
  producer calls. Grant exactly what the workload touches.
- **The binding fixes Kafka authority.** Broker, credentials, groups, client
  policy, transaction IDs, and topic grants are unavailable as guest inputs.
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
  is not spelled `consumer.group.id`, which configures a pull consumer.
- **Handlers: always configure `dead-letter.topic`.** It is required, for the
  same reason: past the redelivery cap a permanently failed record has to go
  somewhere, and the alternative is stalling the partition. Treat malformed
  input as `permanent`, never panic.
- **Producer calls reuse the binding's native client.** Do not open a pull
  consumer per invocation; a pull consumer is a stateful session intended for
  a long-lived Service.
- **Scale handler compute with the component pool.** Per replica, useful
  concurrency is `min(assigned partitions, 64, poolSize × maxConcurrency)`.
  Growing the pool does not add group members or rebalance Kafka. Increase
  workload replicas for availability or to place group members on more hosts.
- **Do not keep required state in a handler instance.** Calls may land on
  different instances, and idle instances may be reclaimed. Store durable
  state externally and make side effects idempotent.
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

## Operational limits

Native Kafka allocations live outside a component's Wasm memory limit. The
plugin therefore bounds clients, queues, streams, and handler buffers. Notable
defaults and ceilings are:

| Resource | Limit |
|---|---|
| Producer clients | 64 ordinary or transactional clients |
| Handler calls | 64 in flight per component |
| Handler batch | 100 records by default; configurable from 1 to 10,000 |
| Handler record buffers | 128 MiB shared across partitions |
| Pull consumers | 64 per component |
| Pull-consumer buffer | 64 MiB and 512 records |
| Concurrent producer streams | 32 per component |
| Producer stream buffer | 64 MiB and 256 records; about 1 MiB per record |
| Producer queue | 32,768 KiB by default; 102,400 KiB maximum per client property |

Queue limits are not eager allocations, but many bindings can still reserve a
large possible footprint. Grant only the interfaces a workload needs and use
smaller queue limits on high-density hosts.

## Toolchain

The templates use `wit-bindgen 0.58` async (WASI P3) and compile with stock
`cargo build --target wasm32-wasip2`, which is also what each template's
`.wash/config.yaml` runs under `wash build`.

Rust only for now. `cosmonic:kafka@0.5.0` declares every function `async
func`, which needs a toolchain that can bind the p3 async ABI — TinyGo tops
out at WASI P2 and cannot. Go via `componentize-go` is the candidate; a Go
set was started and removed here rather than shipped half-built, since a
template that does not compile is worse than one that does not exist.
