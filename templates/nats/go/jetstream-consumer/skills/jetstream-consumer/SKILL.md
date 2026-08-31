---
name: jetstream-consumer-go
description: Build, configure, and deploy a JetStream Consumer component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Go. Use when the user wants to durable at-least-once consumption from a JetStream stream, with ack control.
---

# JetStream Consumer (Go)

✅ **RECOMMENDED** — The safest pattern under load by a wide margin. At an identical 50,000-message burst, core push delivered 32% while JetStream delivered 100% — every JetStream cell in the hostile layer was CLEAN (G17).

## When this pattern is the right answer

You need delivery guarantees. JetStream retains messages, redelivers on failure, and — critically — paces delivery by acknowledgement, so a slow consumer is throttled instead of overrun.

**When it is not:** Do not use it for latency-critical request paths, and do not assume exactly-once. Redelivery is real; handlers must be idempotent.

## Phase 1 — dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **Is your handler idempotent?** At-least-once means the same message can arrive twice. If reprocessing is harmful you need a dedup key — the delivery count and stream sequence are both available on the message handle.
2. **Auto-ack or manual?** Auto acks on a clean return, which is right for most handlers. Manual gives you nak/term control for poison-message handling, at the cost of stalling the consumer for the full ack-wait if you forget to settle.
3. **What should happen to a message that always fails?** Without a `max-deliver` bound, a deterministically-failing handler retries forever and the consumer never advances. Set one, or terminate explicitly.
4. **Will you scale this out?** Read the queue-group caveat in docs/scaling.md first. In the tested build, JetStream queue groups do NOT distribute — every replica receives the whole stream.

## Phase 2 — non-negotiable guardrails

1. **Grants are deny-by-default and separate.** `subject-allow` covers publish
   and request, `stream-allow` covers stream reads, `bucket-allow` covers KV.
   Publishing to a subject does not grant reading the stream capturing it.
2. **Size `subscription-capacity` to the burst, not the average.** It is
   denominated in messages, so the protection it buys is
   `capacity / (arrival - drain)` seconds. Sizing it from throughput alone is
   how people lose data.
3. **The component must export the async P3 interface.** Verify with
   `wasm-tools component wit <component>.wasm` — a component that builds but
   exports nothing is a real and common failure mode.
4. **Never park on a timer.** `time.Sleep`, `time.After`, `context.WithTimeout`
   and anything built on them trap the instance. This is measured and
   reproduces on every toolchain tested. If you need a delay, reconsider the
   design — a timer-free handler is the only shape that works today.
5. **Build with `make`, not bare `componentize-go`.** The Makefile passes `-w`
   to stop the SDK's default wasip2 world (which mandates an HTTP export) from
   being merged, and vendors the wasi deps that `-w` then drops.

## Phase 3 — build, deploy, verify

```bash
make build          # then: make verify
wash oci push --insecure oci.localhost:8200/nats-jetstream-consumer:0.1.0 jetstream-consumer.wasm
# Apply workload.yaml — there is no kubectl on Cosmonic Desktop. From an agent,
# call the cosmonic MCP tool `cosmonic_apply_workload` with the manifest; from
# a shell, POST it as JSON to the daemon's unix socket (README.md → Deploy has
# the recipe); in the app, Workloads → New workload → paste it.
./scripts/e2e.sh    # local `nats` CLI against nats://127.0.0.1:4222 (NATS= overrides)
```

## Pitfalls (read before writing code)

- **A handler that always fails will retry forever** on JetStream — the
  redelivery ladder cannot tell "transient" from "always broken". Bound it with
  `max-deliver`.
- **Adding replicas does not increase buffer capacity.** Each replica gets its
  own subscription and its own buffer, and without a queue group each also gets
  every message. Scaling out a shedding consumer makes throughput no better and
  stability worse.
- **A drop is reported as `subscription=N` with no count and no subject.** You
  cannot attribute loss from the log alone; instrument your own receipts.
- **Changing a config value rebinds the workload**, and the previous consumer
  can keep delivering for up to 120s — expect duplicates around a config change.

## Reference

Extracted from the `nats-2.8-testing` campaign: 186 measured cells against this
driver, in both Rust and Go. See `docs/tuning.md` for this pattern's envelope.
