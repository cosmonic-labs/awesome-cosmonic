# nats-jetstream-worker (Rust)

Drain a JetStream stream in batches, at the component's own pace.

See [the NATS templates overview](../../README.md) for how this pattern
compares with the other six.

## When to use this

The component must control the rate: scheduled drains, a rate-limited
downstream, or pulling only when something else is free. A core NATS
message on `demo.worker.run` triggers a drain; the component opens a pull
consumer (one it fetches from on demand, rather than one the server pushes
to), fetches batches until the stream is caught up, and acknowledges each
message.

## When not to

Ordinary event processing. The push consumer in
[`nats-jetstream-consumer`](../nats-jetstream-consumer) is simpler and is
the right default.

## Build

Prerequisites: Rust 1.88 or newer with the `wasm32-wasip2` target
(`rustup target add wasm32-wasip2`), and `wash` 2.5 or newer. Built and
tested with `wash` 2.5.1 and Rust 1.97.1 against `wasmcloud:nats@0.1.0`. CI
publishes with Rust 1.97.1, pinned.

```shell
wash build
# component: target/wasm32-wasip2/release/nats_jetstream_worker.wasm
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
nats stream add DEMO --subjects 'demo.stream.>' --defaults
nats consumer add DEMO demo-worker --pull --filter 'demo.stream.>' --defaults
nats stream add RECEIPTS --subjects 'done.>' --defaults
```

Pass `--filter` when you create the consumer. Without it the consumer's
filter is `>`, which no grant short of `>` contains, so opening it is
denied.

The stream, consumer, batch size and round ceiling are constants in the
source. They are deliberately not read out of the trigger message: a drain
whose target and size come from whoever can publish to `demo.worker.run`
would be a denial of service with extra steps.

## Deploy

One manifest, `deploy/workload-deployment.yaml`, serves both targets. It
points at a prebuilt image published from this repository
(`ghcr.io/cosmonic-labs/nats-templates/nats-jetstream-worker:latest`), so you can
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
https://raw.githubusercontent.com/cosmonic-labs/awesome-cosmonic/main/components/nats/rust/nats-jetstream-worker/deploy/workload-deployment.yaml
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
nats pub demo.stream.orders 'one'
nats pub demo.worker.run ''
nats stream get RECEIPTS 1
```

## Grants

The workload's `wasmcloud:nats` entry under `hostInterfaces` says what this
component may touch. Grants are deny-by-default and checked by
containment: the grant has to contain the subject or filter being asked
for. This template ships the minimum it needs:

- `subject-allow: demo.worker.run,demo.stream.>,done.demo.>` admits the
  trigger subscription, the pull consumer's filter, and the receipt the
  handler publishes to `done.demo.worker`.
- `stream-allow: DEMO` admits opening the consumer on the stream.
- `core-subscriptions: demo.worker.run` is the trigger subject.
- `max-in-flight: "1"` means one drain at a time.

Widen only what your own workload touches. Server addresses and
credentials are not here: they belong to the host's NATS plugin, and a
workload that tries to set them is refused. On Cosmonic Control a workload
can only narrow the ceiling the operator declares, never widen it. The
[overview](../../README.md#where-the-connection-and-grants-are-configured)
covers grants in full.

## Capacity

Size a batch in bytes, not messages: a fetch holds the whole batch in host
memory. The template calls `fetch` with a batch of 100, which suits small
messages. On 5 MB messages the same call asks the host for 500 MB at once;
in the measured run that OOM-killed the host and dropped every co-tenant
workload's connection with it. For large messages, lower the batch or switch to
`fetch-with-limits`, which takes a byte bound.

The numbers behind that, the other six patterns, and the host-memory sizing
rule are in [the measured operational envelope](../../tuning.md#jetstream-pull-worker).

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
