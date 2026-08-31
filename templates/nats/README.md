# Cosmonic golden templates — `wasmcloud:nats`

Fourteen starting points for building NATS-driven WebAssembly components on
Cosmonic Desktop: **seven use cases × two languages** (Rust and Go), with
identical structure and naming across both.

These are not sketches. Every one is extracted from a component that ran in the
`nats-2.8-testing` campaign — 186 measured cells at 16 KiB, then a dedicated
sizing campaign at 1 MB, 2 MB, and 5 MB payloads, in both languages — so the
guidance in each template is measurement, not opinion. **All seven use cases
pass at every size tested once configured**; the configuration lives in
[`nats-tuning.md`](nats-tuning.md).

---

## The seven use cases

| Template | Use case | Reach for it when | Not the right fit when |
|---|---|---|---|
| [`core-subscriber`](rust/core-subscriber/) · [go](go/core-subscriber/) | Fire-and-forget consumption of a NATS subject — one handler invocation per message. | Telemetry, events, cache invalidation: a lost message is survivable and low latency matters. | Losing a message matters. Core NATS has no ack, no retry, no persistence. |
| [`request-reply`](rust/request-reply/) · [go](go/request-reply/) | An RPC endpoint on NATS that scales to zero between calls. | Synchronous lookups, validations, commands with a result — callers wait for one answer. | The work outlasts the caller's timeout, or the request must survive nobody listening. |
| [`jetstream-consumer`](rust/jetstream-consumer/) · [go](go/jetstream-consumer/) | Durable at-least-once consumption: the server persists, pushes, and redelivers until acked. | You need delivery guarantees and the consumer keeps up with the server's pace. | You need exactly-once (redelivery is real; be idempotent) or the lowest possible latency. |
| [`jetstream-worker`](rust/jetstream-worker/) · [go](go/jetstream-worker/) | Pull-based batch processing: the worker asks for a batch when it is ready. | Slow or expensive work, variable capacity, large messages — the worker controls flow. | Latency-critical paths; the fetch round-trip adds delay. |
| [`kv-store`](rust/kv-store/) · [go](go/kv-store/) | Read and write a durable key/value bucket — get, put, CAS update, delete, history. | Configuration, feature flags, session or device state: the current value of something. | Large blobs or high write rates; each put is a stream publish with an ack. |
| [`kv-watcher`](rust/kv-watcher/) · [go](go/kv-watcher/) | React to every change in a KV bucket or key prefix — the host maintains the watch. | Config reload, cache coherence, leader election, projection updates. | You only need the value now — a plain `get` is far cheaper than a watch. |
| [`fan-out`](rust/fan-out/) · [go](go/fan-out/) | One inbound message becomes many outbound units of work. | Work distribution, per-recipient notification, scatter-gather. | Payloads are large: resident memory is fan-out × payload. Size the host first. |

Two companion documents cover what the table cannot:

- **[`nats-tuning.md`](nats-tuning.md)** — how to hit each use case at every
  payload size, the configuration layers and derivations, and a catalogue of
  every error the campaign produced with its remediation.
- **[`SKILL.md`](SKILL.md)** — the template-selection and build reference,
  including the exact Rust and Go build options these templates use.

---

## Choosing a use case

Three questions settle it almost every time:

1. **Does losing a message matter?**
   Yes → `jetstream-consumer` or `jetstream-worker`. No → `core-subscriber`.
2. **Does the caller wait for an answer?**
   Yes → `request-reply`. No → one of the consumers above.
3. **Is the state the point, rather than the message?**
   Reading/writing it → `kv-store`. Reacting to it → `kv-watcher`.

`fan-out` is a composition, not a destination: it turns one message into many,
and something else consumes them.

**If you are unsure, start with `jetstream-consumer`.** At an identical
50,000-message burst it delivered 100% where the core subscriber delivered 32%,
because JetStream paces delivery by acknowledgement. That backpressure is the
single biggest reliability difference in this interface.

