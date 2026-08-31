# JetStream Consumer (go)

✅ **RECOMMENDED** — The safest pattern under load by a wide margin. At an identical 50,000-message burst, core push delivered 32% while JetStream delivered 100% — every JetStream cell in the hostile layer was CLEAN (G17).

Durable at-least-once consumption from a JetStream stream, with ack control.

> **Go limitation:** a handler that parks on a timer traps. No `time.Sleep`,
> no `context.WithTimeout`, no retry/backoff. See [docs/limitations.md](docs/limitations.md).

## When to use this

You need delivery guarantees. JetStream retains messages, redelivers on failure, and — critically — paces delivery by acknowledgement, so a slow consumer is throttled instead of overrun.

## When not to

Do not use it for latency-critical request paths, and do not assume exactly-once. Redelivery is real; handlers must be idempotent.

## Questions to dial it in

Answer these before you deploy — each one changes a config value, not code.

1. **Is your handler idempotent?**
   At-least-once means the same message can arrive twice. If reprocessing is harmful you need a dedup key — the delivery count and stream sequence are both available on the message handle.

2. **Auto-ack or manual?**
   Auto acks on a clean return, which is right for most handlers. Manual gives you nak/term control for poison-message handling, at the cost of stalling the consumer for the full ack-wait if you forget to settle.

3. **What should happen to a message that always fails?**
   Without a `max-deliver` bound, a deterministically-failing handler retries forever and the consumer never advances. Set one, or terminate explicitly.

4. **Will you scale this out?**
   Read the queue-group caveat in docs/scaling.md first. In the tested build, JetStream queue groups do NOT distribute — every replica receives the whole stream.

## Measured operational envelope

Every number below came from the `nats-2.8-testing` campaign (186 cells against
the `wasmcloud:nats@0.1.0` driver on a 512Mi host). Full detail in
[docs/tuning.md](docs/tuning.md).

| load | core push | **JetStream push** |
|---|---|---|
| 50,000 msgs, stock knobs | LOSS 15,785 (68% lost) | **CLEAN 50,000/50,000** |
| 50,000 msgs, `mif=8192` | LOSS 15,551 | **CLEAN 50,000/50,000** |
| 2,000 msgs @ 36 KiB | (not run) | **CLEAN 2,000/2,000** |

Every JetStream cell in the hostile layer was CLEAN — all 12 of them. The
reason is structural: JetStream paces delivery by *settlement*, so a slow
consumer is throttled rather than overrun. That backpressure is what core push
lacks.

## Known driver defects that affect this pattern

- **G10** — A push-consumer rebuild across a disconnect drops exactly the delivery in flight — 4/4 occurrences across two languages.
- **G15** — JetStream queue groups do NOT distribute — every replica gets the whole stream, because the durable name embeds replica identity.
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
├── skills/jetstream-consumer/SKILL.md
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
wash oci push --insecure oci.localhost:8200/nats-jetstream-consumer:0.1.0 jetstream-consumer.wasm

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
