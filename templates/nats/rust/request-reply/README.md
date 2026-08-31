# Request / Reply (rust)

✅ **RECOMMENDED** — The most robust pattern measured. CLEAN at every replica count (1, 2, 3) and every load tested, including 5,000-request runs. Core queue groups distribute correctly, so it scales horizontally without duplication.

Answer NATS requests — an RPC endpoint that scales to zero between calls.

## When to use this

You want a service other components or clients call and wait on. The host delivers the request, you publish the answer to the requester's reply subject. Per-request instantiation means it costs nothing when idle.

## When not to

Do not use it for work longer than the caller's timeout, and do not use it for fire-and-forget notifications — a reply nobody awaits is wasted work.

## Questions to dial it in

Answer these before you deploy — each one changes a config value, not code.

1. **What is the caller's timeout?**
   Your p99 handling time must sit well inside it. The caller sees a timeout, not an error, if you are slow.

2. **Does the reply need to carry failure detail?**
   Core NATS has no error channel. Returning an error from the handler is logged host-side and the caller just times out. Put failures in the reply body/headers (the NATS micro convention uses `Nats-Service-Error` headers).

3. **How many concurrent requests at peak?**
   This sets `max-in-flight`. Unlike the subscriber patterns, admission is the real limit here because each request occupies an instance until it replies.

4. **Do you need more than one replica?**
   Add a queue group so requests round-robin. Verified CLEAN at 3 replicas.

## Measured operational envelope

Every number below came from the `nats-2.8-testing` campaign (186 cells against
the `wasmcloud:nats@0.1.0` driver on a 512Mi host). Full detail in
[docs/tuning.md](docs/tuning.md).

| load | result |
|---|---|
| 5,000 requests, 1 replica | CLEAN 5000/5000 |
| 5,000 requests, 2 replicas (queue group) | CLEAN 5000/5000 |
| 5,000 requests, 3 replicas (queue group) | CLEAN 5000/5000 |

Core queue groups distribute correctly — delivery stays at exactly the request
count as replicas scale, rather than multiplying. This was the only pattern
CLEAN at every replica count tested, in both languages.

## Known driver defects that affect this pattern

- **G14** — Per-replica memory cost is ~13x higher for Go than Rust; un-grouped replicas multiply the load.

## Layout

```
├── .cargo/config.toml   # wasm32-wasip2 target — cargo emits the component itself
├── .wash/config.yaml    # wash v2 / Cosmonic Desktop project config
├── Cargo.toml           # wit-bindgen 0.60 with async-spawn
├── deploy/workload.yaml # published-image manifest
├── docs/tuning.md       # measured operational envelope
├── scripts/e2e.sh       # drive one message through the deployment, assert the effect
├── skills/request-reply/SKILL.md
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

Either way the artifact is `target/wasm32-wasip2/release/request_reply.wasm`. `wash build` also
re-materialises `wit/deps/` from `wit-vendor/` through `wkg.toml` (the
override lives outside `wit/deps` on purpose: wkg empties that directory
on every build). Cosmonic Desktop's project mode runs the same
`cargo build` from `.wash/config.yaml` and reads the same path.

## Deploy

```bash
# local iteration against Desktop's built-in registry (ingress host oci.localhost;
# oci.localhost.cosmonic.sh is the one name Windows can resolve)
wash oci push --insecure oci.localhost:8200/nats-request-reply:0.1.0 target/wasm32-wasip2/release/request_reply.wasm

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
