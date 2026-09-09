# http-kafka-producer (Rust)

See `../../README.md` for when to choose this pattern over the others.

## Build

Prereqs: Rust 1.85+, `rustup target add wasm32-wasip2`.

The `cosmonic:kafka@0.3.0` WIT comes from a registry rather than from this
repository, and `wash` has to be told which registry serves the `cosmonic`
namespace. Export that once per shell — `wash build` fetches too, so
prefixing a single `wash wit fetch` is not enough:

```sh
export WKG_CONFIG_FILE="$PWD/../../wkg-registries.toml"
wash build                   # fetches the WIT, then runs .wash/config.yaml
# component: target/wasm32-wasip2/release/http_kafka_producer.wasm
```

`wkg.lock` pins the exact versions and `wit/deps/` is gitignored, so the
first build is what populates it. `cargo build --release` produces the same
component; `.wash/config.yaml` just names that command and where its output
lands, which is what lets tooling find the artifact without being told.

To stop passing the variable, merge the entries from
[`../../wkg-registries.toml`](../../wkg-registries.toml) into
`~/.config/wasm-pkg/config.toml` once.

## Deploy

- **Cosmonic**: apply `workload.yaml`.
- **Kubernetes** (wasmCloud runtime-operator / Cosmonic Control):
  `deploy/workload-deployment.yaml`.

Both point at the component published from this template, so they run as
they are. Once you change the source, build it, push it to your own
registry, and set that reference instead.

The broker address and topic names in the manifests are placeholders. The
kafka `hostInterfaces[].config` is **host-pinned**: whatever is set there
(broker, topics grant, group.id, ...) wins over anything the component passes.

## Tuning notes (measured, wasmCloud 2.8 / cosmonic:kafka 0.3.0)

See the header comment in `src/lib.rs` — it carries the numbers for this
pattern.
