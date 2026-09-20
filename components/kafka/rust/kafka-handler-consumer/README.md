# kafka-handler-consumer (Rust)

See `../../README.md` for when to choose this pattern over the others.

This is the recommended starting point for serverless Kafka processing. The
host preserves consumer-group membership while component instances scale with
partition work and can return to zero when idle.

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
# component: target/wasm32-wasip2/release/kafka_handler_consumer.wasm
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
subscription, group, grant, and dead-letter placeholders before applying a
manifest. Create those topics first unless your broker allows automatic topic
creation. Once you change the source, build it, push it to your own registry,
and replace the image reference.

The broker address and topic names in the manifests are placeholders. The
workload's `cosmonic:kafka` entry under `hostInterfaces` is where the
connection lives — broker, credentials, the topic grant, and
`handler.group.id`. The component cannot supply or override these values. Use
`secretFrom` for the credential rather than inlining it. See
[the pattern guide](../../README.md#where-the-broker-and-credentials-are-configured).

## Scaling

Useful concurrency per workload replica is bounded by assigned partitions, the
plugin's 64-call ceiling, and `poolSize × maxConcurrency`. Increasing the pool
does not create Kafka group members or trigger a rebalance. Instances are
created on demand; `reclaimWindowSeconds` and `reclaimMinInstances: 0` let the
pool return to zero after the burst. Keep `maxConcurrency: 1` unless handler
code is safe for overlapping calls on one instance.

Scale-to-zero applies to guest component instances. The host keeps its native
Kafka consumer, connection, and group membership alive so new records do not
wait for a rebalance or a pod start.

Increasing `spec.replicas` is different: it creates another Kafka group member
and can move partitions. Use replicas for host-level availability or when more
group members are needed, and use the component pool for elastic compute on
the partitions already assigned to a replica.

## Delivery

Each call receives a non-empty, offset-ordered batch from one partition.
Return `Ok(None)` after handling the full batch. If a later record fails after
earlier records succeeded, return `Ok(Some(offset))` with the last successful
record's offset; the host preserves that progress and redelivers the suffix.
Return `Transient` when the head record may succeed later and `Permanent` when
it should go directly to `dead-letter.topic`.

Delivery is at least once. A crash can repeat a side effect before its offset
is committed, so handlers must be idempotent. Do not rely on instance-local
state: calls can land on different instances and idle instances are reclaimed.

The sample logs a per-instance heartbeat after each ten seconds of Kafka
record time. Its random ID and counters make pool growth and reclamation
visible during development; they reset with the instance and are not durable
application metrics.

The sample treats tombstones as handled and sends invalid UTF-8 values to the
dead-letter topic. Replace the marked block in `src/lib.rs` with application
logic while preserving its partial-progress behavior.

## Publish from a handler

A handler can also import `cosmonic:kafka/producer@0.5.0` for
consume-transform-produce workloads. Add the producer import to `wit/world.wit`,
add `producer` to the manifest's Kafka interfaces, and include every output
topic in `topics`. The host-owned producer remains available across elastic
handler instances; this does not require the pull-service template.

Use the transactional Service when outputs and consumed offsets must commit
atomically. Ordinary handler-produced output is at least once and may be
duplicated after a crash.
