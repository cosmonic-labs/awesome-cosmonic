# JetStream Pull Worker (Rust)

Guest-paced batch processing - you decide when and how much to fetch.

## When to use this

You want to control the pace and batch size rather than have the host push at
you: expensive per-batch work, rate-limited downstreams, anything that
amortizes setup across a batch. The safest pattern at large payloads - the
worker sets the pace.

## When not to

For latency-critical paths: the fetch round-trip adds delay a push consumer
does not pay.

## Build

```bash
cargo build --release --target wasm32-wasip1     # or: wash build
# verify the export is really there:
wasm-tools component wit target/wasm32-wasip1/release/jetstream_worker.wasm | grep 'export wasmcloud:nats/'
```

## Deploy

```bash
# Push the component wherever your cluster pulls from (Cosmonic Desktop's
# built-in registry shown), point `image:` in deploy/workload.yaml at it,
# and apply.
wash oci push --insecure oci-registry.localhost:8200/nats-jetstream-worker:0.1.0 target/wasm32-wasip1/release/jetstream_worker.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh        # drive one message, assert the effect
```

Rename every `nats-jetstream-worker` occurrence when you fork, and narrow the grants -
they ship deny-by-default and intentionally minimal.

## Deploying on Cosmonic Control

On Cosmonic Control the `wasmcloud-nats` host plugin's `workloadConfig`
defaults to `deny`: grants live in the hostgroup's values —
`hostPlugins: [{id: wasmcloud-nats, config: {subject-allow, stream-allow,
bucket-allow}}]` — and a workload manifest that carries its own grants is
refused at deploy. Before applying this template's manifest to Control, strip
`subject-allow` / `stream-allow` / `bucket-allow` from `deploy/workload.yaml`
(keep only the subscriptions and behaviour keys), and wrap it as a
`WorkloadDeployment` (`kind: WorkloadDeployment`, spec under
`.spec.template.spec` — replicas are the operator's). On Cosmonic Desktop the
manifest works as shipped.

## Performance and tuning

- **`fetch(batch)` materializes `batch × message size` in host memory.**
  Size the batch to the payload: the default is fine at kilobytes, use
  **batch 4 at 1-2 MB and 1-5 at 5 MB** - with that one change, pull ran
  clean at every size measured. Use `fetch-with-limits` to add a byte bound.
- **Close fetched batches when done** - settling the messages does not
  release the batch handle.
- **A refused fetch has two causes with different fixes.** Over the
  consumer's provisioned limits: `info` reports the limits to size against.
  Or already-fetched messages hold the binding's whole memory budget: drop
  the `message-handle`s from earlier batches - acking one does not release
  it. Either way, retrying unchanged fails the same way.
- **Decide the empty-stream behaviour up front.** `fetch` returns a stop
  reason - drained, batch-filled, byte-limit - so the loop can exit, back
  off, or keep polling deliberately.
- **The trigger is a core delivery** (one per run), so the delivery-side
  knobs matter less here; the memory story is the fetch itself.

Measured envelope (details in [docs/tuning.md](docs/tuning.md); cross-pattern
guidance and the full error catalogue in
[`nats-tuning.md`](../../nats-tuning.md)):

| load | result |
|---|---|
| 1,000 msgs @ 16 KiB | CLEAN 1000/1000 @ 1,000 msg/s |
| 500 × 1-5 MB, batch 4 | CLEAN 500/500 at every size |
| 5 MB messages, `fetch(100)` | 500 MB in one call - size the batch instead |

## Layout

```
├── Cargo.toml               # wit-bindgen 0.60, async features
├── .cargo/config.toml       # wasm32-wasip1 target
├── wkg.toml                 # resolves wasmcloud:nats from the vendored WIT
├── src/lib.rs               # START HERE
├── .wash/config.yaml        # wash v2 / Cosmonic Desktop project config
├── .github/workflows/ci.yml # build + verify the async export is really there
├── README.md
├── LICENSE
├── deploy/workload.yaml     # the manifest - set `image:`, then kubectl apply
├── docs/tuning.md           # measured operational envelope
├── scripts/e2e.sh           # drive one message, assert the effect
├── skills/jetstream-worker/SKILL.md
└── wit/world.wit            # + deps/
```

## All tuning options

Every knob this template understands, in one commented manifest. Uncommented
values are the shipped defaults; commented keys show the driver default and
when to reach for them.

```yaml
apiVersion: runtime.wasmcloud.dev/v1alpha1
kind: Workload
metadata:
  name: "nats-jetstream-worker"
  namespace: default
spec:
  # Each replica gets its own subscription and its own buffer. Replicas do
  # not add buffer -- and without a queue group, each receives its own full
  # copy of the traffic.
  #replicas: 1
  hostInterfaces:
    - namespace: wasmcloud
      package: nats
      version: "0.1.0"
      interfaces: [types, jetstream, core-handler]
      config:
        # ---- Grants: operator-declared ceilings, deny-by-default. --------
        # A workload may ask for a subset of what the hostgroup's
        # wasmcloud-nats plugin entry declares, never more. subject-allow
        # covers publish/request, stream-allow covers stream reads,
        # bucket-allow covers KV -- and they are separate: publishing to a
        # subject does not grant reading the stream that captures it.
        subject-allow: pull.run,done.js-pull.>
        stream-allow: LOAD
        # ---- Trigger: subject[:queue], comma separated. One core message
        # starts a fetch loop.
        core-subscriptions: pull.run
        # Unsettled pull deliveries the server allows; x payload = bytes
        # resident.
        #max-ack-pending: "16"
        # Redelivery bound for a message that always fails.
        #max-deliver: "5"
        # Concurrent deliveries in flight (driver default 64). Each in-flight
        # delivery beyond the warm pool occupies its own fresh instance --
        # bound it on a small host (8 measured safe at 512Mi).
        #max-in-flight: "8"
        # Per-subscription buffer, in MESSAGES. Size it to the BURST, not the
        # rate: the protection it buys is capacity/(arrival-drain) seconds.
        # The shed warning prints `would_have_absorbed=` -- the capacity that
        # would have worked for that window.
        #subscription-capacity: "1024"
        # Per-subscription buffer, in BYTES (default 32 MiB). At >=1 MB
        # payloads this binds first: it admits capacity/payload messages.
        #subscription-capacity-bytes: "33554432"
        # Timeout for the driver's own NATS requests (publish acks above
        # all). The measured fix for ack timeouts on values/messages >=1 MB.
        #request-timeout-ms: "10000"
  components:
    - name: jetstream-worker
      image: ghcr.io/your-org/nats-jetstream-worker:0.1.0
      # Warm instances reused across deliveries. 1 keeps a single instance
      # warm; raise it for latency-sensitive or high-rate small-payload work,
      # and leave it low for payloads >=1 MB (each warm instance retains its
      # heap; memory cost ~ poolSize x payload). Reuse is a state contract:
      # package-level state survives across deliveries (treat it as a cache;
      # the shipped handler is reuse-safe). Unset or 0 declares state
      # ephemeral: a fresh instance per delivery, guaranteed.
      poolSize: 1
      # Calls one warm instance may have in flight at once. Default 1 = a
      # guest sees one call at a time. Raise only for a guest that yields
      # while it waits.
      #maxConcurrency: 1
      # Calls one instance serves before it is retired and replaced.
      # 0 = unlimited. Set a bound if package-level state should decay.
      maxInvocations: 0
      localResources:
        # This component's own outbound-HTTP allowlist (wasi:http). Empty
        # denies every outbound host; NATS traffic flows through the host
        # binding, not HTTP.
        allowedHosts: []
```
