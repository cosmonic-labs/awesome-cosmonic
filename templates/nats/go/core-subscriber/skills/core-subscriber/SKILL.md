---
name: core-subscriber-go
description: Build, configure, and deploy a Core Subscriber component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Go. Use when the user wants to receive fire-and-forget core NATS messages on a subject and do work per message.
---

# Core Subscriber (Go)

⚠️ **CAUTION** — Works, but sheds messages under load at stock settings. The campaign measured 60-77% loss at 10k-message bursts with the default subscription-capacity of 1024 (G6, G11).

## When this pattern is the right answer

You have a stream of events on a NATS subject and want a component invoked per message. No acknowledgement, no redelivery, no ordering guarantees — the cheapest possible consumer.

**When it is not:** Do not use this when losing a message matters. Core NATS has no ack and no redelivery: if the handler traps, or the subscription buffer overflows, the message is gone silently.

## Phase 1 — dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **What is your peak arrival rate, in messages/second, and how long can a burst last?** This sets `subscription-capacity`. The buffer holds MESSAGES, so the protection it buys is capacity/(arrival-drain) seconds. If you cannot answer, use JetStream instead.
2. **How long does one message take to handle, at p99?** Capacity must cover the whole burst if your handler is slower than arrival. A Go handler doing ~500 msg/s needed capacity > the full 10,000-message burst where Rust needed 64.
3. **Is losing a message acceptable?** If no, use `jetstream-consumer` instead. This pattern cannot promise delivery.
4. **Will more than one replica subscribe to the same subject?** Without a queue group every replica receives every message, so your work multiplies by replica count. Add a queue group to distribute.

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
wash oci push --insecure oci.localhost:8200/nats-core-subscriber:0.1.0 core-subscriber.wasm
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
