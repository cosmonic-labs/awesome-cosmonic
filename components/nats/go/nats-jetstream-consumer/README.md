# nats-jetstream-consumer (Go)

Consume a JetStream stream with acknowledgement and redelivery.

See [`../../README.md`](../../README.md) for how this pattern compares with
the other six.

## When to use this

Delivery has to be guaranteed. JetStream paces delivery by
acknowledgement, so a slow consumer is throttled instead of overrun, and
an unacknowledged message comes back.

## When not to

The cheapest possible consumer is enough and an occasional lost message
costs nothing. Use [`nats-core-subscriber`](../nats-core-subscriber).

## Build

Prereqs: [componentize-go](https://github.com/bytecodealliance/componentize-go)
and `wash` 2.5+. componentize-go fetches the patched Go it needs the first time
it runs, so there is no toolchain to install by hand.

```sh
make build         # fetches the WIT, generates bindings, builds the component
make verify        # asserts the export is there and the lift is async
# component: nats-jetstream-consumer.wasm
```

`wasmcloud:nats@0.1.0` is async-only WASI p3. componentize-go builds against a
patched Go (golang/go#76775) because stock Go cannot emit the component-model
concurrency ABI, which is why `make build` and not `go build` is the entry
point. `wkg.lock` pins the WIT and `wit/deps/` is gitignored, so the first
build is what populates it.

**A handler that parks on a timer traps.** `time.Sleep`, `time.After` in a
select, `context.WithTimeout`, and any retry or backoff built on them fail the
delivery with `async-lifted export failed to produce a result`. Duration is
fine; only *waiting* on a timer breaks. Await
`wasi:clocks/monotonic-clock` instead.

## A local NATS for this template

```sh
nats-server -js
nats stream add DEMO --subjects 'demo.stream.>' --defaults
nats stream add RECEIPTS --subjects 'done.>' --defaults
```

`ack-mode: auto` in the manifest means returning `Ok` acknowledges and
returning `Err` does not. Switch it to `manual` and the handler must
settle every message itself with `ack`, `nak` or `term` — a message that
returns `Ok` without settling still times out to redelivery.

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
nats pub demo.stream.orders 'hello'
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
├── Makefile                            # build, verify, clean
├── componentize-go.toml                # the world componentize-go targets
├── go.mod / go.sum                     # pinned bindings SDK
├── export_*/handler.go                 # START HERE
├── export_*/errors.go                  # NatsError formatting
├── wit/world.wit                       # + deps/, fetched and gitignored
├── wkg.lock                            # pins the WIT version
└── deploy/workload-deployment.yaml     # Desktop 0.5.32+ and Cosmonic Control
```
