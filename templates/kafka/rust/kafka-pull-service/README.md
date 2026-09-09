# kafka-pull-service (Rust)

See `../../README.md` for when to choose this pattern over the others.

## Build

Prereqs: Rust 1.85+, `rustup target add wasm32-wasip2`.

```sh
wash build                   # runs the command in .wash/config.yaml
# component: target/wasm32-wasip2/release/kafka_pull_service.wasm
```

`cargo build --release` produces the same component — `.wash/config.yaml`
just names that command and where its output lands, which is what lets
tooling find the artifact without being told.

The `cosmonic:kafka@0.3.0` WIT and its dependencies come from the registry
rather than the repository, so fetch them once before the first build:

```sh
WKG_CONFIG_FILE=../../wkg-registries.toml wash wit fetch
```

`wkg.lock` pins the exact versions, `wit/deps/` is gitignored, and
[`../../wkg-registries.toml`](../../wkg-registries.toml) is what maps the
`cosmonic` namespace to the registry serving it.

## Deploy

- **Cosmonic Desktop**: push the component to a registry (or use the Desktop
  local registry), set the image in `workload.yaml`, and apply it.
- **Kubernetes** (wasmCloud runtime-operator / Cosmonic Control):
  `deploy/workload-deployment.yaml`.

Every `CHANGEME` in the manifests needs your registry/broker values. The
kafka `hostInterfaces[].config` is **host-pinned**: whatever is set there
(broker, topics grant, group.id, ...) wins over anything the component passes.

## Tuning notes (measured, wasmCloud 2.8 / cosmonic:kafka 0.3.0)

See the header comment in `src/lib.rs` — it carries the numbers for this
pattern — and the campaign report in `k8s-perf/RESULTS/REPORT.md`.
