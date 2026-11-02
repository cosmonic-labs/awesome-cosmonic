# nats-kv-watcher (Rust)

React to every change in a JetStream KV bucket.

See [the NATS templates overview](../../README.md) for how this pattern
compares with the other six.

## When to use this

Something has to react to every change in a bucket: reloading config,
invalidating a cache, mirroring state onto another system. The host
watches the bucket and calls the component once per change.

## When not to

The reaction has to be transactional with the write. A watch is an
observation after the fact; the write has already happened and cannot be
rolled back from here.

## Build

Prerequisites: Rust 1.88 or newer with the `wasm32-wasip2` target
(`rustup target add wasm32-wasip2`), and `wash` 2.5 or newer. Built and
tested with `wash` 2.5.1 and Rust 1.97.1 against `wasmcloud:nats@0.1.0`. CI
publishes with Rust 1.97.1, pinned.

```shell
wash build
# component: target/wasm32-wasip2/release/nats_kv_watcher.wasm
```

`wash build` fetches the WIT (the interface definitions the component
compiles against) into `wit/deps/`, then runs the build command named in
`.wash/config.yaml`. `wkg.lock` pins the WIT version the way
`package-lock.json` pins npm packages, and `wit/deps/` is gitignored, so a
fresh clone gets it on the first build. Unlike the Kafka templates next
door, this needs no registry mapping: `wasmcloud:nats` is published to a
registry `wash` already knows.

Once `wit/deps/` exists, `cargo build --release --target wasm32-wasip2`
produces the same component. Cargo cannot fetch the WIT itself, so on a
fresh clone run `wash build` (or `wash wit fetch`) first.

## A local NATS for this template

You need [`nats-server`](https://github.com/nats-io/nats-server)
and the [`nats` CLI](https://github.com/nats-io/natscli). Start the server
in one terminal and run the rest in another.

```shell
nats-server -js
nats kv add demo
nats stream add RECEIPTS --subjects 'done.>' --defaults
```

Watches replay: a watcher starting fresh sees the current value of every
key before it sees a new change, and a redelivery can repeat one. Make the
reaction idempotent and compare `revision` when order matters.

## Deploy

One manifest, `deploy/workload-deployment.yaml`, serves both targets. It
points at a prebuilt image published from this repository
(`ghcr.io/cosmonic-labs/nats-templates/nats-kv-watcher:latest`), so you can
deploy the pattern and watch it work before building anything. Once you
change the source, push your own image somewhere your cluster or Desktop can
pull from, and replace the `image:` reference.

On Cosmonic Control:

```shell
kubectl apply -f deploy/workload-deployment.yaml
```

On Cosmonic Desktop 0.5.32 or newer, apply the same file: on the Workloads
page choose **Start workload** and paste the manifest's raw URL,

```text
https://raw.githubusercontent.com/cosmonic-labs/awesome-cosmonic/main/components/nats/rust/nats-kv-watcher/deploy/workload-deployment.yaml
```

or hand the file to the `cosmonic_workload_apply` MCP tool. Desktop takes
the workload out of the `WorkloadDeployment` envelope and records
`replicas` without obeying it, since Desktop is a single host.

Desktop 0.5.31 and older answer `unsupported kind`. Flatten the manifest
first: change `kind: WorkloadDeployment` to `kind: Workload`, move
everything under `.spec.template.spec` up to `spec`, and drop `replicas`.

Desktop reaches NATS through its built-in NATS plugin, configured under
Settings → Built-in plugins → NATS. It defaults to `nats://127.0.0.1:4222`,
where the local `nats-server` above listens.

## Exercise it

```shell
nats kv put demo feature-x on
nats stream get RECEIPTS 1
```

## Grants

The workload's `wasmcloud:nats` entry under `hostInterfaces` says what this
component may touch. Grants are deny-by-default and checked by
containment: the grant has to contain the subject or filter being asked
for. This template ships the minimum it needs:

- `subject-allow: done.demo.>` admits the receipt the handler publishes to
  `done.demo.kv`.
- `bucket-allow: demo` admits the bucket.
- `kv-watches: demo:>` is the bucket and the keys to watch (`>` means every
  key).

Widen only what your own workload touches. Server addresses and
credentials are not here: they belong to the host's NATS plugin, and a
workload that tries to set them is refused. On Cosmonic Control a workload
can only narrow the ceiling the operator declares, never widen it. The
[overview](../../README.md#where-the-connection-and-grants-are-configured)
covers grants in full.

## Capacity

Watch delivery was the one thing that never lost an event in either
language, and the watch itself costs effectively nothing in host memory.

The numbers behind that, the other six patterns, and the host-memory sizing
rule are in [the measured operational envelope](../../tuning.md#kv-watcher).

## Layout

```text
├── Cargo.toml                          # wit-bindgen 0.60, async features
├── .cargo/config.toml                  # wasm32-wasip2 target
├── .wash/config.yaml                   # wash and Cosmonic Desktop project config
├── src/lib.rs                          # START HERE
├── wit/world.wit                       # + deps/, fetched and gitignored
├── wkg.lock                            # pins the WIT version
└── deploy/workload-deployment.yaml     # Cosmonic Control and Desktop 0.5.32+
```
