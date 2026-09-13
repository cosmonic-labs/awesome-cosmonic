# kafka-pull-service (Rust)

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
# component: target/wasm32-wasip2/release/kafka_pull_service.wasm
```

`wkg.lock` pins the exact versions and `wit/deps/` is gitignored, so the
first build is what populates it. `cargo build --release` produces the same
component; `.wash/config.yaml` just names that command and where its output
lands, which is what lets tooling find the artifact without being told.

To stop passing the variable, merge the entries from
[`../../wkg-registries.toml`](../../wkg-registries.toml) into
`~/.config/wasm-pkg/config.toml` once.

## Deploy

- **Cosmonic Desktop**: submit `workload.yaml` through its workload API or MCP
  integration.
- **Kubernetes** (wasmCloud runtime-operator / Cosmonic Control): run
  `kubectl apply -f deploy/workload-deployment.yaml`.

Both point at the component published from this template. Set the broker,
topic, group, and environment placeholders before applying a manifest. Once
you change the source, build it, push it to your own registry, and replace the
image reference.

The broker address and topic names in the manifests are placeholders. The
workload's `cosmonic:kafka` entry under `hostInterfaces` is where the
connection lives — broker, credentials, the topic grant, and
`consumer.group.id`. The component calls `consumer.open()` without arguments
and cannot supply or override these values. Use `secretFrom` for the credential
rather than inlining it.

See [the pattern guide](../../README.md#where-the-broker-and-credentials-are-configured)
for binding and topic-grant rules.

## When to use this pattern

Prefer the handler template for ordinary elastic processing. Use this Service
when guest code needs direct session operations such as assign, pause, seek,
rebalance events, pull pacing, or arbitrary commits.

This Service owns a long-lived consumer session and does not scale to zero.
Adding replicas adds Kafka group members and may move partitions.

`BATCH_SIZE` defaults to 1 for bounded latency. A value above 1 waits until
that many output records arrive or the record stream ends; this template has
no batch timer. Values are capped at 100. Increase it only for a steady stream
where throughput matters more than partial-batch latency.

The service commits stored input positions only after every output delivery
and every per-partition commit result succeeds. A failure exits the Service so
its supervisor restarts it from committed offsets. Transform failures are sent
to `DLQ_TOPIC`; pending output is flushed before that record is committed.
