# NATS tuning guide — `wasmcloud:nats@0.1.0`

How to hit each of the seven use cases in this template set, and how to read the
errors you may meet on the way. Every recommendation here was measured on a live
rig — 186 cells at 16 KiB, then a dedicated sizing campaign at 1 MB, 2 MB, and
5 MB payloads. **All seven patterns pass at all four sizes once configured**;
this guide is the configuration.

Two facts orient everything else:

1. **Small messages mostly need nothing.** At ≤16 KiB, six of the seven
   patterns run clean on stock settings. Only fan-out needs its capacity sized
   to the burst.
2. **Large messages are a memory problem wearing a networking costume.** At
   ≥1 MB the errors you see usually name NATS (`nats: IO error`,
   `disconnected from NATS`) while the actual cause is a memory limit — the
   NATS server's pod, the host's guest budget, or in-flight bytes. Size memory
   first, then tune knobs.

---

## 1. Where each knob lives

Configuration spans four layers, and an error at one layer usually names a
different one. Know which layer a knob belongs to before changing it.

| # | Layer | Where it lives |
|---|---|---|
| 1 | **NATS server** — `max_payload`, `max_pending`, `write_deadline`, pod memory | server config + pod resources (see §4; not all chart-exposed today) |
| 2 | **JetStream stream** — `max_msg_size`, `max_bytes` | the stream itself (`nats stream edit`) |
| 3 | **Driver binding** — grants, subscriptions, capacity, ack window, timeouts | the workload manifest's `hostInterfaces[].config` |
| 4 | **Host / component** — pod memory, heap, warm pool | Helm values + `components[]` in the manifest |

### The binding config keys

The complete key set the driver accepts, grouped by who sets them.

