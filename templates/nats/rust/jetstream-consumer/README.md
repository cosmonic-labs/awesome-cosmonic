# JetStream Consumer (Rust)

Durable at-least-once consumption from a JetStream stream, with ack control.

## When to use this

You need delivery guarantees. JetStream retains messages, redelivers on
failure, and - critically - paces delivery by acknowledgement, so a slow
consumer is throttled instead of overrun. The safe default for anything
that matters.

## When not to

For latency-critical request paths (use `request-reply`), or anywhere you
assume exactly-once: redelivery is real, so handlers must be idempotent.

## Build

```bash
cargo build --release --target wasm32-wasip1     # or: wash build
# verify the export is really there:
wasm-tools component wit target/wasm32-wasip1/release/jetstream_consumer.wasm | grep 'export wasmcloud:nats/'
```

## Deploy

```bash
# Push the component wherever your cluster pulls from (Cosmonic Desktop's
# built-in registry shown), point `image:` in deploy/workload.yaml at it,
# and apply.
wash oci push --insecure oci-registry.localhost:8200/nats-jetstream-consumer:0.1.0 target/wasm32-wasip1/release/jetstream_consumer.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh        # drive one message, assert the effect
```

Rename every `nats-jetstream-consumer` occurrence when you fork, and narrow the grants -
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

- **Keep the queue group.** The fourth field of `jetstream-subscriptions`
  (shipped: `:workers`) makes the consumer **durable** - an ephemeral is
  server-deleted after 120 s of inactivity and delivery stops with no error
  logged - and distributes deliveries across replicas.
- **Be idempotent, and bound poison messages.** The delivery count and stream
  sequence arrive on the message handle for dedup keys; without `max-deliver`
  a deterministically-failing handler redelivers forever.
- **`ack-mode: auto`** (shipped) acks on a clean return and naks on error or
  trap; an explicit guest settle returns `ack-owned-by-host`
  (`in-progress` still works to extend ack-wait). Switch to `manual` for
  nak/term control - and always settle, or the consumer stalls for the full
  ack-wait.
- **Settling is one-shot only on success.** A settle the server accepted
  retires the handle (`already-settled` on a repeat = the work was done); a
  settle that failed on the wire leaves the handle usable - retrying it is
  correct.
- **At ≥1 MB**: give the NATS server memory first (4 Gi), set the stream's
  `max_msg_size` near real message size (the
  ack window derives from it), size `subscription-capacity` in messages
  against payload (64/32/16 at 1/2/5 MB), and set
  `request-timeout-ms: "10000"`. With those, this pattern measured clean at
  every size up to 5 MB.

Measured envelope (details in [docs/tuning.md](docs/tuning.md); cross-pattern
guidance and the full error catalogue in
[`nats-tuning.md`](../../nats-tuning.md)):

| load | result |
|---|---|
| 50,000 msgs, stock | CLEAN 50,000/50,000 |
| 1,000 msgs @ 16 KiB | CLEAN 1000/1000 @ 1,000 msg/s |
| 500 × 5 MB, tuned (server memory + durable + sized capacity) | CLEAN 500/500 |

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
├── skills/jetstream-consumer/SKILL.md
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
  name: "nats-jetstream-consumer"
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
      interfaces: [types, jetstream, jetstream-handler]
      config:
        # ---- Grants: operator-declared ceilings, deny-by-default. --------
        # A workload may ask for a subset of what the hostgroup's
        # wasmcloud-nats plugin entry declares, never more. subject-allow
        # covers publish/request, stream-allow covers stream reads,
        # bucket-allow covers KV -- and they are separate: publishing to a
        # subject does not grant reading the stream that captures it.
        subject-allow: load.>,done.js-sink.>
        stream-allow: LOAD
        # ---- Subscriptions: STREAM:filter[:policy[:queue]], comma separated.
        # policy: new (default) | all | last | last-per-subject; an empty
        # slot keeps the default (STREAM:filter::group). The queue group
        # makes the consumer DURABLE and distributes across replicas.
        jetstream-subscriptions: LOAD:load.push.>:new:workers
        # auto: host acks on ok, naks on error/trap. manual: the guest
        # drives the handle (ack/nak/term/in-progress).
        ack-mode: auto
        # Unsettled deliveries the server keeps in flight; x payload = bytes
        # resident. Derived from capacity-bytes / message size when unset
        # (floored at 16) -- set the stream's max_msg_size so it derives
        # correctly, or pin it here.
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
        # The measured fix for publish-ack timeouts at >=1 MB.
        #request-timeout-ms: "10000"
  components:
    - name: jetstream-consumer
      image: ghcr.io/your-org/nats-jetstream-consumer:0.1.0
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
