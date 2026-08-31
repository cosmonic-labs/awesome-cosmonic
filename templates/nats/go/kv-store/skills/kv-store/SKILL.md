---
name: kv-store-go
description: Build, configure, and deploy a KV Store Client component on Cosmonic Desktop against the wasmcloud:nats@0.1.0 driver, in Go. Use when the user wants to read and write a NATS KV bucket — get, put, CAS update, delete, history.
---

# KV Store Client (Go)

⚠️ **CAUTION** — Core operations (put/get/update/delete) are solid and measured CLEAN. But `history()` on a key with no retained history hung the guest call indefinitely in the reference campaign, pinning the instance (rust ledger D7).

## When this pattern is the right answer

You need durable key/value state that outlives an instance. NATS KV gives you revisions (so compare-and-swap works), history, and a watch channel other components can subscribe to.

**When it is not:** Do not treat it as a database. Listings are capped host-side, and there are no queries — only key lookups and prefix watches.

## Phase 1 — dial it in before writing code

Every one of these changes configuration, not code. Ask them first.

1. **What is your key naming scheme?** Keys are a flat namespace with `.`-delimited convention. Watches are prefix-based, so the scheme decides what can be watched independently.
2. **Do concurrent writers touch the same key?** If so use `update` with the expected revision (CAS) rather than `put`. A revision mismatch returns the current revision so you can retry without re-reading.
3. **How much history do you need?** The bucket's history depth is set at creation, not by the client. Depth 1 means no history at all — and see the `history()` caveat above.
4. **Does anything need to react to changes?** If yes, pair this with `kv-watcher` rather than polling.

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
wash oci push --insecure oci.localhost:8200/nats-kv-store:0.1.0 kv-store.wasm
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