---

## Trigger keywords

### `core-subscriber` — Core Subscriber

**Reach for this when the user says:** "subscribe to a NATS subject", "consume NATS messages", "event handler", "message listener", "react to events", "NATS consumer"

You have a stream of events on a NATS subject and want a component invoked per message. No acknowledgement, no redelivery, no ordering guarantees — the cheapest possible consumer.

*Not this if:* Do not use this when losing a message matters. Core NATS has no ack and no redelivery: if the handler traps, or the subscription buffer overflows, the message is gone silently.

### `request-reply` — Request / Reply

**Reach for this when the user says:** "NATS RPC", "request/reply", "service endpoint", "answer requests", "query service", "micro service on NATS"

You want a service other components or clients call and wait on. The host delivers the request, you publish the answer to the requester's reply subject. Per-request instantiation means it costs nothing when idle.

*Not this if:* Do not use it for work longer than the caller's timeout, and do not use it for fire-and-forget notifications — a reply nobody awaits is wasted work.

### `jetstream-consumer` — JetStream Consumer

**Reach for this when the user says:** "durable consumer", "at-least-once", "JetStream subscriber", "reliable delivery", "don't lose messages", "replay a stream"

You need delivery guarantees. JetStream retains messages, redelivers on failure, and — critically — paces delivery by acknowledgement, so a slow consumer is throttled instead of overrun.

*Not this if:* Do not use it for latency-critical request paths, and do not assume exactly-once. Redelivery is real; handlers must be idempotent.

### `jetstream-worker` — JetStream Pull Worker

**Reach for this when the user says:** "batch worker", "pull consumer", "process a backlog", "drain a queue", "paced processing", "fetch messages"

You want to control the pace and batch size rather than have the host push at you. Good for expensive per-batch work, rate-limited downstreams, and anything that benefits from amortizing setup across a batch.

*Not this if:* Do not use a large `fetch(batch)` on a stream with large messages — the batch materializes `batch × message size` in host memory. Size the batch to the payload; see [`nats-tuning.md`](nats-tuning.md) §2.4.

### `kv-store` — KV Store Client

**Reach for this when the user says:** "NATS KV", "key/value store", "persist state", "config store", "compare-and-swap", "durable state"

You need durable key/value state that outlives an instance. NATS KV gives you revisions (so compare-and-swap works), history, and a watch channel other components can subscribe to.

*Not this if:* Do not treat it as a database. Listings are filtered and capped host-side, and there are no queries — only key lookups and prefix watches.

### `kv-watcher` — KV Watcher

**Reach for this when the user says:** "watch for changes", "config reload", "cache invalidation", "react to state change", "change data capture"

You want a component invoked whenever a key changes — cache invalidation, config reload, projection updates, change-data-capture. The host maintains the watch; you just handle events.

*Not this if:* Do not use it as a work queue. Watch delivery follows KV semantics, not queue semantics, and a purge or a history-trimmed key can collapse several logical changes into one event.

### `fan-out` — Fan-Out / Amplifier

**Reach for this when the user says:** "fan out", "broadcast", "scatter", "one-to-many", "amplify", "notify many subscribers"

One input event needs to become many units of downstream work: notify N subscribers, shard a job, or trigger a parallel pipeline.

*Not this if:* Do not ship it without sizing capacity to the burst and host memory to the fan-out factor — both are quantified in [`nats-tuning.md`](nats-tuning.md) §2.7.

---

## Common layout

Every template has the same shape, in both languages:

```
<pattern>/
├── .wash/config.yaml        # wash v2 / Cosmonic Desktop project config
├── .github/workflows/ci.yml # build + verify the async export is really there
├── README.md                # when to use it, questions to dial it in, envelope
├── LICENSE
├── deploy/workload.yaml     # published-image manifest
├── docs/tuning.md           # measured operational envelope
├── scripts/e2e.sh           # drive one message through the deployment (local nats CLI), assert the effect
├── skills/<pattern>/SKILL.md
├── wit-vendor/              # vendored wasmcloud:nats@0.1.0 — the wkg.toml override target
├── wit/world.wit            # + deps/, wkg's rendering of wit-vendor (committed)
├── wkg.toml, wkg.lock       # override → wit-vendor; outside wit/deps because wash build rewrites that dir
└── workload.yaml            # local-dev manifest
```

