---
name: wasmcloud-nats-templates
description: Select, build, configure, and deploy a NATS-driven WebAssembly component on Cosmonic Desktop from the wasmcloud:nats golden templates — seven use cases in Rust and Go. Use when the user wants a component that subscribes to NATS subjects, answers requests, consumes JetStream durably, pulls batches, reads/writes NATS KV, watches KV changes, or fans one message out to many.
---

# `wasmcloud:nats` golden templates

Fourteen templates: seven use cases × two languages (Rust and Go), identical in
structure and naming. Every template is extracted from a component measured in
the `nats-2.8-testing` campaign at 16 KiB through 5 MB payloads. All seven use
cases pass at every size tested once configured — configuration lives in
[`nats-tuning.md`](nats-tuning.md).

## Step 1 — pick the template

Three questions settle it: does losing a message matter (yes → JetStream)?
does the caller wait for an answer (yes → request-reply)? is the state the
point rather than the message (read/write → kv-store, react → kv-watcher)?
When unsure, start with `jetstream-consumer`.

### `core-subscriber`
**Use when:** a stream of events arrives on a NATS subject and you want a
component invoked per message — telemetry, events, cache invalidation. The
cheapest possible consumer: no ack, no redelivery, no ordering guarantees.
**Not when:** losing a message matters. If the handler traps or the buffer
overflows, the message is gone silently.
**Key detail:** size `subscription-capacity` to the burst, not the rate
(stock 1024 sheds 60–77% of a 10k burst).

### `request-reply`
**Use when:** other components or clients call and wait on a service. The host
delivers the request; you publish the answer to the reply subject. Costs
nothing when idle. The most robust pattern measured — clean at every size up
to 5 MB and at 1, 2, and 3 replicas.
**Not when:** the work outlasts the caller's timeout, or nobody awaits the
reply.
**Key detail:** `poolSize: 8` took p50 latency 3,190 µs → 433 µs at
10,000 req/s. Core NATS has no error channel — put failure detail in the reply
body/headers.

### `jetstream-consumer`
**Use when:** you need at-least-once delivery. JetStream retains, redelivers,
and paces delivery by acknowledgement, so a slow consumer is throttled instead
of overrun. The safe default for anything that matters.
**Not when:** you need exactly-once (be idempotent) or minimum latency.
**Key detail:** always use a queue group — the fourth field of
`jetstream-subscriptions: STREAM:filter:policy:group` makes the consumer
durable; an ephemeral one is server-deleted after 120 s idle and delivery
stops silently.

### `jetstream-worker`
**Use when:** the worker should control pace and batch size — expensive
per-batch work, rate-limited downstreams, large messages. The safest pattern
at large payloads.
**Not when:** the path is latency-critical; the fetch round-trip adds delay.
**Key detail:** `fetch(batch)` materializes `batch × message size` in host
memory. Batch 4 at 1–2 MB, 1–5 at 5 MB. Close fetched batches; settling does
not release the handle.

### `kv-store`
**Use when:** you need durable key/value state with revisions — configuration,
feature flags, session state. CAS via revision numbers; history; a watch
channel others can subscribe to.
**Not when:** you need queries, large blobs, or high write rates — each put is
a stream publish with an ack.
**Key detail:** `keys` takes a subject-pattern filter (`>` = all) and is
capped host-side at 1000 per page; set `request-timeout-ms: "10000"` for
values ≥1 MB.

### `kv-watcher`
**Use when:** a component should run whenever a key changes — config reload,
cache coherence, projections. The host maintains the watch. 100% watch
delivery in every cell measured.
**Not when:** you only need the value now (use `get`), or you need queue
semantics — a purge can collapse several changes into one event.
**Key detail:** the manifest names bucket and filter under `kv-watches`
(`appkv:>`).

### `fan-out`
**Use when:** one inbound message becomes many outbound units of work —
notify N subscribers, shard a job, scatter-gather.
**Not when:** capacity and memory have not been sized: resident memory is
fan-out × payload (measured ×25 peaks: 860 Mi at 1 MB, 2,676 Mi at 5 MB).
**Key detail:** size BOTH capacity knobs to the burst. On Cosmonic Desktop,
10,000 × 16 KiB × 25 delivered 250,000/250,000 with
`subscription-capacity: "65536"` + `subscription-capacity-bytes: "1073741824"`
where stock lost 30–57 % — the messages knob alone is inert at 16 KiB, and
`max-in-flight: "8192"` FAILS deliveries there (the engine admits 1,000
concurrent core instances); keep it ≤ 1000. [`nats-tuning.md`](nats-tuning.md) §2.7.

## Step 2 — know the shared shape

