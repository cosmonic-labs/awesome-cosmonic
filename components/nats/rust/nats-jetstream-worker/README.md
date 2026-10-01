# nats-jetstream-worker (Rust)

Pull batches from a JetStream consumer at a pace the guest sets.

See [`../../README.md`](../../README.md) for how this pattern compares with
the other six.

## When to use this

The guest has to control the fetch rate — draining on a schedule,
batching against a rate-limited downstream, or pulling only when some
other resource is free.

## When not to

Ordinary event processing. A push consumer
([`nats-jetstream-consumer`](../nats-jetstream-consumer)) is simpler and
is what you want unless you can say why the pace has to be yours.

## Build

Prereqs: Rust 1.88+, `rustup target add wasm32-wasip2`, and `wash` 2.5+.
Built and tested with `wash 2.5.1` and Rust 1.97.1 against a Cosmonic Desktop
0.5.30 daemon (wasmCloud runtime 2.9.0) and `wasmcloud:nats@0.1.0`. CI
publishes these on Rust 1.97.1, pinned.

```sh
wash build                   # fetches the WIT, then runs .wash/config.yaml
# component: target/wasm32-wasip2/release/nats_jetstream_worker.wasm
```

`wasmcloud:nats@0.1.0` is published to a registry `wash` already knows, so
unlike the Kafka templates next door this one needs no registry mapping.
`wkg.lock` pins the exact version and `wit/deps/` is gitignored, so the first
build is what populates it.

Once `wit/deps/` exists, `cargo build --release --target wasm32-wasip2`
produces the same component — but it cannot populate it, so a fresh clone has
to run `wash build` (or `wash wit fetch`) first. `.wash/config.yaml` just names
that command and where its output lands, which is what lets tooling find the
artifact without being told.

## A local NATS for this template

```sh
nats-server -js
nats stream add DEMO --subjects 'demo.stream.>' --defaults
nats consumer add DEMO demo-worker --pull --filter 'demo.stream.>' --defaults
nats stream add RECEIPTS --subjects 'done.>' --defaults
```

The stream, consumer, batch size and round ceiling are constants in
`src/lib.rs`. They are deliberately not read out of the trigger message:
a drain whose target and size come from whoever can publish to
`demo.worker.run` would be a denial of service with extra steps.

## Deploy

One manifest, both targets:

```bash
kubectl apply -f deploy/workload-deployment.yaml          # Cosmonic Control
```

On **Cosmonic Desktop 0.5.32+**, apply the same file: Workloads → Run (paste
it), or the `cosmonic_workload_apply` MCP tool. Desktop reads the workload out
of the `WorkloadDeployment` envelope and reports what a single host cannot
honour, so `replicas` is recorded rather than obeyed. On 0.5.31 and older,
which answered `unsupported kind`, flatten it into a `Workload` first.

Desktop's built-in NATS plugin is configured under Settings → Built-in plugins
→ NATS and defaults to `nats://127.0.0.1:4222`.

The image is published from this repository and is pullable from a cluster and
from Desktop alike, so the pattern can be deployed and watched before any of it
is built locally. Once you change the source, push it somewhere both can reach
and replace that reference.


## Exercise it

```sh
nats pub demo.stream.orders 'one'
nats pub demo.worker.run ''
nats stream get RECEIPTS 1
```

## Where the connection is configured

In the workload's own `wasmcloud:nats` entry under `hostInterfaces`. The
grants (`subject-allow`, `stream-allow`, `bucket-allow`) are deny-by-default
ceilings and the shipped ones are intentionally minimal — widen only what this
workload touches.

`subject-allow` covers more than publishing. It gates publish, request, core
subscriptions, the filter on a JetStream subscription or pull consumer, and
stored messages, and it is checked by **containment**: the grant string has to
contain the subject or filter being asked for. So `stream-allow` on its own
reaches nothing readable — grant the subjects a stream stores alongside the
stream itself, or the binding refuses to start.

Connection keys are the host's, not the manifest's: `servers`, credentials and
TLS material come from the host's NATS plugin configuration, and a workload
that tries to set them is refused. The component never sees a server address
in its code.

On Cosmonic Control the plugin defaults to `workload_config: deny`, which
means a workload may only **narrow** the ceiling the operator declares in the
hostgroup's `hostPlugins: [{id: wasmcloud-nats, config: {...}}]`. Widening a
grant, or setting a host-owned key, is what gets refused — so the grants stay
in the manifest rather than being stripped out of it.

## Layout

```
├── Cargo.toml                          # wit-bindgen 0.60, async features
├── .cargo/config.toml                  # wasm32-wasip2 target
├── .wash/config.yaml                   # wash v2 / Desktop project config
├── src/lib.rs                          # START HERE
├── wit/world.wit                       # + deps/, fetched and gitignored
├── wkg.lock                            # pins the WIT version
└── deploy/workload-deployment.yaml     # Desktop 0.5.32+ and Cosmonic Control
```
