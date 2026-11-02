# nats-request-reply (Rust)

Answer NATS requests on a subject, one reply per request.

See [the NATS templates overview](../../README.md) for how this pattern
compares with the other six.

## When to use this

A caller needs an answer over NATS: a lookup, a validation, or an RPC that
uses subjects instead of HTTP.

## When not to

The work outlasts the requester's timeout, or the answer has to survive a
restart. There is no redelivery and no durability here: a reply that is
not published before the requester gives up is simply lost.

## Build

Prerequisites: Rust 1.88 or newer with the `wasm32-wasip2` target
(`rustup target add wasm32-wasip2`), and `wash` 2.5 or newer. Built and
tested with `wash` 2.5.1 and Rust 1.97.1 against `wasmcloud:nats@0.1.0`. CI
publishes with Rust 1.97.1, pinned.

```shell
wash build
# component: target/wasm32-wasip2/release/nats_request_reply.wasm
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
nats-server
```

No JetStream needed. The reply goes back over core NATS to the inbox
subject the requester supplied.

## Deploy

One manifest, `deploy/workload-deployment.yaml`, serves both targets. It
points at a prebuilt image published from this repository
(`ghcr.io/cosmonic-labs/nats-templates/nats-request-reply:latest`), so you can
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
https://raw.githubusercontent.com/cosmonic-labs/awesome-cosmonic/main/components/nats/rust/nats-request-reply/deploy/workload-deployment.yaml
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
nats request demo.requests 'hello'
```

## Grants

The workload's `wasmcloud:nats` entry under `hostInterfaces` says what this
component may touch. Grants are deny-by-default and checked by
containment: the grant has to contain the subject or filter being asked
for. This template ships the minimum it needs:

- `subject-allow: demo.requests,_INBOX.>` admits the subscription and the
  reply. `_INBOX.>` is where the `nats` CLI and most clients put their reply
  subjects. Never widen this to `>`: the handler replies to whatever subject
  the request names, so a `>` grant would make it an open relay.
- `core-subscriptions: demo.requests` is the subject the host subscribes to
  on the component's behalf.

Widen only what your own workload touches. Server addresses and
credentials are not here: they belong to the host's NATS plugin, and a
workload that tries to set them is refused. On Cosmonic Control a workload
can only narrow the ceiling the operator declares, never widen it. The
[overview](../../README.md#where-the-connection-and-grants-are-configured)
covers grants in full.

## Capacity

Core queue groups (several subscribers sharing one subject, each message
going to one of them) distribute correctly, so replicas scale this pattern
cleanly: measured clean at 1, 2 and 3 replicas with no duplication. At
payloads of 1 MB and above, raise the **caller's** timeout. A slow reply
reaches the caller as a timeout, not an error.

The numbers behind that, the other six patterns, and the host-memory sizing
rule are in [the measured operational envelope](../../tuning.md#request--reply).

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
