# Core Subscriber (go)

⚠️ **CAUTION** — Works, but sheds messages under load at stock settings. The campaign measured 60-77% loss at 10k-message bursts with the default subscription-capacity of 1024 (G6, G11).

Receive fire-and-forget core NATS messages on a subject and do work per message.

> **Go limitation:** a handler that parks on a timer traps. No `time.Sleep`,
> no `context.WithTimeout`, no retry/backoff. See [docs/limitations.md](docs/limitations.md).

## When to use this

You have a stream of events on a NATS subject and want a component invoked per message. No acknowledgement, no redelivery, no ordering guarantees — the cheapest possible consumer.

## When not to

Do not use this when losing a message matters. Core NATS has no ack and no redelivery: if the handler traps, or the subscription buffer overflows, the message is gone silently.

## Questions to dial it in

Answer these before you deploy — each one changes a config value, not code.

1. **What is your peak arrival rate, in messages/second, and how long can a burst last?**
   This sets `subscription-capacity`. The buffer holds MESSAGES, so the protection it buys is capacity/(arrival-drain) seconds. If you cannot answer, use JetStream instead.

2. **How long does one message take to handle, at p99?**
   Capacity must cover the whole burst if your handler is slower than arrival. A Go handler doing ~500 msg/s needed capacity > the full 10,000-message burst where Rust needed 64.

3. **Is losing a message acceptable?**
   If no, use `jetstream-consumer` instead. This pattern cannot promise delivery.

4. **Will more than one replica subscribe to the same subject?**
   Without a queue group every replica receives every message, so your work multiplies by replica count. Add a queue group to distribute.

## Measured operational envelope

Every number below came from the `nats-2.8-testing` campaign (186 cells against
the `wasmcloud:nats@0.1.0` driver on a 512Mi host). Full detail in
[docs/tuning.md](docs/tuning.md).

| load | Rust | Go |
|---|---|---|
| 1,000 msgs, 0 B, stock | CLEAN 1000/1000 | CLEAN 1000/1000 |
| 10,000 msgs, 0 B, stock | **CLEAN 10000/10000** | **LOSS 3,960/10,000** |
| 10,000 msgs, `capacity=65536` | CLEAN | **CLEAN 10000/10000** |
| 16 KiB x 10,000 | CLEAN, 110 Mi peak | CRASH — OOMKilled |

The single most important number: at stock `subscription-capacity` (1024), a
guest draining ~566 msg/s against ~2,000 msg/s arrival **shed 60%**. Raising
capacity above the burst width fixed it completely at every admission setting
tested. `max-in-flight` is *inert* here — sweeping it 1 -> 8192 moved delivery
by noise, because the buffer in front of the semaphore is what overflows.

## Known driver defects that affect this pattern

- **G6** — 'External publishers are safe at <=5k msg/s' is false for a slow guest — a Go consumer shed 60% at ~2,000 msg/s arrival.
- **G11** — `subscription-capacity` is the only knob that matters; `max-in-flight` is inert. Required value spans 1000x across guest languages.
- **G16** — Tuning the receiver's capacity relocates the bottleneck upstream to the publisher's own subscription.
- **G17** — For a slow guest JetStream delivered 100% where core push delivered 32%, at identical load.

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
├── skills/core-subscriber/SKILL.md
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
wash oci push --insecure oci.localhost:8200/nats-core-subscriber:0.1.0 core-subscriber.wasm

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
