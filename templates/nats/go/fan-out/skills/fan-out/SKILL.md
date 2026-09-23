---
name: fan-out-go
description: Build, configure, and deploy a Fan-Out / Amplifier component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Go. Use when the user wants to receive one message and republish it to many - the classic scatter pattern.
---

# Fan-Out / Amplifier (Go)

## When this pattern is the right answer

One input event needs to become many units of downstream work: notify N subscribers, shard a job, or trigger a parallel pipeline.

**When it is not:** Do not use it with core publish at scale without reading the warning below. This is the pattern that produced the campaign's largest data loss.

## Phase 1 - dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **What is your fan-out factor, and what is the input rate?** Multiply them. That product is the downstream arrival rate, and it is what overruns the receiver. 1,000 in x 25 = 25,000 out, which shed 72-92% at defaults.
2. **Can the downstream be JetStream instead of core?** Strongly preferred. JetStream absorbs the burst durably and paces delivery; core simply drops. This single change turns the pattern from lossy to safe.
3. **If it must be core, what capacity does the RECEIVER need?** Size it to the whole fan-out burst, not the input. And size the amplifier's OWN input subscription too - tuning only the receiver moves the loss upstream.
4. **Do you need to know what was dropped?** You cannot, today. Core publish is fire-and-forget and slow-consumer drops are reported as an unnamed subscription id with no count. Budget for that blindness or use JetStream.

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
4. **Never park on a timer.** `time.Sleep`, `time.After`, `context.WithTimeout`
   and anything built on them trap the instance. This is measured and
   reproduces on every toolchain tested. If you need a delay, reconsider the
   design - a timer-free handler is the only shape that works today.
5. **Build with `make`, not bare `componentize-go`.** The Makefile passes `-w`
   to stop the SDK's default wasip2 world (which mandates an HTTP export) from
   being merged, and vendors the wasi deps that `-w` then drops.

## Phase 3 - build, deploy, verify

```bash
make build
wash oci push --insecure oci-registry.localhost:8200/nats-fan-out:0.1.0 fan-out.wasm
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
