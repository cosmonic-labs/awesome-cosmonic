# kafka-transactional (Rust)

See `../../README.md` for when to choose this pattern over the others.

## Build

Prereqs: Rust 1.85+, `rustup target add wasm32-wasip2`.

```sh
cargo build --release        # target defaults to wasm32-wasip2 (.cargo/config.toml)
# component: target/wasm32-wasip2/release/kafka_transactional.wasm
```

The `cosmonic:kafka@0.3.0` WIT and its dependencies are vendored under
`wit/deps/` — no registry fetch needed.

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
