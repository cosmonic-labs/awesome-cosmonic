# Fan-Out / Amplifier (go)

🚧 **NEEDS WORK** — Dangerous at stock settings. Fan-out over core NATS lost 72% of messages in Rust and 92% in Go at a 25x amplification of a 1,000-message input — the in-host republish outruns any consumer's buffer (G4, G6).

Receive one message and republish it to many — the classic scatter pattern.

> **Go limitation:** a handler that parks on a timer traps. No `time.Sleep`,
> no `context.WithTimeout`, no retry/backoff. See [docs/limitations.md](docs/limitations.md).

## When to use this

One input event needs to become many units of downstream work: notify N subscribers, shard a job, or trigger a parallel pipeline.

## When not to

Do not use it with core publish at scale without reading the warning below. This is the pattern that produced the campaign's largest data loss.

## Questions to dial it in

Answer these before you deploy — each one changes a config value, not code.

1. **What is your fan-out factor, and what is the input rate?**
   Multiply them. That product is the downstream arrival rate, and it is what overruns the receiver. 1,000 in x 25 = 25,000 out, which shed 72-92% at defaults.

2. **Can the downstream be JetStream instead of core?**
   Strongly preferred. JetStream absorbs the burst durably and paces delivery; core simply drops. This single change turns the pattern from lossy to safe.

3. **If it must be core, what capacity does the RECEIVER need?**
   Size it to the whole fan-out burst, not the input. And size the amplifier's OWN input subscription too — tuning only the receiver moves the loss upstream (G16).

4. **Do you need to know what was dropped?**
   You cannot, today. Core publish is fire-and-forget and slow-consumer drops are reported as an unnamed subscription id with no count. Budget for that blindness or use JetStream.

## Measured operational envelope

Every number below came from the `nats-2.8-testing` campaign (186 cells against
the `wasmcloud:nats@0.1.0` driver on a 512Mi host). Full detail in
[docs/tuning.md](docs/tuning.md).

| shape | Rust | Go |
|---|---|---|
| 200 in x 25 = 5,000 out, stock | LOSS 2,180 (56% lost) | LOSS 1,257 (75% lost) |
| 1,000 in x 25 = 25,000 out, stock | **LOSS 6,972 (72% lost)** | **LOSS 2,073 (92% lost)** |
| 2,000 in x 25 = 50,000 out, `capacity=65536` | **CLEAN 50,000/50,000** | LOSS 46,425 (7% lost, upstream) |

Read the last row carefully. Raising the *receiver's* capacity fixed Rust
completely and took Go from 6% delivery to 93% — the remaining 7% was the
**amplifier's own input subscription** shedding, which no receiver-side knob
touches. Capacity must be sized at every hop a burst traverses.

## Known driver defects that affect this pattern

- **G4** — Fan-out over core loses 72% (Rust) / 92% (Go) of messages at stock settings.
- **G6** — 'External publishers are safe at <=5k msg/s' is false for a slow guest — a Go consumer shed 60% at ~2,000 msg/s arrival.
- **G16** — Tuning the receiver's capacity relocates the bottleneck upstream to the publisher's own subscription.

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
├── skills/fan-out/SKILL.md
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
wash oci push --insecure oci.localhost:8200/nats-fan-out:0.1.0 fan-out.wasm

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
