---
name: core-subscriber-rust
description: Build, configure, and deploy a Core Subscriber component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Rust. Use when the user wants to receive fire-and-forget core NATS messages on a subject and do work per message.
---

# Core Subscriber (Rust)

## When this pattern is the right answer

You have a stream of events on a NATS subject and want a component invoked per message. No acknowledgement, no redelivery, no ordering guarantees - the cheapest possible consumer.

**When it is not:** Do not use this when losing a message matters. Core NATS has no ack and no redelivery: if the handler traps, or the subscription buffer overflows, the message is gone silently.

## Phase 1 - dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **What is your peak arrival rate, in messages/second, and how long can a burst last?** This sets `subscription-capacity`. The buffer holds MESSAGES, so the protection it buys is capacity/(arrival-drain) seconds. If you cannot answer, use JetStream instead.
2. **How long does one message take to handle, at p99?** Capacity must cover the whole burst if your handler is slower than arrival. A Go handler doing ~500 msg/s needed capacity > the full 10,000-message burst where Rust needed 64.
3. **Is losing a message acceptable?** If no, use `jetstream-consumer` instead. This pattern cannot promise delivery.
4. **Will more than one replica subscribe to the same subject?** Without a queue group every replica receives every message, so your work multiplies by replica count. Add a queue group to distribute.

## Phase 2 - non-negotiable guardrails

1. **Grants are deny-by-default and separate.** `subject-allow` covers publish
   and request, `stream-allow` covers stream reads, `bucket-allow` covers KV.
   Publishing to a subject does not grant reading the stream capturing it.
2. **Size `subscription-capacity` to the burst, not the average.** It is
   denominated in messages, so the protection it buys is
   `capacity / (arrival - drain)` seconds. Sizing it from throughput alone is
   how people lose data.
3. **The component must export the async P3 interface.** Verify with
   `wasm-tools component wit <component>.wasm` - a component that builds but
   exports nothing is a real and common failure mode.
4. **wit-bindgen must be 0.60+ with `async-spawn`.** Every function in
   `wasmcloud:nats@0.1.0` is an `async func`; a sync-signature function cannot
   be lifted with the async canonical ABI.

## Phase 3 - build, deploy, verify

```bash
wash build
wash oci push --insecure oci-registry.localhost:8200/nats-core-subscriber:0.1.0 target/wasm32-wasip1/release/core_subscriber.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh
```

## Pitfalls (read before writing code)

- **A handler that always fails will retry forever** on JetStream - the
  redelivery ladder cannot tell "transient" from "always broken". Bound it with
  `max-deliver`.
- **Adding replicas does not increase buffer capacity.** Each replica gets its
  own subscription and its own buffer, and without a queue group each also gets
  every message. Scaling out a shedding consumer makes throughput no better and
  stability worse.
- **A drop is reported as `subscription=N` with no count and no subject.** You
  cannot attribute loss from the log alone; instrument your own receipts.
- **Changing a config value rebinds the workload**, and the previous consumer
  can keep delivering for up to 120s - expect duplicates around a config change.

## Reference

Extracted from the `nats-2.8-testing` campaign: 186 measured cells against this
driver, in both Rust and Go. See `docs/tuning.md` for this pattern's envelope.
