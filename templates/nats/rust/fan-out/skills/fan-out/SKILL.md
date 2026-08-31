---
name: fan-out-rust
description: Build, configure, and deploy a Fan-Out / Amplifier component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Rust. Use when the user wants to receive one message and republish it to many — the classic scatter pattern.
---

# Fan-Out / Amplifier (Rust)

🚧 **NEEDS WORK** — Dangerous at stock settings. Fan-out over core NATS lost 72% of messages in Rust and 92% in Go at a 25x amplification of a 1,000-message input — the in-host republish outruns any consumer's buffer (G4, G6).

## When this pattern is the right answer

One input event needs to become many units of downstream work: notify N subscribers, shard a job, or trigger a parallel pipeline.

**When it is not:** Do not use it with core publish at scale without reading the warning below. This is the pattern that produced the campaign's largest data loss.

## Phase 1 — dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **What is your fan-out factor, and what is the input rate?** Multiply them. That product is the downstream arrival rate, and it is what overruns the receiver. 1,000 in x 25 = 25,000 out, which shed 72-92% at defaults.
2. **Can the downstream be JetStream instead of core?** Strongly preferred. JetStream absorbs the burst durably and paces delivery; core simply drops. This single change turns the pattern from lossy to safe.
3. **If it must be core, what capacity does the RECEIVER need?** Size it to the whole fan-out burst, not the input. And size the amplifier's OWN input subscription too — tuning only the receiver moves the loss upstream (G16).
4. **Do you need to know what was dropped?** You cannot, today. Core publish is fire-and-forget and slow-consumer drops are reported as an unnamed subscription id with no count. Budget for that blindness or use JetStream.

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
4. **wit-bindgen must be 0.60+ with `async-spawn`.** Every function in
   `wasmcloud:nats@0.1.0` is an `async func`; a sync-signature function cannot
   be lifted with the async canonical ABI.

## Phase 3 — build, deploy, verify

```bash
wash build          # or: cargo build --release --target wasm32-wasip2 — the same component
wash oci push --insecure oci.localhost:8200/nats-fan-out:0.1.0 target/wasm32-wasip2/release/fan_out.wasm
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