Language-specific additions:

| Rust | Go |
|---|---|
| `.cargo/config.toml` — wasip2 target (cargo emits the component) | `Makefile` — componentize-go build |
| `Cargo.toml` — wit-bindgen 0.60 async | `componentize-go.toml` — world selection |
| `src/lib.rs` — **start here** | `export_*/handler.go` — **start here** |
| | `go.mod`/`go.sum` — pkg v0.2.2, `docs/building.md`, `docs/limitations.md` |

---

## Cross-cutting things every template repeats

These came out of the campaign and apply regardless of pattern:

- **`subscription-capacity` is the knob that matters**, and it is denominated
  in *messages*, so the protection it buys is `capacity / (arrival − drain)`
  seconds. Its required value spanned **1000×** between guest languages for the
  same workload. `max-in-flight` is largely inert for push consumers.
- **Grants are deny-by-default and separate.** Publishing to a subject does not
  grant reading the stream that captures it.
- **Adding replicas does not add buffer.** Each replica gets its own
  subscription and its own buffer — and without a queue group, its own full
  copy of the traffic.
- **Instance reuse (`poolSize`) applies to every delivery path** —
  request/reply, core, JetStream, and KV watch. It is a large latency win at
  small payloads and a memory cost at large ones; each template's deployment
  manifests carry the numbers inline, and [`nats-tuning.md`](nats-tuning.md) §5
  has the full picture.
- **Verify the export.** A component that builds but exports nothing is a real
  failure mode; the standalone Go generator produces exactly that, silently.
  Every CI workflow here checks for it.

### Go-specific, and important

**A Go handler must not park on a Go runtime timer.** `time.Sleep`,
`time.After`, `context.WithTimeout` and anything built on them trap the
instance; the working alternative is awaiting the host clock
(`wasi:clocks/monotonic-clock@0.3.0` — the wasmCloud Go SDK's `sleep` package
does exactly this). Every Go template carries
[`docs/limitations.md`](go/core-subscriber/docs/limitations.md) with the
detail. Go components are also ~24× the size of the Rust equivalent and cost
~13× more memory per replica.

---

## Provenance

| | |
|---|---|
| Driver | `wasmcloud:nats@0.1.0` (async-only WASI P3) |
| Campaign | `nats-2.8-testing` — 186 cells at 16 KiB + XL sizing at 1/2/5 MB, Rust + Go tracks |
| Rust toolchain | wit-bindgen 0.60 (`async-spawn`), `wasm32-wasip2` — `cargo build`, `wash build` and Desktop project mode produce the same component (the campaign built wasip1 + wash's adapter; identical exports) |
| Go toolchain | componentize-go `main`@`20f3b0c2` (reports 0.4.1; generated headers say wit-bindgen 0.59.0; pkg v0.2.2) + patched Go (golang/go#76775, auto-installed into the OS cache dir) |
| Desktop validation | 2026-08-30, all fourteen on Cosmonic Desktop 0.5.26 (`harness/desktop/REPORT.md`) — see [`nats-tuning.md`](nats-tuning.md) §7 for what differs on Desktop |
| Tuning | [`nats-tuning.md`](nats-tuning.md) — by use case, with the full error catalogue |
| Findings | [`tracks/go/FINDINGS.md`](../tracks/go/FINDINGS.md) · [`tracks/rust/FINDINGS.md`](../tracks/rust/FINDINGS.md) |
| Fix recommendations | [`tracks/go/REMEDIATION.md`](../tracks/go/REMEDIATION.md) |
