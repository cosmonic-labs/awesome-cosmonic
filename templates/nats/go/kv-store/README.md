# KV Store Client (go)

⚠️ **CAUTION** — Core operations (put/get/update/delete) are solid and measured CLEAN. But `history()` on a key with no retained history hung the guest call indefinitely in the reference campaign, pinning the instance (rust ledger D7).

Read and write a NATS KV bucket — get, put, CAS update, delete, history.

> **Go limitation:** a handler that parks on a timer traps. No `time.Sleep`,
> no `context.WithTimeout`, no retry/backoff. See [docs/limitations.md](docs/limitations.md).

## When to use this

You need durable key/value state that outlives an instance. NATS KV gives you revisions (so compare-and-swap works), history, and a watch channel other components can subscribe to.

## When not to

Do not treat it as a database. Listings are capped host-side, and there are no queries — only key lookups and prefix watches.

## Questions to dial it in

Answer these before you deploy — each one changes a config value, not code.

1. **What is your key naming scheme?**
   Keys are a flat namespace with `.`-delimited convention. Watches are prefix-based, so the scheme decides what can be watched independently.

2. **Do concurrent writers touch the same key?**
   If so use `update` with the expected revision (CAS) rather than `put`. A revision mismatch returns the current revision so you can retry without re-reading.

3. **How much history do you need?**
   The bucket's history depth is set at creation, not by the client. Depth 1 means no history at all — and see the `history()` caveat above.

4. **Does anything need to react to changes?**
   If yes, pair this with `kv-watcher` rather than polling.

## Measured operational envelope

Every number below came from the `nats-2.8-testing` campaign (186 cells against
the `wasmcloud:nats@0.1.0` driver on a 512Mi host). Full detail in
[docs/tuning.md](docs/tuning.md).

| operation | result |
|---|---|
| put / get / update (CAS) / delete | CLEAN, ~333 op/s serial |
| 20-op batch with watch | `ok=20 err=0 watch_receipts=20` |
| `history()` on a key with no history | **hangs the guest call indefinitely** |

Operations are serial per handler invocation, so throughput is bounded by
round-trip latency rather than by admission. The `history()` hang pins the
instance and its admission permit — treat that call as unsafe until fixed.

## Known driver defects that affect this pattern

- **G18** — Go's minimum component memory is 2.2x Rust's; a heap-floor refusal never reaches the Kubernetes CRD status.

## Layout

```
├── .wash/config.yaml    # wash v2 / Cosmonic Desktop project config
├── Makefile             # componentize-go build (see docs/building.md)
├── componentize-go.toml # world selection
├── deploy/workload.yaml # published-image manifest
├── docs/building.md     # toolchain workarounds you WILL need
├── docs/limitations.md  # the timer trap — read before writing a handler
├── docs/tuning.md       # measured operational envelope
├── export_.../handler.go # START HERE
├── go.mod, go.sum       # pkg v0.2.2 — what the pinned componentize-go generates against
├── scripts/e2e.sh       # drive one message through the deployment, assert the effect
├── skills/kv-store/SKILL.md
├── wit-vendor/          # vendored wasmcloud:nats@0.1.0 — the wkg.toml override target
├── wit/world.wit        # + deps/, written from wit-vendor by wash build
├── wkg.toml, wkg.lock   # override → wit-vendor (outside wit/deps, which wash build rewrites)
└── workload.yaml
```

## Build

```bash
make build      # make verify asserts the export and the async lift
```

`make bindings` (which `build` depends on) first runs
`go mod download go.bytecodealliance.org/pkg` — componentize-go starts with
`go list`, which needs `go.sum` — and may rewrite `go.mod`'s pkg version to
the one its generator was built against (`v0.2.2` at the pinned commit; commit
what it writes). `wash build` and Cosmonic Desktop's project mode run the
bare `componentize-go … build` from `.wash/config.yaml` instead, so they rely
on the committed `go.sum`. Details: [docs/building.md](docs/building.md).

## Deploy

```bash
# local iteration against Desktop's built-in registry (ingress host oci.localhost;
# oci.localhost.cosmonic.sh is the one name Windows can resolve)
wash oci push --insecure oci.localhost:8200/nats-kv-store:0.1.0 kv-store.wasm

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
