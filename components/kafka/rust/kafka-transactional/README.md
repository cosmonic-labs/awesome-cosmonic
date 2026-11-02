# kafka-transactional (Rust)

An exactly-once read-process-write pipeline. A long-running Service consumes
from one topic, produces to another, and commits its output records together
with the input offsets in one Kafka transaction. A replayed or half-applied
batch is never visible to `read_committed` consumers. See the
[pattern guide](../../README.md) for when to choose this template over the
other three.

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
# component: target/wasm32-wasip2/release/kafka_transactional.wasm
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

- **Cosmonic Desktop**: create a project from the `rust-kafka-transactional`
  starter, which runs the component as the workload's `service`.
- **Kubernetes** (Cosmonic Control or the wasmCloud runtime operator): run
  `kubectl apply -f deploy/workload-deployment.yaml`.

The manifests point at the component published from this template
(`ghcr.io/cosmonic-labs/kafka-templates/kafka-transactional:latest`; pin a
`sha-<commit>` tag if you need fixed bytes). Once you change the source,
build it, push it to your own registry and replace the image reference.

The broker, topic, group, transaction ID and environment values in the
manifests are placeholders; set them before you apply. Create the topics
first (`demo.events` and `demo.enriched` as shipped) unless your broker
creates topics automatically.

The workload's `cosmonic:kafka` entries under `hostInterfaces` hold the
connection: broker, credentials, groups, topic grants, and the
`transactional.id` that fences off a restarted producer (Kafka rejects the
old instance once a new one starts with the same ID). The component cannot
supply or override these values. Put the credential behind `secretFrom`
rather than inlining it. The
[pattern guide](../../README.md#where-the-broker-and-credentials-are-configured)
covers the binding and topic-grant rules.

## Transaction binding

The named `transaction` binding is separate from the pull-consumer binding.
Every transactional send and `send-offsets` call goes through the resource
that `transaction::begin()` returns.

- Configure the broker and credentials on both bindings.
- The consumer grant needs the input topic. The transaction grant needs the
  output topic and every input topic whose offsets it enlists.
- Set `transaction.group.id` on the transaction binding to the same value as
  `consumer.group.id` on the consumer binding, so the offsets commit to the
  consumer's group.

The shipped manifests run a single replica on purpose. Don't scale one
unchanged manifest above one replica: every live transactional producer needs
its own stable `transactional.id`.

## Configuration

The component reads its topics and batch size from environment variables set
under `localResources.environment.config` in the manifest: `IN_TOPIC`,
`OUT_TOPIC` and `BATCH_SIZE`.

`BATCH_SIZE` defaults to 1 for bounded latency. A larger value waits until
that many records arrive or the record stream ends; there is no batch timer,
and values are capped at 100. Raise it only for a steady stream.

## Delivery

Any failed delivery or offset enlistment aborts the transaction and exits the
Service. Its supervisor then restarts it from the last atomically committed
consumer position. Downstream consumers must set
`isolation.level=read_committed` so they never see aborted output.