Every template: `workload.yaml` (local dev, Desktop's built-in registry),
`deploy/workload.yaml` (published image), `wit/world.wit` + the vendored
interface in `wit-vendor/wasmcloud-nats-0.1.0/` (a `wkg.toml` override points
there; `wit/deps/` is what wkg renders from it and is committed so builds
without `wash` work), `docs/tuning.md` (measured envelope), `scripts/e2e.sh`
(drive one message through the deployment with a local `nats` CLI, assert the
effect, exit non-zero with what to check), and a CI workflow that verifies the
built component actually exports its handler.

Manifest config keys are validated by the host — an unknown key fails
deployment rather than being ignored. Grants (`subject-allow`, `stream-allow`,
`bucket-allow`) are deny-by-default **ceilings** declared by the operator;
subscriptions (`core-subscriptions`, `jetstream-subscriptions`, `kv-watches`)
and behaviour (`ack-mode`, `max-in-flight`, `subscription-capacity`,
`request-timeout-ms`, …) belong to the workload, within the grant. Publishing
to a subject does not grant reading the stream that captures it.

Instance reuse: `poolSize` on the component keeps warm instances and applies
to every delivery path (request/reply, core, JetStream, KV). Large latency win
at small payloads; a memory cost at ≥1 MB (each warm instance retains its
heap). The deployment manifests carry this guidance inline.

## Step 3 — build

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
# .cargo/config.toml — build for wasm32-wasip2: cargo's wasm-component-ld emits
# a finished component (the async canonical ABI comes from wit-bindgen's
# generated lift/lower code, not from the target). A wasip1 build emits a core
# module that only `wash build` adapts; Cosmonic Desktop's project mode runs
# build.command verbatim and expects a component at component_path.
[build]
target = "wasm32-wasip2"

# wkg.toml — resolve the interface from the vendored copy. The path MUST be
# outside wit/deps: `wash build` (wkg) empties wit/deps and re-materialises it
# from the override on every build, so an override inside wit/deps deletes
# itself and the build fails with ENOENT, tree left broken. wit/deps/ holds
# wkg's rendering of it (committed; plain `cargo build` needs it) and wkg.lock
# stays `packages = []` because nothing is fetched.
[overrides]
"wasmcloud:nats" = { path = "wit-vendor/wasmcloud-nats-0.1.0" }
```

```rust
// src/lib.rs
wit_bindgen::generate!({ path: "wit", world: "<pattern>", generate_all });
```

Build and verify:

```bash
cargo build --release --target wasm32-wasip2     # or: wash build — same artifact, also refreshes wit/deps + wkg.lock
wasm-tools component wit target/wasm32-wasip2/release/<name>.wasm \
  | grep -q 'export wasmcloud:nats/<handler>@0.1.0'
wasm-tools print target/wasm32-wasip2/release/<name>.wasm \
  | grep -qE 'async-lift|task-return'             # async ABI really present
```

Cosmonic Desktop's project mode (Projects → add the folder, the `cosmonic dev`
CLI, or the MCP `cosmonic_dev` tool) runs `.wash/config.yaml`'s `build.command`
through `sh -c` and reads `component_path` as a finished component — it does
not run `wash build`, does not adapt a wasip1 core module (that fails with
"`<path>` is not a WebAssembly component"), and its toolchain doctor installs
only the `wasm32-wasip2` target. That is why the templates build for wasip2:
`cargo build`, `wash build` and Desktop all produce and read the same file.

### Go build options (what these templates use)

Two tool facts before anything else:

1. **componentize-go, pinned by commit.** The v0.4.1 release tag predates the
   current generator, and `main` reports the same `0.4.1` version string, so
   confirm the generator from a generated header, not from `--version`. The
   campaign and these templates were validated at commit
   `20f3b0c2a4127a3efd0c4a00580dc0b409a2f91c` (module version
   `v0.4.2-0.20260827144128-20f3b0c2a412`), which the CI workflows pin and
   whose headers read **`wit-bindgen` 0.59.0**:

   ```bash
   go install github.com/bytecodealliance/componentize-go@20f3b0c2a4127a3efd0c4a00580dc0b409a2f91c
   export PATH="$(go env GOPATH)/bin:$PATH"   # ahead of any other componentize-go (a cargo-installed 0.3.x, say)
   head -1 <generated-pkg>/wit_bindings.go     # → Generated by `wit-bindgen` 0.59.0
   ```

2. **Never standalone `wit-bindgen-go`** (v0.7.0): it silently emits **zero
   functions** for an async-only package and exits 0. The `make verify` target
   exists because of it.

componentize-go drives a **patched Go toolchain** (golang/go#76775 — stock Go
cannot emit the component-model concurrency ABI). It downloads one into the
OS cache directory automatically for any world whose WIT declares `async func`
— `~/Library/Caches/componentize-go/v2/go-darwin-arm64-bootstrap` on macOS,
`~/.cache/componentize-go/v2/…` on Linux. Expect and ignore this note on every
build — it is correct behaviour, not a warning to fix:

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
`wit_component`, `go.bytecodealliance.org/pkg`) — no SDK, no vendored WASI
deps; the world imports only `wasmcloud:nats@0.1.0`:

```bash
make bindings   # go mod download go.bytecodealliance.org/pkg && componentize-go -d ./wit -w "<world>" bindings --format && go mod tidy
make build      # componentize-go -d ./wit -w "<world>" build -o <name>.wasm
make verify     # asserts the handler export AND the async lift are present
```

Two facts behind that `bindings` line. componentize-go starts every subcommand
with `go list`, which fails on a checkout without a `go.sum` entry for the
runtime package (`missing go.sum entry for go.mod file; to add it: go mod
download go.bytecodealliance.org/pkg`) — the templates commit `go.sum`, the
Makefile runs the download anyway (a no-op then), and `wash build` / Desktop
project mode, which run the bare `componentize-go … build` from
`.wash/config.yaml`, rely on the committed file. And `bindings` rewrites the
`go.bytecodealliance.org/pkg` require in `go.mod` to the version its generator
was built against — `v0.2.2` at the pinned commit (it downgraded a `v0.2.3`),
which is what `go.mod` ships. If you move the pin and it rewrites again, commit
what it writes with the regenerated `go.sum`; the bindings and the runtime
package have to agree.

`-w` is passed explicitly on purpose: componentize-go merges worlds from
`componentize-go.toml` files it discovers in dependencies, and a dependency's
default world can impose exports this component has no reason to provide
(the Go SDK's default world mandates a `wasi:http/incoming-handler`).

If you switch to the wasmCloud Go SDK (`go.wasmcloud.dev/component`) instead:
use `componentize-go build` **only, never `bindings`** — the `bindings`
subcommand rewrites `go.mod` (renames the module, drops the SDK requirement)
with exit 0 and no warning. Declare your own world with
`include wasi:cli/imports@0.2.8`, vendor the WASI deps into `wit/deps/`
(passing `-d` with `-w` silently drops discovered WIT paths), and note the SDK
release must match the driver's ABI revision.

Two Go runtime rules that hold regardless of build style:

- **Never park on a Go runtime timer in a handler** (`time.Sleep`,
  `time.After`, `context.WithTimeout`) — the instance traps with
  `async-lifted export failed to produce a result`. Await the host clock
  (`wasi:clocks/monotonic-clock@0.3.0`) instead; the SDK's `sleep` package
  does this.
- **A Go component needs ~2.3 MiB of linear memory to instantiate** (Rust
  ~1 MiB). Keep the host's default heap ≥ 4 MiB. Go artifacts run ~2.7 MB
  each — ~24× the Rust equivalent, ~13× the per-replica memory.

## Step 4 — deploy and verify

```bash
# local iteration against Desktop's built-in registry (ingress host oci.localhost;
# oci.localhost.cosmonic.sh is the one name Windows can resolve)
wash oci push --insecure oci.localhost:8200/<name>:0.1.0 <artifact>.wasm

# apply workload.yaml — there is no kubectl on Cosmonic Desktop. One of:
#   agent  the cosmonic MCP tool `cosmonic_apply_workload`, manifest as its argument
#   app    Workloads → New workload → paste the manifest
#   shell  POST it as JSON to the daemon's unix socket (`cosmonicd paths` prints it;
#          macOS ~/Library/Application Support/Cosmonic/cosmonicd.sock,
#          Linux $XDG_RUNTIME_DIR/cosmonic/cosmonicd.sock)
SOCK="$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
yq -o=json . workload.yaml | curl -sS --unix-socket "$SOCK" -X POST \
  -H 'content-type: application/json' --data-binary @- http://localhost/v1/workloads
curl -sS --unix-socket "$SOCK" http://localhost/v1/workloads | jq '.[] | [.workload.metadata.name, .status.state]'

./scripts/e2e.sh                  # local `nats` CLI: drives one message, asserts the effect
```

The `cosmonic` CLI has no apply verb (`cosmonic promote --deploy` applies the
draft of a *project*, not a manifest file), so the socket recipe above is the
shell path. A workload whose binding cannot connect or lacks a grant fails at
start with the reason in its status and the event stream — read that before
the logs.

For a published image, use `deploy/workload.yaml` and rename every occurrence
of the template's workload name. Narrow the grants — they ship intentionally
minimal.

## Step 5 — tune

[`nats-tuning.md`](nats-tuning.md) is the reference: per-use-case
recommendations at 16 KiB / 1 MB / 2 MB / 5 MB, the NATS server profile for
large payloads (size the broker before tuning the client), the capacity and
ack-window derivations, instance-reuse guidance, and the full error catalogue —
every error string the campaign produced, its condition, and its fix.
