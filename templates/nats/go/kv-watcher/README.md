# KV Watcher (go)

✅ **RECOMMENDED** — Measured 100% watch delivery across every KV cell in the campaign — `ok=20 err=0 watch_receipts=20` with no drops, in both languages.

React to changes in a NATS KV bucket — put, delete, and purge events.

> **Go limitation:** a handler that parks on a timer traps. No `time.Sleep`,
> no `context.WithTimeout`, no retry/backoff. See [docs/limitations.md](docs/limitations.md).

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
├── skills/kv-watcher/SKILL.md
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
wash oci push --insecure oci.localhost:8200/nats-kv-watcher:0.1.0 kv-watcher.wasm

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
