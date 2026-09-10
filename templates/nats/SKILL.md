---
name: wasmcloud-nats-templates
description: Select, build, configure, and deploy a NATS-driven WebAssembly component on Cosmonic Desktop from the wasmcloud:nats golden templates - seven use cases in Rust and Go. Use when the user wants a component that subscribes to NATS subjects, answers requests, consumes JetStream durably, pulls batches, reads/writes NATS KV, watches KV changes, or fans one message out to many.
---

# `wasmcloud:nats` golden templates

Fourteen templates: seven use cases × two languages (Rust and Go), identical in
structure and naming. Every template is extracted from a component measured in
the `nats-2.8-testing` campaign at 16 KiB through 5 MB payloads. All seven use
cases pass at every size tested once configured - configuration lives in
[`nats-tuning.md`](nats-tuning.md).

## Step 1 - pick the template

Three questions settle it: does losing a message matter (yes → JetStream)?
does the caller wait for an answer (yes → request-reply)? is the state the
point rather than the message (read/write → kv-store, react → kv-watcher)?
When unsure, start with `jetstream-consumer`.

### `core-subscriber`
**Use when:** a stream of events arrives on a NATS subject and you want a
component invoked per message - telemetry, events, cache invalidation. The
cheapest possible consumer: no ack, no redelivery, no ordering guarantees.
**Not when:** losing a message matters. If the handler traps or the buffer
overflows, the message is gone silently.
**Key detail:** size `subscription-capacity` to the burst, not the rate
(stock is 1024; size it above the largest burst you expect).

### `request-reply`
**Use when:** other components or clients call and wait on a service. The host
delivers the request; you publish the answer to the reply subject. Costs
nothing when idle. The most robust pattern measured - clean at every size up
to 5 MB and at 1, 2, and 3 replicas.
**Not when:** the work outlasts the caller's timeout, or nobody awaits the
reply.
**Key detail:** `poolSize: 8` took Go p50 latency 3,190 µs → 433 µs at
10,000 req/s (Rust measured 458 µs cold at the same rate). Core NATS has no error channel - put failure detail in the reply
body/headers.

### `jetstream-consumer`
**Use when:** you need at-least-once delivery. JetStream retains, redelivers,
and paces delivery by acknowledgement, so a slow consumer is throttled instead
of overrun. The safe default for anything that matters.
**Not when:** you need exactly-once (be idempotent) or minimum latency.
**Key detail:** always use a queue group - the fourth field of
`jetstream-subscriptions: STREAM:filter:policy:group` makes the consumer
durable; an ephemeral one is server-deleted after 120 s idle and delivery
stops silently.

### `jetstream-worker`
**Use when:** the worker should control pace and batch size - expensive
per-batch work, rate-limited downstreams, large messages. The safest pattern
at large payloads.
**Not when:** the path is latency-critical; the fetch round-trip adds delay.
**Key detail:** `fetch(batch)` materializes `batch × message size` in host
memory. Batch 4 at 1-2 MB, 1-5 at 5 MB. Close fetched batches; settling does
not release the handle.

### `kv-store`
**Use when:** you need durable key/value state with revisions - configuration,
feature flags, session state. CAS via revision numbers; history; a watch
channel others can subscribe to.
**Not when:** you need queries, large blobs, or high write rates - each put is
a stream publish with an ack.
**Key detail:** `keys` takes a subject-pattern filter (`>` = all) and is
capped host-side at 1000 per page; set `request-timeout-ms: "10000"` for
values ≥1 MB.

### `kv-watcher`
**Use when:** a component should run whenever a key changes - config reload,
cache coherence, projections. The host maintains the watch. 100% watch
delivery in every cell measured.
**Not when:** you only need the value now (use `get`), or you need queue
semantics - a purge can collapse several changes into one event.
**Key detail:** the manifest names bucket and filter under `kv-watches`
(`appkv:>`).

