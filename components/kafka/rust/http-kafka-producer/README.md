# http-kafka-producer (Rust)

An HTTP endpoint that publishes records to Kafka: the shape of a webhook
receiver or an ingest gateway. The component handles each request and sends
through the binding's producer, which the host owns and reuses. No Kafka
client is created per request. See the [pattern guide](../../README.md) for
when to choose this template over the other three.

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
# component: target/wasm32-wasip2/release/http_kafka_producer.wasm
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
  also ships this template as the `rust-kafka-http-producer` project starter.
- **Kubernetes** (Cosmonic Control or the wasmCloud runtime operator): run
  `kubectl apply -f deploy/workload-deployment.yaml`.

Both manifests point at the component published from this template
(`ghcr.io/cosmonic-labs/kafka-templates/http-kafka-producer:latest`; pin a
`sha-<commit>` tag if you need fixed bytes). Once you change the source,
build it, push it to your own registry and replace the image reference.

The broker address and topic name in the manifests are placeholders; set them
before you apply. The workload's `cosmonic:kafka` entry under
`hostInterfaces` holds the connection: broker, credentials, client policy and
the topic grant. The component cannot supply or override these values. Put
the credential behind `secretFrom` rather than inlining it. The
[pattern guide](../../README.md#where-the-broker-and-credentials-are-configured)
covers the binding and topic-grant rules.

## API

- `POST /produce?topic=T&key=K&value=V` sends one record and returns its
  `partition:offset` acknowledgement.
- `POST /produce-batch?topic=T&count=N&size=S` sends `N` records (1 to
  10,000) with values of `S` bytes each (up to 1 MiB, and up to 16 MiB in
  total). It returns one delivery result per line. Batch outcomes are
  positional, so one response can contain both successes and failures.

The requested topic must be in the binding's `topics` grant.

On Cosmonic Desktop the workload answers at
`http://http-kafka-producer.localhost:8200/`:

```shell
curl -X POST 'http://http-kafka-producer.localhost:8200/produce?topic=demo.events&key=k1&value=hello'
```
