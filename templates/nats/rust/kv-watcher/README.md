# KV Watcher (rust)

✅ **RECOMMENDED** — Measured 100% watch delivery across every KV cell in the campaign — `ok=20 err=0 watch_receipts=20` with no drops, in both languages.

React to changes in a NATS KV bucket — put, delete, and purge events.

## When to use this

You want a component invoked whenever a key changes — cache invalidation, config reload, projection updates, change-data-capture. The host maintains the watch; you just handle events.

## When not to

Do not use it as a work queue. Watch delivery follows KV semantics, not queue semantics, and a purge or a history-trimmed key can collapse several logical changes into one event.

## Questions to dial it in

Answer these before you deploy — each one changes a config value, not code.

1. **Which key prefix do you need to watch?**
   The watch is configured on the binding, not in code. A prefix that is too broad wakes your component on every unrelated change.

2. **Do you care about deletes and purges, or only writes?**
   Delete and purge events arrive with an empty value and an operation marker. Handlers that assume a value will misbehave on them.

3. **Is the handler idempotent?**
   A watch can redeliver, and a restart replays from the bucket's current state rather than from where you left off.

4. **Do you need the previous value?**
   The event carries the new value and a revision, not a diff. If you need before/after, keep the prior value yourself or read history.

## Measured operational envelope

Every number below came from the `nats-2.8-testing` campaign (186 cells against
the `wasmcloud:nats@0.1.0` driver on a 512Mi host). Full detail in
[docs/tuning.md](docs/tuning.md).

| load | result |
|---|---|
| 20 KV writes | 20 watch events delivered (100%) |
| across every KV cell in the campaign | no drops observed |

Watch delivery was the one thing that never lost an event in either language.
Peak host memory stayed flat — the watch itself costs effectively nothing.

## Known driver defects that affect this pattern

- No pattern-specific defects found.

## Layout

```
├── .cargo/config.toml   # wasm32-wasip2 target — cargo emits the component itself
├── .wash/config.yaml    # wash v2 / Cosmonic Desktop project config
├── Cargo.toml           # wit-bindgen 0.60 with async-spawn
├── deploy/workload.yaml # published-image manifest
├── docs/tuning.md       # measured operational envelope
├── scripts/e2e.sh       # drive one message through the deployment, assert the effect
├── skills/kv-watcher/SKILL.md
├── src/lib.rs           # START HERE
├── wit-vendor/          # vendored wasmcloud:nats@0.1.0 — the wkg.toml override target
├── wit/world.wit        # the world this component targets (+ deps/, written from wit-vendor)
├── wkg.toml, wkg.lock   # override → wit-vendor (outside wit/deps, which wash build rewrites)
└── workload.yaml        # local-dev manifest
```

## Build

```bash
wash build      # or: cargo build --release --target wasm32-wasip2 — the same component
```

Either way the artifact is `target/wasm32-wasip2/release/kv_watcher.wasm`. `wash build` also
re-materialises `wit/deps/` from `wit-vendor/` through `wkg.toml` (the
override lives outside `wit/deps` on purpose: wkg empties that directory
on every build). Cosmonic Desktop's project mode runs the same
`cargo build` from `.wash/config.yaml` and reads the same path.

## Deploy

```bash
# local iteration against Desktop's built-in registry (ingress host oci.localhost;
# oci.localhost.cosmonic.sh is the one name Windows can resolve)
wash oci push --insecure oci.localhost:8200/nats-kv-watcher:0.1.0 target/wasm32-wasip2/release/kv_watcher.wasm

# apply workload.yaml — there is no kubectl on Cosmonic Desktop. Pick one:
#   app     Workloads → New workload → paste workload.yaml
#   agent   the cosmonic MCP server's `cosmonic_apply_workload` tool, manifest as its argument
#   shell   POST the manifest as JSON to the daemon's unix socket (`cosmonicd paths` prints it;
#           macOS: ~/Library/Application Support/Cosmonic/cosmonicd.sock,
#           Linux: $XDG_RUNTIME_DIR/cosmonic/cosmonicd.sock)
SOCK="$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
yq -o=json . workload.yaml | curl -sS --unix-socket "$SOCK" -X POST \
  -H 'content-type: application/json' --data-binary @- http://localhost/v1/workloads
curl -sS --unix-socket "$SOCK" http://localhost/v1/workloads | jq '.[] | .status.state'   # → running

# then drive it (a local `nats` CLI against nats://127.0.0.1:4222; see the script header)
./scripts/e2e.sh
```

## Grants

The manifest is deny-by-default and lists only what this pattern needs.
`subject-allow` covers publish and request, `stream-allow` covers stream reads,
`bucket-allow` covers KV — permission to publish to a subject does **not**
carry permission to read a stream capturing it.
