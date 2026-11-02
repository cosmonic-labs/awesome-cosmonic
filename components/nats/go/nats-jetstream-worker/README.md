# nats-jetstream-worker (Go)

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
# component: nats-jetstream-worker.wasm
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
(`ghcr.io/cosmonic-labs/nats-templates/nats-jetstream-worker-go:latest`), so you can
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
https://raw.githubusercontent.com/cosmonic-labs/awesome-cosmonic/main/components/nats/go/nats-jetstream-worker/deploy/workload-deployment.yaml
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
