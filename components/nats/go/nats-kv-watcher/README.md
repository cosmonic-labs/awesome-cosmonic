# nats-kv-watcher (Go)

React to every change in a JetStream KV bucket.

See [`../../README.md`](../../README.md) for how this pattern compares with
the other six.

## When to use this

Something has to react to every change in a bucket — reloading config,
invalidating a cache, mirroring state onto another system.

## When not to

The reaction has to be transactional with the write. A watch is an
observation after the fact; the write has already happened and cannot be
rolled back from here.

## Build

Prereqs: Go 1.25+, [componentize-go](https://github.com/bytecodealliance/componentize-go)
v0.4.1, `wasm-tools`, and `wash` 2.5+:

```sh
go install github.com/bytecodealliance/componentize-go@v0.4.1
cargo install wasm-tools        # or: brew install wasm-tools
```

componentize-go fetches the *patched* Go it compiles with (golang/go#76775),
so that one you do not install. Go itself you do need, for the install above
and for the `go mod tidy` in the bindings step.

```sh
make build         # fetches the WIT, generates bindings, builds the component
make verify        # asserts the export is there and the lift is async
# component: nats-kv-watcher.wasm
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
nats kv add demo
nats stream add RECEIPTS --subjects 'done.>' --defaults
```

Watches replay: a watcher starting fresh sees the current value of every
key before it sees a new change, and a redelivery can repeat one. Make
the reaction idempotent and compare `revision` when order matters.

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

The image is published from this repository once this lands on `main`, and is
then pullable from a cluster and from Desktop alike, so the pattern can be deployed and watched before any of it
is built locally. Once you change the source, push it somewhere both can reach
and replace that reference.

## Exercise it

```sh
nats kv put demo feature-x on
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

On Cosmonic Control the plugin defaults to `workloadConfig: deny`, which
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
