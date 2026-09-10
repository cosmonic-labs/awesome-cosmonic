---
name: request-reply-rust
description: Build, configure, and deploy a Request / Reply component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Rust. Use when the user wants to answer NATS requests - an RPC endpoint that scales to zero between calls.
---

# Request / Reply (Rust)

## When this pattern is the right answer

You want a service other components or clients call and wait on. The host delivers the request, you publish the answer to the requester's reply subject. Per-request instantiation means it costs nothing when idle.

**When it is not:** Do not use it for work longer than the caller's timeout, and do not use it for fire-and-forget notifications - a reply nobody awaits is wasted work.

## Phase 1 - dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **What is the caller's timeout?** Your p99 handling time must sit well inside it. The caller sees a timeout, not an error, if you are slow.
2. **Does the reply need to carry failure detail?** Core NATS has no error channel. Returning an error from the handler is logged host-side and the caller just times out. Put failures in the reply body/headers (the NATS micro convention uses `Nats-Service-Error` headers).
3. **How many concurrent requests at peak?** This sets `max-in-flight`. Unlike the subscriber patterns, admission is the real limit here because each request occupies an instance until it replies.
4. **Do you need more than one replica?** Add a queue group so requests round-robin. Verified CLEAN at 3 replicas.

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
wash oci push --insecure oci-registry.localhost:8200/nats-request-reply:0.1.0 target/wasm32-wasip1/release/request_reply.wasm
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
