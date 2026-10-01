# nats-kv-store (Rust)

Write records from a subject into a JetStream KV bucket.

See [`../../README.md`](../../README.md) for how this pattern compares with
the other six.

## When to use this

Records arriving on subjects should land in a durable, revisioned key
space — device state, per-tenant settings, a last-known-value cache that
has to survive a restart.

## When not to

You need queries beyond a key lookup. A KV bucket is a key-value store,
not a database: there is no secondary index and no query language.

## Build

Prereqs: Rust 1.88+, `rustup target add wasm32-wasip2`, and `wash` 2.5+.
Built and tested with `wash 2.5.1` and Rust 1.97.1 against
`wasmcloud:nats@0.1.0`; CI publishes on Rust 1.97.1, pinned. The manifest
needs a Desktop newer than 0.5.31 to apply as written: see Deploy.

```sh
wash build                   # fetches the WIT, then runs .wash/config.yaml
# component: target/wasm32-wasip2/release/nats_kv_store.wasm
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
nats kv add demo
nats stream add RECEIPTS --subjects 'done.>' --defaults
```

The bucket has to exist first — a guest cannot create one — and
`bucket-allow` has to name it. That grant is separate from
`subject-allow` on purpose: being able to publish to a subject does not
grant writing the bucket that captures it.

## Deploy

One manifest, both targets:

```bash
kubectl apply -f deploy/workload-deployment.yaml          # Cosmonic Control
```

On **Cosmonic Desktop 0.5.32+**, apply the same file: Workloads → Run (paste
it), or the `cosmonic_workload_apply` MCP tool. Desktop reads the workload out
of the `WorkloadDeployment` envelope and reports what a single host cannot
honour, so `replicas` is recorded rather than obeyed. Desktop reads this kind from the
release after 0.5.31; until that ships, flatten it first: change `kind:
WorkloadDeployment` to `kind: Workload`, lift everything under
`.spec.template.spec` up to `spec`, and drop `replicas`.

Desktop's built-in NATS plugin is configured under Settings → Built-in plugins
→ NATS and defaults to `nats://127.0.0.1:4222`.

The image is published from this repository and is pullable from a cluster and
from Desktop alike, so the pattern can be deployed and watched before any of it
is built locally. Once you change the source, push it somewhere both can reach
and replace that reference.

## Exercise it

```sh
nats pub demo.records.order-1 '{"total":10}'
nats kv get demo order-1
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

On Cosmonic Control the plugin defaults to `workloadConfig: deny`, which
means a workload may only **narrow** the ceiling the operator declares in the
hostgroup's `hostPlugins: [{id: wasmcloud-nats, config: {...}}]`. Widening a
grant, or setting a host-owned key, is what gets refused — so the grants stay
in the manifest rather than being stripped out of it.

## Capacity

Operations are serial per handler invocation, so throughput is bounded by
round-trip latency rather than by admission. `history()` on a key with no
history **hangs the guest call indefinitely**, pinning the instance and its
admission permit — treat that call as unsafe until it is fixed.

The numbers behind that, the other six patterns, and the host-memory sizing
rule are in [the measured operational envelope](../../tuning.md#kv-store-client).

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