### `fan-out`
**Use when:** one inbound message becomes many outbound units of work -
notify N subscribers, shard a job, scatter-gather.
**Not when:** capacity and memory have not been sized: resident memory is
fan-out × payload (measured ×25 peaks: 860 Mi at 1 MB, 2,676 Mi at 5 MB).
**Key detail:** for a 25,000-delivery burst, `max-in-flight: "8192"` +
`subscription-capacity: "65536"` measured clean at 50,000/50,000 - pair a
large `max-in-flight` with host memory sized for it.

## Step 2 - know the shared shape

Every template: `deploy/workload.yaml` (the manifest: point `image:` at
wherever you push, then apply), `wit/world.wit` + vendored
`wit/deps/wasmcloud-nats-0.1.0/package.wit`, `docs/tuning.md` (measured
envelope), `scripts/e2e.sh` (drive one message, assert the effect), and a CI
workflow that verifies the built component actually exports its handler.

Manifest config keys are validated by the host - an unknown key fails
deployment rather than being ignored. Grants (`subject-allow`, `stream-allow`,
`bucket-allow`) are deny-by-default **ceilings** declared by the operator;
subscriptions (`core-subscriptions`, `jetstream-subscriptions`, `kv-watches`)
and behaviour (`ack-mode`, `max-in-flight`, `subscription-capacity`,
`request-timeout-ms`, …) belong to the workload, within the grant. Publishing
to a subject does not grant reading the stream that captures it.

Instance reuse: `poolSize` on the component keeps warm instances and applies
to every delivery path (request/reply, core, JetStream, KV). Large latency win
at small payloads; a memory cost at ≥1 MB (each warm instance retains its
heap). Reuse is also a state contract: package-level state survives across
deliveries on a warm instance, so handlers must treat it as a cache (the
shipped handlers are reuse-safe), while `poolSize` unset or 0 declares state
ephemeral and is honoured with a fresh instance per delivery - see
[`nats-tuning.md`](nats-tuning.md) §5. The deployment manifests carry this
guidance inline.

## Step 3 - build

`wasmcloud:nats@0.1.0` is **async-only WASI P3**. Both toolchains need their
async support switched on explicitly; both failure modes are silent without
the verification step, so always run it.

### Rust build options (what these templates use)

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
# 0.60+ with async-spawn is REQUIRED: every function in wasmcloud:nats@0.1.0
# is an `async func`, and a sync-signature function cannot be lifted with the
# async canonical ABI.
wit-bindgen = { version = "0.60", features = ["async-spawn", "inter-task-wakeup"] }

[profile.release]
lto = true
opt-level = "s"
strip = true
```

```toml
# .cargo/config.toml - build for wasip1; the async canonical ABI comes from
# wit-bindgen's generated lift/lower code, not from the target
[build]
target = "wasm32-wasip1"

# wkg.toml - resolve the interface from the vendored copy
[overrides]
"wasmcloud:nats" = { path = "wit/deps/wasmcloud-nats-0.1.0" }
```

```rust
// src/lib.rs
wit_bindgen::generate!({ path: "wit", world: "<pattern>", generate_all });
```

Build and verify:

```bash
cargo build --release --target wasm32-wasip1     # or: wash build
wasm-tools component wit target/wasm32-wasip1/release/<name>.wasm \
  | grep -q 'export wasmcloud:nats/<handler>@0.1.0'
wasm-tools print target/wasm32-wasip1/release/<name>.wasm \
  | grep -qE 'async-lift|task-return'             # async ABI really present
