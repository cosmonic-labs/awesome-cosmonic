# nats-request-reply (Go)

Answer NATS requests on a subject, one reply per request.

See [`../../README.md`](../../README.md) for how this pattern compares with
the other six.

## When to use this

A caller needs an answer over NATS — a lookup, a validation, an RPC
that happens to use subjects instead of HTTP.

## When not to

The work outlasts the requester's timeout, or the answer has to survive a
restart. There is no redelivery and no durability here: a reply that is
not published before the requester gives up is simply lost.

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
# component: nats-request-reply.wasm
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
nats-server
```

This is the one pattern in the set that needs no JetStream: the reply
goes back over core NATS to the inbox subject the requester supplied,
which is what `_INBOX.>` in `subject-allow` grants.

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
nats request demo.requests 'hello'
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
