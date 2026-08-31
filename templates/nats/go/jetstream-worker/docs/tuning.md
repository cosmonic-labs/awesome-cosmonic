# Operational envelope — JetStream Pull Worker

Measured on the `nats-2.8-testing` rig: kind on podman, a 512Mi host, NATS
2.12.8 with JetStream, `wasmcloud:nats@0.1.0`. 186 cells total.

## What was measured

| load | result |
|---|---|
| 1 MB messages, `fetch(100)` | CLEAN 100/100 |
| **5 MB messages, `fetch(100)`** | **CRASH — host OOMKilled, 0/50, both languages** |
| 25 MB messages, `fetch(100)` | CRASH (known) |

`fetch(100)` on 5 MB messages asks the host to materialize **500 MB** in one
call. It killed the host — and every co-tenant workload's connection with it —
identically in Rust and Go, which is what proves it is a driver-side issue and
not a guest one. Use `fetch-with-limits` with a byte bound.

## How to read these numbers

- **Verdicts are categorical and reliable** (CLEAN / LOSS / CRASH).
- **Delivered counts on a shedding cell carry ~17% run-to-run variance**
  (n=7 on one cell, range 2,407-4,014). Treat a single-cell difference below
  ~35% as noise.
- **Peak memory is censored at the pod limit.** A cell reporting exactly the
  limit means "reached the ceiling", not "used precisely that much".

## Knobs that matter for this pattern

| knob | effect |
|---|---|
| `subscription-capacity` | Buffer between NATS and the admission semaphore, in **messages**. The dominant knob for every push pattern. |
| `max-in-flight` | Concurrent handler admissions. Matters for request/reply; largely inert for push subscribers, because a full semaphore stops the driver *pulling* and the buffer overflows instead. |
| `--default-heap-memory` | Per-guest linear memory ceiling. Defaults to 4GiB against a 512Mi host — the host warns about this at every boot. |
| replicas | Multiplies load unless a queue group distributes it. Core queue groups work; JetStream's do not (see the pattern's defect list). |

## Sizing rule

```
host_mem >= baseline(guest) + subscription-capacity x max_payload + mif x payload
```

`baseline(guest)` is **not** a driver constant — it was ~98 Mi for Rust and
~315 Mi for Go on the same host. Measure it for your own component rather than
assuming; and note a GC'd guest's residency is elastic, so a peak observed
under one budget cannot be used to size that budget.
