# Golden NATS templates for Cosmonic

Clone-and-customize starting points for building NATS workloads on Cosmonic
(`wasmcloud:nats@0.1.0`), in the same shape as the
[Kafka set](../kafka/) next door: each template is a self-contained project —
source, a `wkg.lock` pinning the WIT it fetches, a Cosmonic `workload.yaml`,
and a Kubernetes `deploy/workload-deployment.yaml`.

Every manifest points at a prebuilt component published from this directory,
so a pattern can be deployed and watched before any of it is built locally.
Those images are published when this lands on `main`; before that, build
locally and push to a registry of your own.

To start from one of these without cloning the repository:

```console
wash new https://github.com/cosmonic-labs/awesome-cosmonic \
  --subfolder components/nats/rust/nats-core-subscriber
```

Seven patterns, in Rust:

```
components/nats/
  rust/   nats-core-subscriber  | nats-request-reply | nats-fan-out
          nats-jetstream-consumer | nats-jetstream-worker
          nats-kv-store         | nats-kv-watcher
```

Start with **nats-core-subscriber** if messages are cheap and losing one is
survivable, and **nats-jetstream-consumer** if it is not. Those two cover most
workloads; the other five are for a specific need you can name.

## Which pattern do I want?

| Template | Shape | Delivery guarantee | Use when… | Don't use when… |
|---|---|---|---|---|
| **nats-core-subscriber** | Host subscribes, calls the component once per message | none — fire and forget | Telemetry, cache invalidation, notifications. The cheapest consumer there is | Losing a message matters. There is no ack and no redelivery, and an overflowing subscription buffer drops silently |
| **nats-jetstream-consumer** (recommended for durable work) | Host pushes stream messages, paced by acknowledgement | at-least-once; redelivery on nak, timeout, or trap | Delivery has to be guaranteed, and you want backpressure rather than drops | An occasional lost message costs nothing — the core subscriber is simpler and cheaper |
| **nats-request-reply** | Host subscribes, the component publishes one reply to the request's inbox | none; the requester's timeout is the only bound | A caller needs an answer over NATS — lookup, validation, RPC on subjects | The work outlasts the requester's timeout, or the answer has to survive a restart |
| **nats-fan-out** | One event in, a fixed set of downstream subjects out | none, and partial failure is not repairable | One event needs reshaping onto several subjects, and you want that routing in one place | The consumers could just subscribe to the original subject — NATS already fans out to subscribers |
| **nats-jetstream-worker** | Guest opens a pull consumer and fetches batches at its own pace | at-least-once; you ack each message | The guest must control the rate: scheduled drains, a rate-limited downstream, pulling only when something else is free | Ordinary event processing. The push consumer is simpler and is the right default |
| **nats-kv-store** | Messages on a subject become keys in a JetStream KV bucket | durable write, revisioned | Device state, per-tenant settings, a last-known-value cache that survives a restart | You need queries beyond a key lookup. A bucket is a key-value store, not a database |
| **nats-kv-watcher** | Host watches a bucket, calls the component per change | replayed; redelivery can repeat one | Reload config, invalidate a cache, mirror state onto another system | The reaction has to be transactional with the write — a watch observes after the fact |

## Where the connection and grants are configured

In the workload's own `wasmcloud:nats` entry under `hostInterfaces`, which is
what every manifest here shows.

**Grants are deny-by-default ceilings, checked by containment** — the grant
string has to contain the subject or filter being asked for, so a grant of
`demo.records` does not admit `demo.records.*`:

- `subject-allow` covers publish, request, **core subscriptions**, the
  **filter on a JetStream subscription or pull consumer**, and stored
  messages.
- `stream-allow` names the streams that may be read.
- `bucket-allow` names the KV buckets.

The separation cuts both ways, and the second direction is the one that bites.
Being able to publish to a subject does not grant reading the stream that
captures it — and `stream-allow` on its own reaches nothing readable, because
the subjects the stream stores are checked against `subject-allow` too. A
JetStream subscription whose filter is outside the subject grant does not fail
at the first message; the binding refuses to start.

