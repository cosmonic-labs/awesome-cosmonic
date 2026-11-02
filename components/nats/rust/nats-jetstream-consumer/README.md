# nats-jetstream-consumer (Rust)

Consume a JetStream stream with acknowledgement and redelivery.

See [the NATS templates overview](../../README.md) for how this pattern
compares with the other six.

## When to use this

Delivery has to be guaranteed. JetStream (NATS's persistence layer) paces
delivery by acknowledgement, so a slow consumer is throttled instead of
overrun, and an unacknowledged message comes back.

## When not to

The cheapest possible consumer is enough and an occasional lost message
costs nothing. Use [`nats-core-subscriber`](../nats-core-subscriber).

## Build

Prerequisites: Rust 1.88 or newer with the `wasm32-wasip2` target
(`rustup target add wasm32-wasip2`), and `wash` 2.5 or newer. Built and
tested with `wash` 2.5.1 and Rust 1.97.1 against `wasmcloud:nats@0.1.0`. CI
publishes with Rust 1.97.1, pinned.

```shell
wash build
# component: target/wasm32-wasip2/release/nats_jetstream_consumer.wasm
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
nats stream add RECEIPTS --subjects 'done.>' --defaults
```

`ack-mode: auto` in the manifest means a handler that returns success
acknowledges the message, and one that returns an error does not, so the
message is redelivered. Switch it to `manual` and the handler must settle
every message itself with `ack`, `nak` or `term`. A message that returns
success without being settled still times out to redelivery.

## Deploy

One manifest, `deploy/workload-deployment.yaml`, serves both targets. It
points at a prebuilt image published from this repository
(`ghcr.io/cosmonic-labs/nats-templates/nats-jetstream-consumer:latest`), so you can
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
https://raw.githubusercontent.com/cosmonic-labs/awesome-cosmonic/main/components/nats/rust/nats-jetstream-consumer/deploy/workload-deployment.yaml
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
nats pub demo.stream.orders 'hello'
nats stream get RECEIPTS 1
```

## Grants

The workload's `wasmcloud:nats` entry under `hostInterfaces` says what this
component may touch. Grants are deny-by-default and checked by
containment: the grant has to contain the subject or filter being asked
for. This template ships the minimum it needs:

- `subject-allow: demo.stream.>,done.demo.>` admits the subscription's
  filter and the receipt the handler publishes to `done.demo.processed`.
- `stream-allow: DEMO` admits reading the stream.
- `jetstream-subscriptions: DEMO:demo.stream.>:new:workers` is the stream,
  the filter, where to start, and the queue group. Keep the queue group: see
  Capacity.

Widen only what your own workload touches. Server addresses and
credentials are not here: they belong to the host's NATS plugin, and a
workload that tries to set them is refused. On Cosmonic Control a workload
can only narrow the ceiling the operator declares, never widen it. The
[overview](../../README.md#where-the-connection-and-grants-are-configured)
covers grants in full.

## Capacity

JetStream paces delivery by settlement, so a slow consumer is throttled
rather than overrun. Every JetStream run measured was clean, which is why
this is the right default for durable work. Always set the queue group (the
fourth field of `jetstream-subscriptions`): it spreads messages across
replicas *and* makes the consumer durable. A consumer without one is
ephemeral, and the server reclaims it after 120 s idle, after which delivery
stops with nothing logged.

The numbers behind that, the other six patterns, and the host-memory sizing
rule are in [the measured operational envelope](../../tuning.md#jetstream-consumer-push).

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
