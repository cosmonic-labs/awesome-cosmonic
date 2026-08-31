# JetStream Pull Worker (rust)

🚧 **NEEDS WORK** — The pattern is sound but ships next to a live footgun: plain `fetch(batch)` materializes batch x message_size in host memory and OOM-killed the host at 5 MB messages in BOTH languages, taking every co-tenant workload down with it (F7).

Guest-paced batch processing — you decide when and how much to fetch.

## When to use this

You want to control the pace and batch size rather than have the host push at you. Good for expensive per-batch work, rate-limited downstreams, and anything that benefits from amortizing setup across a batch.

## When not to

Do not use plain `fetch(batch)` on a stream with large messages. See the warning below — it is the single most dangerous call in this interface.

## Questions to dial it in

Answer these before you deploy — each one changes a config value, not code.

1. **How large can one message be?**
   MULTIPLY IT BY YOUR BATCH SIZE. `fetch(100)` on a stream of 5 MB messages asks the host for 500 MB in one call. Use `fetch-with-limits` and set a byte bound.

2. **What is a useful batch size for your work?**
   Batches amortize setup. Too large and you risk the memory above; too small and you lose the advantage over a push consumer.

3. **What should the worker do when the stream is empty?**
   `fetch` returns a stop reason — drained, batch-filled, or byte-limit. Decide whether to exit, back off, or keep polling.

4. **Who triggers a run?**
   This template is trigger-driven: a core message starts a fetch loop. Alternatives are a timer (NOT available to Go guests — see the Go template's limitation note) or a long-running loop.

## Measured operational envelope

Every number below came from the `nats-2.8-testing` campaign (186 cells against
the `wasmcloud:nats@0.1.0` driver on a 512Mi host). Full detail in
[docs/tuning.md](docs/tuning.md).

| load | result |
|---|---|
| 1 MB messages, `fetch(100)` | CLEAN 100/100 |
| **5 MB messages, `fetch(100)`** | **CRASH — host OOMKilled, 0/50, both languages** |
| 25 MB messages, `fetch(100)` | CRASH (known) |

`fetch(100)` on 5 MB messages asks the host to materialize **500 MB** in one
call. It killed the host — and every co-tenant workload's connection with it —
identically in Rust and Go, which is what proves it is a driver-side issue and
not a guest one. Use `fetch-with-limits` with a byte bound.

## Known driver defects that affect this pattern

- **G21** — Plain `fetch(batch)` on jumbo messages OOM-kills the host in both languages.

## Layout

```
├── .cargo/config.toml   # wasm32-wasip2 target — cargo emits the component itself
├── .wash/config.yaml    # wash v2 / Cosmonic Desktop project config
├── Cargo.toml           # wit-bindgen 0.60 with async-spawn
├── deploy/workload.yaml # published-image manifest
├── docs/tuning.md       # measured operational envelope
├── scripts/e2e.sh       # drive one message through the deployment, assert the effect
├── skills/jetstream-worker/SKILL.md
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

Either way the artifact is `target/wasm32-wasip2/release/jetstream_worker.wasm`. `wash build` also
re-materialises `wit/deps/` from `wit-vendor/` through `wkg.toml` (the
override lives outside `wit/deps` on purpose: wkg empties that directory
on every build). Cosmonic Desktop's project mode runs the same
`cargo build` from `.wash/config.yaml` and reads the same path.

## Deploy

```bash
# local iteration against Desktop's built-in registry (ingress host oci.localhost;
# oci.localhost.cosmonic.sh is the one name Windows can resolve)
wash oci push --insecure oci.localhost:8200/nats-jetstream-worker:0.1.0 target/wasm32-wasip2/release/jetstream_worker.wasm

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
