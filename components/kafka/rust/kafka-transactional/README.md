# kafka-transactional (Rust)

See `../../README.md` for when to choose this pattern over the others.

## Build

Prereqs: Rust 1.85+, `rustup target add wasm32-wasip2`, and `wash` 2.7+.
This template was built with `wash 2.7.0` and tested against Cosmonic Control
0.11.0 with `cosmonic:kafka@0.5.0`.

The `cosmonic:kafka@0.5.0` WIT comes from a registry rather than from this
repository, and `wash` has to be told which registry serves the `cosmonic`
namespace. Export that once per shell — `wash build` fetches too, so
prefixing a single `wash wit fetch` is not enough:

```sh
export WKG_CONFIG_FILE="$PWD/../../wkg-registries.toml"
wash build                   # fetches the WIT, then runs .wash/config.yaml
# component: target/wasm32-wasip2/release/kafka_transactional.wasm
```

`wkg.lock` pins the exact versions and `wit/deps/` is gitignored, so the
first build is what populates it. `cargo build --release` produces the same
component. `.wash/config.yaml` also marks it as a service and carries the
same environment and Kafka bindings as `workload.yaml` for project tooling.

To stop passing the variable, merge the entries from
[`../../wkg-registries.toml`](../../wkg-registries.toml) into
`~/.config/wasm-pkg/config.toml` once.

## Deploy

- **Cosmonic Desktop**: submit `workload.yaml` through its workload API or MCP
  integration.
- **Kubernetes** (wasmCloud runtime-operator / Cosmonic Control): run
  `kubectl apply -f deploy/workload-deployment.yaml`.

Both point at the component published from this template. Set the broker,
topic, group, transaction ID, and environment placeholders before applying a
manifest. Once you change the source, build it, push it to your own registry,
and replace the image reference.

This component exports `wasi:cli/run`, so it must occupy `spec.service`.
Putting it only in `spec.components` loads it but never calls `run`. The
Desktop manifest keeps a zero-size compatibility component because current
Desktop validation still requires a non-empty `components` list.
The `.wash/config.yaml` marks the build as a service for dev tooling; the dev
host must still provide `cosmonic:kafka`.

The broker address and topic names in the manifests are placeholders. The
workload's `cosmonic:kafka` entry under `hostInterfaces` is where the
connection lives — broker, credentials, groups, topic grants, and the
`transactional.id` that fences a restarted producer. The component cannot
supply or override these values. Use `secretFrom` for the credential rather
than inlining it.

See [the pattern guide](../../README.md#where-the-broker-and-credentials-are-configured)
for binding and topic-grant rules.

## Transaction binding

One Kafka binding grants both `consumer` and `transaction`. All transactional
sends and `send-offsets` calls run through the resource returned by
`transaction::begin()`. Its topic grant must include the output topic and every
input topic whose offsets it enlists.

The checked-in manifests are intentionally single-replica. Do not scale one
unchanged manifest above one replica: each simultaneously live transaction
producer needs a distinct, stable `transactional.id`.

`BATCH_SIZE` defaults to 1 for bounded latency. A value above 1 waits until
that many records arrive or the record stream ends; this template has no batch
timer, and values are capped at 100. Increase it only for a steady stream.

Any failed delivery or offset enlistment aborts the transaction and exits the
Service. The supervisor then restarts it from the last atomically committed
consumer position. Downstream consumers must use `isolation.level=read_committed`
to hide aborted output.
