# kafka-handler-consumer (Rust)

The recommended starting point for serverless Kafka processing. The host owns
the Kafka consumer and pushes batches of records to the component, much like
a function triggered by a queue. The host keeps consumer-group membership
stable while component instances scale with partition work and drop to zero
when idle. See the [pattern guide](../../README.md) for when to choose this
template over the other three.

## Build

You need Rust 1.88 or newer with the `wasm32-wasip2` target
(`rustup target add wasm32-wasip2`) and `wash` 2.7 or newer. This template
was built with `wash 2.7.0` and tested against Cosmonic Control 0.11.0 with
`cosmonic:kafka@0.5.0`.

The `cosmonic:kafka@0.5.0` WIT (the interface definition the Rust bindings
are generated from) comes from a registry, not from this repository, and
`wash` has to be told which registry serves the `cosmonic` namespace. Export
that mapping once per shell. `wash build` fetches WIT too, so prefixing a
single `wash wit fetch` is not enough:

```shell
export WKG_CONFIG_FILE="$PWD/../../wkg-registries.toml"
wash build    # fetches the WIT, then runs .wash/config.yaml
# component: target/wasm32-wasip2/release/kafka_handler_consumer.wasm
```

If you started from `wash new`, the project lives outside this repository and
`../../wkg-registries.toml` does not exist. Download the file into the
project and point at it there:

```shell
curl -fsSLO https://raw.githubusercontent.com/cosmonic-labs/awesome-cosmonic/main/components/kafka/wkg-registries.toml
export WKG_CONFIG_FILE="$PWD/wkg-registries.toml"
```

`wkg.lock` pins the exact WIT versions, much like a `package-lock.json`.
`wit/deps/` is gitignored, so the first build populates it. Once it exists,
`cargo build --release` produces the same component. `.wash/config.yaml` only
names that command and where its output lands, so tooling can find the
artifact without being told.

To stop exporting the variable, merge the entries from
[`wkg-registries.toml`](../../wkg-registries.toml) into
`~/.config/wasm-pkg/config.toml` once.

## Deploy

- **Cosmonic Desktop**: apply `workload.yaml` with the
  `cosmonic_workload_apply` MCP tool or the daemon's workload API. Desktop
  also ships this template as the `rust-kafka-handler-consumer` project
  starter.
- **Kubernetes** (Cosmonic Control or the wasmCloud runtime operator): run
  `kubectl apply -f deploy/workload-deployment.yaml`.

Both manifests point at the component published from this template
(`ghcr.io/cosmonic-labs/kafka-templates/kafka-handler-consumer:latest`; pin a
`sha-<commit>` tag if you need fixed bytes). Once you change the source,
build it, push it to your own registry and replace the image reference.

The broker, subscription, group, grant and dead-letter values in the
manifests are placeholders; set them before you apply. Create the topics
first (`demo.events` and `demo.events.dlq` as shipped) unless your broker
creates topics automatically.

The workload's `cosmonic:kafka` entry under `hostInterfaces` holds the
connection: broker, credentials, the topic grant and `handler.group.id`. The
component cannot supply or override these values. Put the credential behind
`secretFrom` rather than inlining it. The
[pattern guide](../../README.md#where-the-broker-and-credentials-are-configured)
covers the binding and topic-grant rules.

## Scaling

Useful concurrency per workload replica is bounded by three things: the
partitions assigned to it, the plugin's ceiling of 64 calls, and
`poolSize × maxConcurrency`. Growing the pool does not create Kafka group
members or trigger a rebalance. Instances are created on demand, and
`reclaimWindowSeconds` with `reclaimMinInstances: 0` lets the pool return to
zero after a burst. Keep `maxConcurrency: 1` unless the handler code is safe
for overlapping calls on one instance.

Scale-to-zero applies to component instances only. The host keeps its native
Kafka consumer, connection and group membership alive, so new records don't
wait for a rebalance or a pod start.

Raising `spec.replicas` is different: it adds a Kafka group member and can
move partitions. Use replicas for host-level availability or when you need
more group members. Use the component pool for elastic compute on the
partitions a replica already holds.

## Delivery

Each call receives a non-empty, offset-ordered batch from one partition.

- Return `Ok(None)` after handling the whole batch.
- If a later record fails after earlier ones succeeded, return
  `Ok(Some(offset))` with the offset of the last record that succeeded. The
  host keeps that progress and redelivers the rest.
- Return `Transient` when the first unhandled record may succeed on retry.
- Return `Permanent` when it should go straight to `dead-letter.topic`.

Delivery is at least once. A crash can repeat a side effect before its offset
is committed, so handlers must be idempotent. Don't rely on instance-local
state: calls can land on different instances, and idle instances are
reclaimed.

The sample treats tombstones (records with no value) as handled and sends
values that are not valid UTF-8 to the dead-letter topic. Replace the block
marked `Replace this with idempotent processing` in `src/lib.rs` with your
own logic, and keep its partial-progress behavior.

## Publish from a handler

A handler can also import `cosmonic:kafka/producer@0.5.0` to consume,
transform and produce. Add the producer import to `wit/world.wit`, add
`producer` to the manifest's Kafka `interfaces`, and include every output
topic in `topics`. The host-owned producer stays available across elastic
handler instances, so this does not need the pull-service template.

Output produced this way is at least once and may be duplicated after a
crash. Use the [transactional template](../kafka-transactional/) when outputs
and consumed offsets must commit atomically.
