# Measured operational envelope

What these seven patterns actually did under load, so the capacity numbers in
the manifests are a starting point you can reason about rather than a guess.

Measured on a `kind` cluster on podman: a 512 Mi host, NATS 2.12.8 with
JetStream, `wasmcloud:nats@0.1.0`, both language tracks. 186 cells in total.

## The one number to take away

At the stock `subscription-capacity` of 1024, a core subscriber draining about
566 msg/s against about 2,000 msg/s of arrivals **shed 60% of the messages**.
Raising capacity above the width of the burst fixed it completely, at every
admission setting tested.

`max-in-flight` is *inert* for that failure. Sweeping it from 1 to 8192 moved
delivery by noise, because the thing overflowing is the buffer in **front** of
the admission semaphore, not the semaphore. Reach for `subscription-capacity`
first on any push pattern.

## How to read these numbers

- **Verdicts are categorical and reliable** (CLEAN / LOSS / CRASH).
- **Delivered counts on a shedding cell carry about 17% run-to-run variance**
  (n=7 on one cell, range 2,407-4,014). Treat a single-cell difference below
  about 35% as noise.
- **Peak memory is censored at the pod limit.** A cell reporting exactly the
  limit means "reached the ceiling", not "used precisely that much".

## The knobs

| knob | effect |
|---|---|
| `subscription-capacity` | Buffer between NATS and the admission semaphore, in **messages**. The dominant knob for every push pattern. |
| `max-in-flight` | Concurrent handler admissions. Matters for request/reply; largely inert for push subscribers, because a full semaphore stops the driver *pulling* and the buffer overflows instead. |
| `--default-heap-memory` | Per-guest linear memory ceiling. Defaults to 4 GiB against a 512 Mi host, which the host warns about at every boot. |
| replicas | Multiplies load unless a queue group distributes it. Core queue groups distribute correctly — verified clean at 1, 2 and 3 replicas with no duplication. For JetStream push, the queue group is the fourth field of `jetstream-subscriptions`; it distributes across replicas *and* makes the consumer durable, so always set it. An ephemeral push consumer is reclaimed by the server after 120 s idle, after which delivery stops with nothing logged. Each replica keeps its own buffer. |

## Sizing rule

```
host_mem >= baseline(guest) + subscription-capacity x max_payload + mif x payload
```

`baseline(guest)` is **not** a driver constant — it was about 98 Mi for Rust
and about 315 Mi for Go on the same host. Measure it for your own component
rather than assuming. Note also that a garbage-collected guest's residency is
elastic, so a peak observed under one budget cannot be used to size that
budget.

## Per pattern

### Core subscriber

| load | Rust | Go |
|---|---|---|
| 1,000 msgs, 0 B, stock | CLEAN 1000/1000 | CLEAN 1000/1000 |
| 10,000 msgs, 0 B, stock | **CLEAN 10000/10000** | **LOSS 3,960/10,000** |
| 10,000 msgs, `capacity=65536` | CLEAN | **CLEAN 10000/10000** |
| 16 KiB x 10,000 | CLEAN, 110 Mi peak | CRASH — OOMKilled |

This is the pattern the 60% shed above was measured on, and the one with no
backpressure of any kind: there is no ack and no redelivery, so an overflowing
buffer drops silently.

### Request / reply

| load | result |
|---|---|
| 5,000 requests, 1 replica | CLEAN 5000/5000 |
| 5,000 requests, 2 replicas (queue group) | CLEAN 5000/5000 |
| 5,000 requests, 3 replicas (queue group) | CLEAN 5000/5000 |

Core queue groups distribute correctly — delivery stays at exactly the request
count as replicas scale, rather than multiplying. This was the only pattern
CLEAN at every replica count tested, in both languages.

At payloads of 1 MB and above, raise the **caller's** timeout to 5-10 s: a
5 MB reply takes longer to move, and the caller sees a timeout rather than an
error. Core NATS has no error channel, so put failure detail in the reply body
or headers; a handler error on its own just leaves the caller to time out.

### Fan-out / amplifier

| shape | Rust | Go |
|---|---|---|
| 200 in x 25 = 5,000 out, stock | LOSS 2,180 (56% lost) | LOSS 1,257 (75% lost) |
| 1,000 in x 25 = 25,000 out, stock | **LOSS 6,972 (72% lost)** | **LOSS 2,073 (92% lost)** |
| 2,000 in x 25 = 50,000 out, `capacity=65536` | **CLEAN 50,000/50,000** | LOSS 46,425 (7% lost, upstream) |

Read the last row carefully. Raising the *receiver's* capacity fixed Rust
completely and took Go from 6% delivery to 93% — and the remaining 7% was the
**amplifier's own input subscription** shedding, which no receiver-side knob
touches. Capacity has to be sized at every hop a burst traverses.

### JetStream consumer (push)

| load | core push | **JetStream push** |
|---|---|---|
| 50,000 msgs, stock knobs | LOSS 15,785 (68% lost) | **CLEAN 50,000/50,000** |
| 50,000 msgs, `mif=8192` | LOSS 15,551 | **CLEAN 50,000/50,000** |
| 2,000 msgs @ 36 KiB | (not run) | **CLEAN 2,000/2,000** |

Every JetStream cell in the hostile layer was CLEAN, all 12 of them. The reason
is structural: JetStream paces delivery by *settlement*, so a slow consumer is
throttled rather than overrun. That backpressure is exactly what core push
lacks, and it is why this is the recommended pattern for durable work.

JetStream at payloads of 1 MB and above wants a 4 Gi broker pod. An undersized
broker reports itself as client-side errors, so size the broker before touching
client knobs.

### JetStream pull worker

| load | result |
|---|---|
| 1 MB messages, `fetch(100)` | CLEAN 100/100 |
| **5 MB messages, `fetch(100)`** | **CRASH — host OOMKilled, 0/50, both languages** |
| 25 MB messages, `fetch(100)` | CRASH (known) |

`fetch(100)` on 5 MB messages asks the host to materialize **500 MB** in one
call. It killed the host — and every co-tenant workload's connection with it —
identically in Rust and Go, which is what proves it is a driver-side issue and
not a guest one. Use `fetch-with-limits` with a byte bound.

### KV store client

| operation | result |
|---|---|
| put / get / update (CAS) / delete | CLEAN, about 333 op/s serial |
| 20-op batch with watch | `ok=20 err=0 watch_receipts=20` |
| `history()` on a key with no history | **hangs the guest call indefinitely** |

Operations are serial per handler invocation, so throughput is bounded by
round-trip latency rather than by admission. The `history()` hang pins the
instance *and* its admission permit — treat that call as unsafe until it is
fixed.

### KV watcher

| load | result |
|---|---|
| 20 KV writes | 20 watch events delivered (100%) |
| across every KV cell in the campaign | no drops observed |

Watch delivery was the one thing that never lost an event in either language,
and peak host memory stayed flat — the watch itself costs effectively nothing.