```

### Go build options (what these templates use)

Two tool facts before anything else:

1. **componentize-go, pinned to `main`.** The v0.4.1 release tag predates the
   current generator; `main` reports the same `0.4.1` version string but
   vendors wit-bindgen 0.61.1 - confirm from a generated header, not from
   `--version`:

   ```bash
   go install github.com/bytecodealliance/componentize-go@main
   head -2 <generated-pkg>/wit_bindings.go   # → Generated by `wit-bindgen` 0.61.1
   ```

   The campaign and these templates were validated at commit
   `20f3b0c2a4127a3efd0c4a00580dc0b409a2f91c`, which is what the CI workflows
   pin.

2. **Never standalone `wit-bindgen-go`** (v0.7.0): it silently emits **zero
   functions** for an async-only package and exits 0. The `make verify` target
   exists because of it.

componentize-go drives a **patched Go toolchain** (golang/go#76775 - stock Go
cannot emit the component-model concurrency ABI). It downloads one into
`~/.cache/componentize-go` automatically for any world whose WIT declares
`async func`. Expect and ignore this note on every build - it is correct
behaviour, not a warning to fix:

```
Note: .../go does not support async operation; will use downloaded version.
```

Environment that makes it reliable:

```bash
export GOROOT_BOOTSTRAP_GO=/path/to/go     # bootstrap toolchain
export GOTOOLCHAIN=local
export GOFLAGS=-mod=mod
```

The templates use componentize-go's own generated bindings directly (module
`wit_component`, `go.bytecodealliance.org/pkg`) - no SDK, no vendored WASI
deps; the world imports only `wasmcloud:nats@0.1.0`:

```bash
make bindings   # componentize-go -d ./wit -w "<world>" bindings --format && go mod tidy
make build      # componentize-go -d ./wit -w "<world>" build -o <name>.wasm
make verify     # asserts the handler export AND the async lift are present
```

`-w` is passed explicitly on purpose: componentize-go merges worlds from
`componentize-go.toml` files it discovers in dependencies, and a dependency's
default world can impose exports this component has no reason to provide
(the Go SDK's default world mandates a `wasi:http/incoming-handler`).

If you switch to the wasmCloud Go SDK (`go.wasmcloud.dev/component`) instead:
use `componentize-go build` **only, never `bindings`** - the `bindings`
subcommand rewrites `go.mod` (renames the module, drops the SDK requirement)
with exit 0 and no warning. Declare your own world with
`include wasi:cli/imports@0.2.8`, vendor the WASI deps into `wit/deps/`
(passing `-d` with `-w` silently drops discovered WIT paths), and use SDK
`component/v0.1.3` or later - it is the first release that matches the landed
`wasmcloud:nats` ABI (denied-resource variant, `already-settled`/
`ack-owned-by-host`, `kv.keys` filter) and it ships the `sleep` package.

Two Go runtime rules that hold regardless of build style:

- **Never park on a Go runtime timer in a handler** (`time.Sleep`,
  `time.After`, `context.WithTimeout`) - the instance traps with
  `async-lifted export failed to produce a result`. Await the host clock
  (`wasi:clocks/monotonic-clock@0.3.0`) instead; the SDK's `sleep` package
  does this.
- **A Go component needs ~2.3 MiB of linear memory to instantiate** (Rust
  ~1 MiB). Keep the host's default heap ≥ 4 MiB. Go artifacts run ~2.7 MB
  each - ~24× the Rust equivalent, ~13× the per-replica memory.

## Step 4 - deploy and verify

```bash
# local iteration against Desktop's built-in registry
wash oci push --insecure oci-registry.localhost:8200/<name>:0.1.0 <artifact>.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh                  # drives one message, asserts the effect
```

Point `image:` in deploy/workload.yaml at wherever you push, and rename every
occurrence of the template's workload name when you fork. Narrow the grants:
they ship intentionally minimal.

## Step 5 - tune

[`nats-tuning.md`](nats-tuning.md) is the reference: per-use-case
recommendations at 16 KiB / 1 MB / 2 MB / 5 MB, the NATS server profile for
large payloads (size the broker before tuning the client), the capacity and
ack-window derivations, instance-reuse guidance, and the full error catalogue -
every error string the campaign produced, its condition, and its fix.
