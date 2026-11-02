# nats-fan-out (Go)

Take one event and publish it to several downstream subjects.

See [the NATS templates overview](../../README.md) for how this pattern
compares with the other six.

## When to use this

One incoming event has to reach several downstream subjects in different
shapes (an audit log, a search index, a notifier), and you want that
routing in one place rather than in each consumer.

## When not to

The consumers could simply subscribe to the original subject. NATS
already delivers one message to every subscriber; this template is for
when each branch needs its own subject or its own payload.

## Build

Prerequisites: Go 1.25 or newer,
[componentize-go](https://github.com/bytecodealliance/componentize-go)
v0.4.1, `wasm-tools`, and `wash` 2.5 or newer.

```shell
go install github.com/bytecodealliance/componentize-go@v0.4.1
cargo install wasm-tools        # or: brew install wasm-tools
```

```shell
make build         # fetches the WIT, generates bindings, builds the component
make verify        # checks the export is there and the lift is async
# component: nats-fan-out.wasm
```

Why `make build` and not `go build`: every function in
`wasmcloud:nats@0.1.0` is async (WASI P3), and stock Go cannot emit the
component-model async ABI. componentize-go downloads a patched Go
(golang/go#76775) and compiles with that, so you do not install it
yourself. You still need your own Go, for the `go install` above and for
the `go mod tidy` the bindings step runs. `wkg.lock` pins the WIT version
the way `go.sum` pins modules, and `wit/deps/` is gitignored, so the first
build fetches it.

**A handler that waits on a timer traps.** `time.Sleep`, `time.After` in a
select, `context.WithTimeout`, and any retry or backoff built on them fail
the delivery with `async-lifted export failed to produce a result`. Taking
a duration is fine; only *waiting* on a timer breaks. Await
`wasi:clocks/monotonic-clock` instead.

## A local NATS for this template

You need [`nats-server`](https://github.com/nats-io/nats-server)
and the [`nats` CLI](https://github.com/nats-io/natscli). Start the server
in one terminal and run the rest in another.

```shell
nats-server
```

No JetStream needed: everything here is core NATS. The fan is the `TARGETS`
list in the source, not a field in the message, so whoever publishes to
`demo.events` cannot choose where it goes. Every target must also appear in
`subject-allow`, or the host denies the publish.

## Deploy

One manifest, `deploy/workload-deployment.yaml`, serves both targets. It
points at a prebuilt image published from this repository
(`ghcr.io/cosmonic-labs/nats-templates/nats-fan-out-go:latest`), so you can
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
https://raw.githubusercontent.com/cosmonic-labs/awesome-cosmonic/main/components/nats/go/nats-fan-out/deploy/workload-deployment.yaml
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
nats sub 'demo.events.>' &
nats pub demo.events 'hello'
```

## Grants

The workload's `wasmcloud:nats` entry under `hostInterfaces` says what this
component may touch. Grants are deny-by-default and checked by
containment: the grant has to contain the subject or filter being asked
for. This template ships the minimum it needs:

- `subject-allow: demo.events,demo.events.>` admits the subscription and the
  three targets (`demo.events.audit`, `demo.events.index` and
  `demo.events.notify`).
- `core-subscriptions: demo.events` is the subject the host subscribes to on
  the component's behalf.

Widen only what your own workload touches. Server addresses and
credentials are not here: they belong to the host's NATS plugin, and a
workload that tries to set them is refused. On Cosmonic Control a workload
can only narrow the ceiling the operator declares, never widen it. The
[overview](../../README.md#where-the-connection-and-grants-are-configured)
covers grants in full.

## Capacity

Capacity has to be sized at **every hop a burst traverses**. Raising only the
receiver's capacity left the fan-out's own input subscription shedding, and
no receiver-side setting touches that.

The numbers behind that, the other six patterns, and the host-memory sizing
rule are in [the measured operational envelope](../../tuning.md#fan-out--amplifier).

## Layout

```text
├── Makefile                            # build, verify, clean
├── componentize-go.toml                # the world componentize-go targets
├── go.mod / go.sum                     # pinned bindings SDK
├── .wash/config.yaml                   # wash and Cosmonic Desktop project config
├── export_*/handler.go                 # START HERE
├── export_*/errors.go                  # NatsError formatting
├── wit/world.wit                       # + deps/, fetched and gitignored
├── wkg.lock                            # pins the WIT version
└── deploy/workload-deployment.yaml     # Cosmonic Control and Desktop 0.5.32+
```
