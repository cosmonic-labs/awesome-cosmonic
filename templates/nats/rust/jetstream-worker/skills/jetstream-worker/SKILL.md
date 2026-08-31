---
name: jetstream-worker-rust
description: Build, configure, and deploy a JetStream Pull Worker component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Rust. Use when the user wants to guest-paced batch processing - you decide when and how much to fetch.
---

# JetStream Pull Worker (Rust)

## When this pattern is the right answer

You want to control the pace and batch size rather than have the host push at you. Good for expensive per-batch work, rate-limited downstreams, and anything that benefits from amortizing setup across a batch.

**When it is not:** Do not use plain `fetch(batch)` on a stream with large messages. See the warning below - it is the single most dangerous call in this interface.

## Phase 1 - dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **How large can one message be?** MULTIPLY IT BY YOUR BATCH SIZE. `fetch(100)` on a stream of 5 MB messages asks the host for 500 MB in one call. Use `fetch-with-limits` and set a byte bound.
2. **What is a useful batch size for your work?** Batches amortize setup. Too large and you risk the memory above; too small and you lose the advantage over a push consumer.
3. **What should the worker do when the stream is empty?** `fetch` returns a stop reason - drained, batch-filled, or byte-limit. Decide whether to exit, back off, or keep polling.
4. **Who triggers a run?** This template is trigger-driven: a core message starts a fetch loop. Alternatives are a timer (NOT available to Go guests - see the Go template's limitation note) or a long-running loop.

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
wash oci push --insecure oci-registry.localhost:8200/nats-jetstream-worker:0.1.0 target/wasm32-wasip1/release/jetstream_worker.wasm
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