A pull consumer is the sharpest edge: its filter is the consumer's own, not
one this manifest sets, and `nats consumer add` with no `--filter` creates it
with a filter of `>` — which no grant short of `>` contains. Create pull
consumers with an explicit `--filter`.

Grant exactly what the workload touches.

**Subscriptions and behaviour are the workload's** — `core-subscriptions`,
`jetstream-subscriptions`, `kv-watches`, `ack-mode`, `max-in-flight`,
`subscription-capacity`.

**Connection keys are the host's** — `servers`, credentials, and TLS material
come from the host's NATS plugin configuration, and a workload that tries to
set them is refused. The component never sees a server address in its code.
On Cosmonic Desktop that configuration lives under Settings → Built-in plugins
→ NATS, and defaults to `nats://127.0.0.1:4222`.

### On Cosmonic Control, a workload narrows; it does not widen

The `wasmcloud-nats` host plugin defaults to `workload_config: deny`. Under
`deny` the operator declares the ceiling in the hostgroup's values:

```yaml
hostPlugins:
  - id: wasmcloud-nats
    config:
      subject-allow: demo.>
      stream-allow: DEMO
      bucket-allow: demo
```

and a workload may ask for a **subset** of it. Widening a grant, or setting a
host-owned connection key, is what gets refused — so the grants stay in the
workload manifest rather than being stripped out of it. That is the same
arrangement the Kafka set uses, where the Control manifest keeps its `topics:`
grant inline.

## Two manifests, because the two runtimes take different kinds

Each template ships both, and they are not interchangeable:

- `workload.yaml` is `kind: Workload`, with `spec.components` and
  `spec.hostInterfaces` at the top level. Cosmonic Desktop takes this.
- `deploy/workload-deployment.yaml` is `kind: WorkloadDeployment`, the same
  spec nested under `.spec.template.spec` with `replicas` beside it. Cosmonic
  Control takes this.

Desktop's validator rejects a `WorkloadDeployment` outright
(`unsupported kind "WorkloadDeployment" (expected Workload)`), so a single
file cannot serve both.

That validator checks shape, not meaning: it accepts an unknown config key and
it accepts a grant too narrow for the subscription beside it. A green
`validate` says the manifest parses, not that the workload will run.

## Toolchain

The templates use `wit-bindgen 0.60` async (WASI P3) and compile with stock
`cargo build --target wasm32-wasip2`, which is also what each template's
`.wash/config.yaml` runs under `wash build`. Rust 1.88 or newer; built and
tested with `wash 2.5.1` and Rust 1.97.1 against a Cosmonic Desktop 0.5.30
daemon (wasmCloud runtime 2.9.0).

CI pins the toolchain it publishes with, following `mcp-servers.yml` rather
than the Kafka workflow: for `cargo build --target wasm32-wasip2` the WASI
import versions come from rustc's standard library, not from `Cargo.lock`, so
a floating toolchain silently changes a published component's import surface.
On a capability-driven host that surface is what an operator reviews before
granting anything, so it moves when someone decides it moves.

`wasm32-wasip2` is the target Cosmonic Desktop's Preflight doctor provisions,
and the only one these are built and published against. The async canonical
ABI comes from wit-bindgen's generated code, not from the compile target.

Unlike the Kafka set, these need no registry mapping: `wasmcloud:nats@0.1.0`
is published to a registry `wash` already knows, so `wash build` resolves the
WIT with no `WKG_CONFIG_FILE` in play.

Rust only for now. `wasmcloud:nats@0.1.0` declares every function `async
func`, which needs a toolchain that can bind the p3 async ABI — TinyGo tops
out at WASI P2 and cannot. Go via `componentize-go` is the candidate and a Go
set exists upstream; it is not published here until it is built and verified
in CI the way these are, on the same reasoning the Kafka set gives: a template
that does not compile is worse than one that does not exist.
