# Component Host Plugins

Capabilities built as [WebAssembly components](https://wasmcloud.com/docs/runtime/creating-component-host-plugins)
and deployed into a host at runtime as trigger services with a capability
ingress, so you ship, version, and sandbox them like any other component.

Currently opt-in via the `host-component-plugins` feature, so check the docs
for the state of play before depending on one.

One directory per hosted plugin. See [CONTRIBUTING.md](../../CONTRIBUTING.md)
for requirements.

| Plugin | Serves | Status |
|---|---|---|
| [`kafka`](kafka/) | `cosmonic:kafka` — produce, consume, and a push trigger | Verified against Redpanda and Apache Kafka 4.3.1 |
| [`s3`](s3/) | `wasmcloud:blobstore` — an S3 backend | Verified against RustFS |

## CI

[`.github/workflows/host-plugins.yml`](../../.github/workflows/host-plugins.yml)
formats, lints, and unit-tests each project, then builds it and checks the
output with `wasm-tools`: that it is a valid component, and that it still
exports the interfaces a host binds against. That last check is the one worth
having — a component that has quietly stopped exporting a capability builds
fine and fails at deploy time with a message about a type mismatch.

The build step is `wash build --skip-fetch` — the same command as locally, with
wash installed by
[`wasmCloud/setup-wash-action`](https://github.com/wasmCloud/setup-wash-action)
at whatever the latest release is. Unpinned on purpose: these plugins track an
evolving host feature, so CI finding out that a new wash no longer builds them
is the point.