**Connection and identity** (operator-declared on the hostgroup's
`wasmcloud-nats` plugin entry; a binding that sets no `servers` dials the
hostgroup's data plane):
`servers`, `name`, `jetstream-domain`, `inbox-prefix`,
`creds` (alias `creds-file`), `jwt`, `nkey-seed` (alias `nkey`),
`username` (alias `user`), `password`, `token`,
`tls-ca`, `tls-cert`, `tls-key`, `tls-first`.

**Grants** (operator-declared ceilings, deny-by-default — a workload may ask
for a subset, never more):
`subject-allow`, `stream-allow`, `bucket-allow`.

**Workload behaviour** (set in the workload manifest, within the grant):
`ack-mode`, `request-timeout-ms`, `max-in-flight`,
`subscription-capacity`, `subscription-capacity-bytes`,
`max-ack-pending`, `max-deliver`,
`jetstream-subscriptions`, `core-subscriptions`, `kv-watches`, `component`.

Three grant behaviours worth knowing before the first deployment:

- The grants are **separate on purpose**. Publishing to a subject does not
  grant reading the stream that captures it.
- `stream-allow` on its own reaches nothing readable: stored messages are
  checked against `subject-allow` too. Grant the subjects a stream stores
  alongside the stream, or reads return empty rather than erroring.
- `inbox-prefix` belongs per named binding, never host-wide — two workloads
  sharing an inbox consume each other's replies. Unset is better still.

### The three derivations to know by heart

**Byte budget admits `capacity ÷ payload` messages.** At the 32 MiB
`subscription-capacity-bytes` default: 2,048 messages at 16 KiB, **32 at 1 MB,
16 at 2 MB, 6 at 5 MB**. Above ~1 MB the byte budget, not the message count, is
the binding constraint.

**The JetStream ack window derives from bytes:**

```
max_ack_pending = min(max-in-flight × 2, subscription-capacity, capacity_bytes ÷ per_message_bytes)
                  floored at 16
```

`per_message_bytes` is the stream's `max_msg_size` where it sets one, otherwise
the server's `max_payload` — the worst case. Setting the stream's
`max_msg_size` near your real message size is the single cheapest fix in this
guide: it makes the window derive correctly on its own. The floor of 16 cannot
be undercut by capacity alone — set `max-ack-pending` explicitly, or set
`max-in-flight` below 8.

**The host-wide backlog ceiling is `guest memory ÷ 4`,** shared by every
subscription on the host — 128 MiB on a 512 Mi host. Past it, deliveries shed
with `reason="host memory budget"`. Capacity and host memory can only be chosen
together.

---

## 2. Tuning by use case

### 2.1 Core subscriber (`core-subscriber`)

Cheapest consumer; no ack, no redelivery. The one knob that matters is
`subscription-capacity`, and the rule is: **size it to the burst, not the
rate**. It is denominated in messages, so the protection it buys is
`capacity ÷ (arrival − drain)` seconds.

- Stock capacity is 1024 messages. A 10,000-message burst sheds 60–77% of it.
  The shed warning prints `would_have_absorbed=` — the host telling you the
  capacity that would have worked for that window. Use it.
- `max-in-flight` is largely inert for push delivery; raise the drain rate with
  `poolSize` instead (see §5).
- At ≥1 MB payloads the byte budget binds first — see §2.7's sizing rule; a
  single subscriber at 5 MB holds at most 6 undelivered messages at the
  default.
- If losing a message matters, this is the wrong pattern; use
  `jetstream-consumer`. At an identical 50,000-message burst, core push
  delivered 32% where JetStream delivered 100%.

### 2.2 Request / reply (`request-reply`)

The most forgiving pattern measured: clean at every size tested, including
5 MB, on stock settings.

- **Set `poolSize` for latency.** Unset means cold instantiation per request
  (p50 3,190 µs). `poolSize: 8` measured **p50 433 µs at 10,000 req/s** — a 6×
  gain. Small payloads only; see §5.
- Concurrency is bounded by `max-in-flight` — each request occupies an
  instance until it replies, so this is the real admission limit.
- At ≥1 MB, raise the **caller's** timeout to 5–10 s; a 5 MB reply takes
  longer to move, and the caller sees a timeout, not an error.
- Scale out with a queue group; requests round-robin. Verified clean at 1, 2,
  and 3 replicas with no duplication.
- Core NATS has no error channel. Put failure detail in the reply body or
  headers; a handler error alone leaves the caller to time out.

### 2.3 JetStream consumer, push (`jetstream-consumer`)

The safe default for anything that matters — and the pattern with the most
moving parts at large payloads. Three settings carry it from failing to clean
at every size, and a fourth keeps the ack window honest:

1. **Use a queue group, always.** The fourth field of
   `jetstream-subscriptions` is the group, and it makes the consumer
   **durable**:

   ```yaml
   jetstream-subscriptions: LOAD:load.push.>:new:workers
   #                        stream:filter   :policy:queue group
   ```

   An ephemeral push consumer is deleted by the server after **120 s** of
   inactivity; at large payloads a slow guest trips this, the consumer
   vanishes, and delivery stops with no error logged (the signature is
   `cons=N→0`). Measured at 2 MB × 500: ephemeral 151/500 → durable 429/500,
   and clean once the server was sized.

2. **Give the NATS server memory** (§4). Every JetStream push failure above
   1 MB in the campaign traced to the broker being OOM-killed at the chart's
   default pod size.

3. **Size the client buffer in messages against your payload:**
   `subscription-capacity: 64` at 1 MB, `32` at 2 MB, `16` at 5 MB (each ≈
   64–80 MB of buffer). Add `request-timeout-ms: "10000"` — large publishes
   and acks need it.

4. **Set the stream's `max_msg_size`, or expect a warning at every bind.** The
   ack window derives from the per-message estimate (§1), and without a
   stream limit that estimate is the server's `max_payload`. On the §4 server
   profile (`max_payload` 8 MB) a `LOAD` stream with no `max_msg_size` logs
   this on every deployment of this template:

   ```
   WARN wasmcloud:nats jetstream backpressure: `subscription-capacity-bytes` of 33554432
        against a 8388608-byte per-message size derives max-ack-pending 4, which would
        serialise delivery; using 16 instead …
   ```

   Delivery still works (50,000/50,000 measured on Desktop with the warning
   present), but the window floored at 16 is 16 × 8 MB of exposure on paper.
   `nats stream edit LOAD --max-msg-size=<your real M> --force` makes the
   derivation correct; pinning `max-ack-pending` is the alternative.

Also: redelivery is real. Handlers must be idempotent, and a handler that
always fails will redeliver until `max-deliver` bounds it. Under
`ack-mode: auto` the host owns the settle — an explicit ack from the guest
returns `ack-owned-by-host` (`in-progress` still works to extend ack-wait).
Settling is one-shot only on success: a settle the server *rejected* leaves the
handle usable, and retrying it is correct; `already-settled` means the work was
already done.

### 2.4 JetStream worker, pull (`jetstream-worker`)

The pattern to prefer at large payloads: the worker sets the pace. One rule
dominates:

- **`fetch(batch)` materializes `batch × message size` in host memory.** The
  default batch of 100 at 1 MB is 100 MB in a single fetch. Use **batch 4** at
  1–2 MB and **1–5 at 5 MB**. With that one change, pull ran clean at every
  size tested.
- Close fetched batches when done — settling messages does not release the
  batch handle.
- A fetch can be refused before anything is delivered. Two causes with
  different fixes: the server refused a request over the consumer's provisioned
  limits (`info` reports the limits to size against), or the host refused it
  because already-fetched messages hold the binding's whole memory budget —
  drop the `message-handle`s from earlier batches, since acking one does not
  release it. Either way, retrying unchanged fails the same way.
- **In Go, "drop" is an explicit call.** The generated bindings release a host
  resource from a GC cleanup that never runs inside one invocation, so a Go
  worker that only acks leaks every handle: measured on Desktop at
  10,000 × 16 KiB, 958 delivered and then every fetch refused with
  `LimitExceeded("the next message is larger than this fetch's 6054-byte bound; fetch without one is bound by what is left of the binding's subscription-capacity-bytes after the handles it still holds")`
  — 958 × 16 KiB is the 32 MiB default budget. `handle.Drop()` right after
  `Ack()` (and `defer puller.Drop()`) took it to 10,000/10,000; the Rust
  worker gets the same for free when its batch goes out of scope each round.
- Latency-critical paths belong elsewhere; the fetch round-trip adds delay.

### 2.5 KV store (`kv-store`)

Durable key/value with revisions and CAS. Solid at every size; the one failure
mode above 1 MB is a **publish-ack timeout**, not memory:

```
kv put failed: ack error: timed out: didn't receive ack in time
```

- Set `request-timeout-ms: "10000"` for values ≥1 MB. That change alone took
  the 1 MB and 2 MB cells to clean, and 5 MB to 498/500 — the closest to the
  edge any tuned cell runs.
- `keys` takes a subject-pattern filter (`>` for every key) and the listing is
  capped host-side at 1000; the page's `truncated` flag distinguishes a partial
  page from a complete one. Narrow the filter to walk a large bucket.
- Each put is a stream publish with an ack — this is not the pattern for high
  write rates or large blobs. At 5 MB a put lands at ~1/s.
- `get` on an absent, deleted, or purged key returns a typed status rather
  than an error or a hang.

### 2.6 KV watcher (`kv-watcher`)

The quietest pattern in the campaign: 100% watch delivery in every cell
measured, both languages, no tuning required.

- The manifest names the bucket and key filter under `kv-watches`
  (`appkv:>`).
- It shares KV's ack-timeout behaviour for its own writes; if the handler
  writes back to the bucket at ≥1 MB, carry the same
  `request-timeout-ms: "10000"`.
- Use it to react to changes. To read a value once, a plain `get` is far
  cheaper than a watch; and watch delivery follows KV semantics — a purge or
  history-trimmed key can collapse several logical changes into one event.

### 2.7 Fan-out (`fan-out`)

One inbound message becomes N outbound. Two rules, one for count and one for
bytes:

1. **Size capacity to the burst — both knobs, and mind the message size.**
   The in-host republish outruns any consumer's buffer at stock settings. On
   the campaign's Kubernetes host, a 25× fan-out of a 1,000-message input
   (25,000 deliveries) measured **clean at 50,000/50,000** with
   `max-in-flight: "8192"` + `subscription-capacity: "65536"`; stock lost ~92%.
   Two corrections from the Cosmonic Desktop validation (10,000 × 16 KiB × 25 =
   250,000 deliveries, `harness/desktop/REPORT.md`):

   - **At 16 KiB the byte budget binds, not the message count.** Stock lost
     30.5 % (Rust) / 57.2 % (Go) with
     `core subscription is shedding … reason="byte budget" queued_bytes=33550330 capacity=4096 capacity_bytes=33554432 would_have_absorbed=5521`.
     `subscription-capacity: "4096"` alone still lost 24–44 %: 4,096 × 16 KiB
     is 64 MiB against a 32 MiB `subscription-capacity-bytes` default, so the
     messages-denominated knob is inert. `subscription-capacity: "65536"` +
     `subscription-capacity-bytes: "1073741824"` delivered **250,000/250,000**
     in both languages (daemon RSS peak 357 MB Rust, 633 MB Go). At 16 B
     bodies stock was already clean at 250,000/250,000.
   - **Do not add `max-in-flight: "8192"` on a Desktop host.** Its engine
     admits **1,000 concurrent core instances**; with 8192 the plugin admitted
     deliveries the pool could not instantiate and **36–39 % FAILED** — 3,645
     and 3,945 lines of
     `core delivery failed: maximum concurrent limit of 1000 for core instances reached subject=fan.in`
     — and a delivery that fails to instantiate is dropped, not queued. The
     same run without the `max-in-flight` change was clean. Keep
     `max-in-flight` ≤ 1000 there (the default is fine); the 8192 recipe is
     unsafe on Desktop until the plugin back-pressures against the instance
     pool (reported upstream, not fixed).
2. **Size host memory from the fan-out factor, not the message size.**
   Resident memory is ≈ fan-out × payload and grows linearly: measured peaks
   for ×25 fan-out were **860 Mi at 1 MB, 1,360 Mi at 2 MB, 2,676 Mi at
   5 MB**. A 512 Mi host cannot run ×25 at 1 MB at any setting; give the pod
   2 Gi at 1–2 MB and 4 Gi at 5 MB, and check the measured peak before
   assuming any knob will help.

---

## 3. Sizing rules, in one place

Given payload **M**, burst size **B**, and fan-out factor **F**:

```
in-flight bytes         ≈ max_ack_pending × M          (JetStream)
per-subscription buffer ≈ subscription-capacity-bytes  (default 32 MiB)
host backlog ceiling    = guest memory ÷ 4             (shared by ALL subscriptions)
fan-out resident        ≈ F × M                        (size the pod from this)
server                  max_payload ≥ M + headers;  max_pending ≥ 10 × M
stream                  max_msg_size ≈ M;  max_bytes ≥ B × M
```

And the five rules of thumb that held across every measured cell:

1. `max_payload` just above M, never the 64 MiB cap — the driver *reasons*
   from it, not just enforces it.
2. Always set the stream's `max_msg_size`. Cheapest single fix.
3. Size host memory from the fan-out factor, not the message size.
4. `poolSize` is a latency win at small payloads and a memory cost at large
   ones (§5).
5. Never leave the default heap at GiB scale for components that need MiB.

---

## 4. The NATS server itself

The server is the layer most likely to be wrong and the hardest to see: a
misconfigured or dying broker reports itself as a *client-side* error.

**Pod memory is the setting that bites first.** The chart default (768 Mi) is
fine to about 16 KiB and OOM-kills reliably under 1 MB+ JetStream load — 30
times across this campaign, and every time the symptom appeared driver-side as
`nats: IO error` or a vanishing consumer. Raising the NATS pod to **4 Gi** took
it to 0 restarts and flipped four cells from failing to clean by itself.
**Size the broker before you tune the client.**

One server profile covers all payloads 1–5 MB:

```
max_payload:    8388608     # 8 MB — NATS's recommended ceiling when raised.
                            # NOT the 64 MiB cap: the driver derives the ack
                            # window from this value (§1).
max_pending:   83886080     # 80 MB — ≥ 10× peak message size, or connections
                            # stall during bursts.
write_deadline: "10s"       # large writes need more than the 2 s default.
```

These are **config-file keys, not flags**: `nats-server` (2.14.2) has no
`--max_payload` — `nats-server -js -m 8222 --max_payload 8MB` exits with
`flag provided but not defined: -max_payload`. Locally, write the profile to a
file and start from it:

```
# nats-server.conf
listen: 127.0.0.1:4222
http:   127.0.0.1:8222          # monitoring: curl :8222/varz | jq .max_payload
max_payload:    8388608
max_pending:    83886080
write_deadline: "10s"
jetstream { store_dir: "/path/with/room" }
```

```sh
nats-server -c nats-server.conf
```

And the stream:

```bash
nats stream edit LOAD --max-msg-size=8388608 --force   # ack window derives correctly
nats stream edit LOAD --max-bytes=24GB --force         # ≥ burst × payload, or
                                                       # `discard: old` drops messages
                                                       # BEFORE the consumer sees them
```

Two operational cautions:

- Where the deployment chart renders `nats-server.conf` without exposing these
  keys, they must be patched into the server's ConfigMap out of band — and **a
  `helm upgrade` re-renders the ConfigMap and reverts them silently**.
  Re-assert after every upgrade, and verify with the monitoring endpoint
  (`/varz`, grep `max_payload`).
- At ≥5 MB, NATS's own architectural advice is the **Object Store** pattern —
  chunk to 128 KB segments and publish a lightweight completion notification.
  Everything here makes 5 MB *work*; head-of-line blocking on shared subjects
  is why it may still not be the right design.

### Server sizing reference

| Payload | NATS pod memory | `max_payload` | `max_pending` | stream `max_msg_size` |
|---|---|---|---|---|
| ≤ 16 KiB | 768 Mi (default) | default (1 MB) | default | unset |
| 1–5 MB | **4 Gi** | 8388608 | 83886080 | 8388608 |

### Host values baseline for any payload ≥ 1 MB

```yaml
nats:
  resources:
    limits:   { memory: "4Gi" }     # 1. size the BROKER first
runtime:
  resources:
    limits:   { memory: "2Gi" }     # 2. 4Gi if you fan out at 5 MB
    defaultHeapMemory: "64MiB"      # 3. a Go component needs ~2.3 MiB; never GiB scale
```

---

## 5. Instance reuse (`poolSize`)

`poolSize` keeps up to N instances warm and reuses them across deliveries —
and it now applies to **every** delivery path in this set: request/reply,
core subscriptions, JetStream, and KV watches (extended to core and KV
deliveries by wasmCloud PR #5513). Each template's deployment manifests carry
this guidance inline.

- **Small payloads: a large latency win.** Request/reply p50 went 3,190 µs →
  **433 µs** at 10,000 req/s with `poolSize: 8` — cold instantiation per
  delivery is the dominant cost.
- **Large payloads: a memory cost.** Warm instances each retain their heap and
  their last message. Measured at 5 MB: `poolSize: 4` turned a survivable run
  into an OOM kill. Leave it at 1 (or unset) above ~1 MB unless host memory is
  sized for `poolSize × payload`.
- The floor exists regardless: any Go component needs ~2.3 MiB of linear
  memory per instance (Rust ~1 MiB), so the default heap must be ≥4 MiB.

---

## 6. Error catalogue

Every message below was produced on a measured run. The most important habit:
**at large payloads the error text usually does not name the cause** — diagnose
from the pairing of signatures, not from the loudest one.

### Backpressure and capacity

| You see | It means | Do |
|---|---|---|
| `core subscription is shedding … reason="backlog full" … would_have_absorbed=N` | This subscription's own buffer filled. | Set `subscription-capacity` to at least `N` (the host computed it for you). If `queued_bytes` ≈ `capacity_bytes`, raise `subscription-capacity-bytes` instead. Or raise drain rate (`poolSize`). |
| shed `reason="host memory budget"` | The **host-wide** ceiling (guest memory ÷ 4), not this subscription. | Raise host memory, lower per-subscription capacity, or split workloads across hosts. |
| `jetstream backpressure: subscription-capacity-bytes of N … derives max-ack-pending 0 … using 16 instead` | Per-message estimate is the server's `max_payload` (worst case), so the window floored. 16 × real payload is the true exposure. | Set the stream's `max_msg_size` near real message size, or pin `max-ack-pending`. |
| `slow=0` alongside `shed_total=6836` | Both are correct: `slow consumer` is the NATS client, `shed_total` is the driver, and the shed counter is windowed (~110 s). | Read `shed_total=` for shedding; never read one as the other. |
| `NATS slow consumer: at least one message was dropped before reaching the host subscription=N` | The daemon's own NATS client buffer overflowed *in front of* the driver's backlog — a second, unaccounted drop path (`shed_total=` does not see it). Measured only with Go guests and with `kv.keys(">")` on a 22,000-key bucket. | Raise `subscription-capacity` to the burst, narrow `keys` filters, raise the guest's drain rate (`poolSize`). Treat one line as an unknown amount lost. |
| `core delivery failed: maximum concurrent limit of 1000 for core instances reached` | `max-in-flight` exceeds the engine's instance pool (1,000 on a Desktop host); the delivery could not instantiate and was dropped, not queued. | Keep `max-in-flight` ≤ 1000 (default is fine); see §2.7 and §7. |

### The large-payload OOM signature

| You see | It means | Do |
|---|---|---|
| A storm of `NATS client error err=nats: IO error`, `nats: timed out`, `disconnected from NATS`, consumers rebuilding — and **no shed lines** | A process is being OOM-killed and the connection is collateral damage. These name NATS, so they read as a network or server fault; the network is fine. | Confirm: `kubectl get pod -o jsonpath='{..lastState.terminated.reason}'` → `OOMKilled` (there is no driver log line for the kill). Then: size the NATS pod (§4), reduce in-flight bytes (`max-ack-pending`, `max-in-flight`, capacity), or raise host memory. |
| `DUPLICATES` — more receipts than messages sent | The guest cannot settle within ack-wait, so JetStream redelivers. Not loss — the opposite. | Raise drain rate (`poolSize`, `max-in-flight`) or lower delivery rate. Raising `max-ack-pending` alone makes it worse. |
| Delivery stops mid-run, no error; consumer count drops (`cons=N→0`) | An **ephemeral** push consumer idled >120 s and the server reclaimed it. | Use a queue group (durable): `jetstream-subscriptions: STREAM:filter:new:group`. |
| Loss and memory worsen run over run for no config reason; `nats consumer ls` shows several random-named consumers | Orphaned ephemeral consumers from prior crashes — each receives every message. | Delete them, then bounce the workload (an orphan cannot be told from the live one by name). A queue group avoids the whole class. |

### Startup and configuration refusals

| You see | It means | Do |
|---|---|---|
| `the component needs 2.3MiB of linear memory but --default-heap-memory is 1MiB` | Heap floor: a Go component needs ~2.3–2.4 MiB, Rust ~1 MiB. Appears in the host log and the replica's `Sync` condition, not on the deployment. | Set default heap ≥ 4 MiB for Go. |
| `unexpected argument '--insecure-registry' found` | The flag was renamed and reshaped (host-wide boolean). | `--allow-insecure-registries`; prefer `--oci-ca-path` where a CA bundle exists. |
| A config key the plugin does not read | Fails host startup rather than being silently ignored. | Check the key against the list in §1 — including its spelling. |
| Deployment refused for a config key the operator owns | The hostgroup runs `workloadConfig: deny` (the default): a workload cannot set host-owned keys or widen a grant. | Take it to the operator; the grant lives in the hostgroup's plugin entry, not the workload manifest. |

### JetStream storage

| You see | It means | Do |
|---|---|---|
| `could not create Stream: insufficient storage resources available (10047)` | Requested `max-bytes` exceeds `max_storage`, which the server auto-sizes **from free disk at startup** — a stale limit survives freeing space. | Free disk, then **restart NATS** so it re-detects. |
| `could not purge Stream: … permission denied (10051)` | Aftermath of a full disk: the store directory was recreated root-owned. Reads as a permissions bug; it is a disk bug. | The store is an `emptyDir` — delete the NATS pod to clear it, re-create streams, and size runs so it does not recur. |
| Loss with no driver warning at all | Stream `max-bytes` reached with `discard: old` — messages dropped before the consumer saw them. At 5 MB, a 1 GB stream holds ~200 messages. | `max_bytes ≥ burst × payload`. |
| `kv put failed: ack error: timed out: didn't receive ack in time` | Publish-ack timeout, not memory. The dominant KV failure at ≥1 MB. | `request-timeout-ms: "10000"`. |

### Settlement

| You see | It means | Do |
|---|---|---|
| `already-settled` | An ack/nak/term the server accepted retired the handle. The work was already done — never "retry is pointless". | Nothing; treat as success. A settle that *failed on the wire* leaves the handle usable, and retrying that one is correct. |
| `ack-owned-by-host` | The binding runs `ack-mode: auto`, so the host settles. | Remove the guest's explicit settle, or switch the manifest to manual ack. `in-progress` works in either mode. |
| A pull request refused, nothing delivered | Over the consumer's provisioned limits (`info` reports them), **or** already-fetched messages hold the binding's memory budget (`info` says nothing). | Size the request against `info`; or drop `message-handle`s from earlier batches — acking one does not release it. Retrying unchanged fails the same way. |
| `LimitExceeded("the next message is larger than this fetch's N-byte bound; fetch without one is bound by what is left of the binding's subscription-capacity-bytes after the handles it still holds")` | The host's version of the row above: handles from earlier batches are still held. In Go they are held until you call `Drop()` — acking is not dropping. | Go: `handle.Drop()` after `Ack()`, `defer puller.Drop()` (§2.4). Rust: let the batch go out of scope each round. |

### Build-time (Go)

| You see | It means | Do |
|---|---|---|
| `wasm trap: async-lifted export failed to produce a result` | `time.Sleep` (or any Go runtime timer) inside an async-lifted export. | Await the host clock instead — `wasi:clocks/monotonic-clock@0.3.0` (the wasmCloud Go SDK's `sleep` package does exactly this). Never park on a Go timer in a handler. |
| Generated package has **zero functions**, exit 0 | You used standalone `wit-bindgen-go`, which silently drops every `async func`. | Use componentize-go; all functions come back. The `make verify` target and CI check catch the resulting empty component. |
| `package 'wasi:cli@0.2.8' not found. known packages: …` | Passing `-d` alongside `-w` silently drops discovered WIT paths. | Vendor the deps into the project's own `wit/deps/`. |
| `go.mod` suddenly says `module wit_component` and your requires are gone | `componentize-go bindings` rewrites `go.mod` for SDK-based projects. | Harmless in these templates (the module *is* `wit_component`); in an SDK project, restore `go.mod` and use `build` only. |
| `failed to find export of interface 'wasi:http/incoming-handler@0.2.8'` | A dependency's `componentize-go.toml` merged its default world into yours. | Pass `-w` explicitly (the templates' Makefiles do). |

### Measurement traps

If you instrument your own runs, four things produce confident wrong answers:
a peak-memory reading of `0` on a multi-second run is a failed read, not a
measurement; a peak equal to the limit is a lower bound (censored), not a peak;
shedding cells vary ~17% run to run, so verdicts need two runs; and receipts
read after the fact may have aged out of the stream — read them during or right
after a run.

---

## 7. Cosmonic Desktop

Everything above was measured on a Kubernetes host unless it says otherwise;
these are the differences when the host is Cosmonic Desktop (validated
2026-08-30 against 0.5.26, all fourteen templates — `harness/desktop/REPORT.md`).

- **The NATS is yours.** The daemon dials `nats://127.0.0.1:4222` by default
  (Settings → NATS, `GET /v1/nats`); it does not run a broker. Start one from a
  config file as in §4 with JetStream on, then create what the templates
  expect: `RECEIPTS` (`done.>`), `LOAD` (`load.>`), the durable pull consumer
  `pull-workers` on `LOAD` (filter `load.pull.>`), and the KV bucket `appkv`.
  Each template's `scripts/e2e.sh` checks for its own and prints the command.
- **Policy is `allow`.** A manifest carrying grants plus behaviour keys is
  accepted as-is; there is no operator ceiling to negotiate unless you set
  `workload_config: deny`, which then behaves like the Kubernetes hostgroup
  (a workload may narrow a declared grant, never widen one).
- **Connection keys are host-owned regardless.** `servers`, `creds`, `jwt`,
  `nkey-seed`, `username`/`password`, `token`, `tls-*`, `name`,
  `jetstream-domain`, `inbox-prefix` in a workload manifest are refused at
  apply; a different broker or credential is a named binding in `nats.yaml`
  that the workload imports with `name:` on the hostInterface.
- **No kubectl.** Apply a manifest from the app (Workloads → paste), from an
  agent (MCP tool `cosmonic_apply_workload`), or from a shell as JSON to the
  daemon's unix socket — the recipe is in every template's README under
  *Deploy*. Failures land in the workload's status message and the event
  stream (`/v1/events`), not in a CRD condition.
- **The instance pool is 1,000.** `max-in-flight` above that fails core
  deliveries instead of queueing them (§2.7); leave it at the default.
- **Go guests drain 3–6× slower than Rust and can trip a second drop path.**
  Under a 50,000 × 16 B burst the Go core-subscriber shed 501 (accounted in
  `shed_total=`) and lost 172 more to the daemon's NATS client —
  `NATS slow consumer: at least one message was dropped before reaching the host` —
  which no counter attributes; the Go kv-watcher logged 1,095 of those lines
  at 10,000 × 16 B yet lost nothing (the ordered watch consumer self-heals).
  For Go: `subscription-capacity` ≥ the burst *and* watch for that line.
- **Images from the built-in registry.** `wash oci push --insecure
  oci.localhost:8200/<name>:0.1.0 …` — the registry answers ingress hosts
  `oci`, `oci.localhost` and `oci.localhost.cosmonic.sh`, nothing else.
  Unsigned images run after a logged warning unless the policy is `strict`.

## References

- Per-template envelopes: each template's `docs/tuning.md`
- [NATS server configuration](https://docs.nats.io/reference/config) — `max_payload`, connection limits
- [Sizing & resources](https://docs.nats.io/learn/deployment/sizing-and-resources) — `max_pending` ≥ 10× peak message size
- [Slow consumers](https://docs.nats.io/learn/resilient-clients/slow-consumers) — client buffer tuning
- [NATS Object Store](https://docs.nats.io/learn/object-store/) — the recommended pattern at ≥5 MB
