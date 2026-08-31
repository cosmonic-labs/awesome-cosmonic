# Go guest limitations — read before writing a handler

## 1. A handler that parks on a timer traps (no workaround)

```go
func Handle(msg nats.Message) error {
    time.Sleep(100 * time.Millisecond)   // ← TRAPS. Delivery fails.
    ...
}
```

The host reports:

```
WARN core handler trapped: wasm trap: async-lifted export failed to produce a result
```

**What this rules out:** retry with backoff, rate limiting, debouncing,
polling, `context.WithTimeout`, and `time.After` in a `select`.

**What still works:** duration itself is fine. A 500 ms busy-wait completed
normally, as did 200 million arithmetic iterations. Only *waiting on a timer*
breaks — the runtime goes idle with nothing runnable and returns a status the
host reads as "completed without a result".

**Confirmed across three toolchains**, so it is not a version artifact:
wit-bindgen 0.59.0 + pkg v0.2.2, 0.59.0 + pkg v0.2.4-pre (the official wasmCloud
example's pin), and 0.61.1 + pkg v0.2.3.

**Blast radius differs by path.** On core push a trapped delivery is lost once.
On JetStream it is redelivered — and traps again — indefinitely: the campaign
measured 7,000 traps for 2,000 messages, with the consumer never advancing.

**Where a fix belongs:** upstream, in the patched Go's `wasi-on-idle` path
(golang/go#76775) or `witAsync.Run`'s status mapping — not in the wasmCloud
driver, which behaves correctly given a guest that never calls `task.return`.

## 2. Components are ~24x larger than the Rust equivalent

2.6 MB versus ~100 KB, and the spread across patterns is only 4% — almost all
of it is the Go runtime rather than your code. Budget for it in image pull time
and in the per-instance memory ceiling.

## 3. Per-replica memory cost is ~13x Rust's

~53 Mi per additional Go replica against ~4 Mi for Rust. Three replicas of an
un-grouped consumer OOM-killed a 512Mi host. Prefer a queue group, and budget
memory per replica rather than assuming it is free.

## 4. Minimum component memory is 2.2x Rust's

2.31-2.37 MiB versus 1.06 MiB. `--default-heap-memory` below that refuses the
deploy — correctly, but the reason never reaches the Kubernetes CRD status, so
`kubectl` shows only `Ready=Unknown`. Check the host log. (On Cosmonic
Desktop the reason does reach the workload's status message and event stream.)

## 5. Host resources are released by the garbage collector, not by scope

Every `wasmcloud:nats` resource — `message-handle`, `pull-consumer`, `bucket` —
arrives as a Go value whose host handle is dropped from a `runtime.AddCleanup`
finalizer. Inside one handler invocation that finalizer effectively never runs,
so a handle you stopped using is still *held* as far as the host is concerned.
It bites the pull worker: the host charges every held `message-handle` against
the binding's `subscription-capacity-bytes` (32 MiB default), and acking does
not release it. Measured on Cosmonic Desktop: the Go worker at 10,000 × 16 KiB
stalled at 958 delivered with every later `fetch` refused —
`LimitExceeded("the next message is larger than this fetch's 6054-byte bound; fetch without one is bound by what is left of the binding's subscription-capacity-bytes after the handles it still holds")`
— while the Rust worker, whose batch goes out of scope each round, delivered
10,000/10,000. Call `handle.Drop()` as soon as you are done with a handle (the
jetstream-worker template does it right after `Ack()`) and `defer
puller.Drop()` on the consumer.
