# kafka-pull-service (Rust)

A long-running Service that owns a Kafka consumer session, transforms each
record and produces the result to another topic. A Service exports
`wasi:cli/run` and runs for the life of the workload, much like a
container's main process. Use it when your code needs direct control of the
session. See the [pattern guide](../../README.md) for when to choose this
template over the other three.

## When to use this pattern

Prefer the [handler template](../kafka-handler-consumer/) for ordinary
elastic processing. Use this Service when your code needs direct session
operations: assign, pause, seek, rebalance events, its own pull pace, or
arbitrary commits.

This Service owns a long-lived consumer session and does not scale to zero.
Adding replicas adds Kafka group members and may move partitions.

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
# component: target/wasm32-wasip2/release/kafka_pull_service.wasm
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

- **Cosmonic Desktop**: create a project from the `rust-kafka-pull-service`
  starter, which runs the component as the workload's `service`.
- **Kubernetes** (Cosmonic Control or the wasmCloud runtime operator): run
  `kubectl apply -f deploy/workload-deployment.yaml`.

The manifests point at the component published from this template
(`ghcr.io/cosmonic-labs/kafka-templates/kafka-pull-service:latest`; pin a
`sha-<commit>` tag if you need fixed bytes). Once you change the source,
build it, push it to your own registry and replace the image reference.

The broker, topic, group and environment values in the manifests are
placeholders; set them before you apply. Create the topics first
(`demo.events`, `demo.enriched` and `demo.events.dlq` as shipped) unless your
broker creates topics automatically.

The workload's `cosmonic:kafka` entry under `hostInterfaces` holds the
connection: broker, credentials, the topic grant and `consumer.group.id`. The
component calls `consumer.open()` with no arguments and cannot supply or
override these values. Put the credential behind `secretFrom` rather than
inlining it. The
[pattern guide](../../README.md#where-the-broker-and-credentials-are-configured)
covers the binding and topic-grant rules.

## Configuration

The component reads its topics and batch size from environment variables set
under `localResources.environment.config` in the manifest:

| Variable | Default | Purpose |
|---|---|---|
| `IN_TOPIC` | `input` | Topic to consume |
| `OUT_TOPIC` | `output` | Topic to produce transformed records to |
| `DLQ_TOPIC` | `input.dlq` | Topic for records that fail to transform |
| `BATCH_SIZE` | `1` | Output records to collect before flushing (capped at 100) |

`BATCH_SIZE` defaults to 1 for bounded latency. A larger value waits until
that many output records arrive or the record stream ends; there is no batch
timer. Raise it only for a steady stream where throughput matters more than
the latency of a partial batch.

## Delivery

Delivery is at least once. The Service commits stored input positions only
after every output delivery and every per-partition commit succeeds. Any
failure exits the Service, and its supervisor restarts it from the last
committed offsets. Records that fail to transform go to `DLQ_TOPIC`; pending
output is flushed before that record is committed.
