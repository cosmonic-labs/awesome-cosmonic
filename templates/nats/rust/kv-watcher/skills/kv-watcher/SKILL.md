---
name: kv-watcher-rust
description: Build, configure, and deploy a KV Watcher component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Rust. Use when the user wants to react to changes in a NATS KV bucket — put, delete, and purge events.
---

# KV Watcher (Rust)

✅ **RECOMMENDED** — Measured 100% watch delivery across every KV cell in the campaign — `ok=20 err=0 watch_receipts=20` with no drops, in both languages.

## When this pattern is the right answer

You want a component invoked whenever a key changes — cache invalidation, config reload, projection updates, change-data-capture. The host maintains the watch; you just handle events.

**When it is not:** Do not use it as a work queue. Watch delivery follows KV semantics, not queue semantics, and a purge or a history-trimmed key can collapse several logical changes into one event.

## Phase 1 — dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **Which key prefix do you need to watch?** The watch is configured on the binding, not in code. A prefix that is too broad wakes your component on every unrelated change.
2. **Do you care about deletes and purges, or only writes?** Delete and purge events arrive with an empty value and an operation marker. Handlers that assume a value will misbehave on them.
3. **Is the handler idempotent?** A watch can redeliver, and a restart replays from the bucket's current state rather than from where you left off.
4. **Do you need the previous value?** The event carries the new value and a revision, not a diff. If you need before/after, keep the prior value yourself or read history.

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
wash oci push --insecure oci.localhost:8200/nats-kv-watcher:0.1.0 target/wasm32-wasip2/release/kv_watcher.wasm
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
