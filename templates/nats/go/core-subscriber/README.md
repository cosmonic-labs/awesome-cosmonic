# Core Subscriber (Go)

Receive fire-and-forget core NATS messages on a subject and do work per message.

> **Go note:** a handler must not park on a Go runtime timer - `time.Sleep`,
> `time.After`, `context.WithTimeout` all trap the instance. Await the host
> clock (`wasi:clocks/monotonic-clock@0.3.0`) instead; details in
> [docs/limitations.md](docs/limitations.md).

## When to use this

You have a stream of events on a NATS subject and want a component invoked per
message - telemetry, events, cache invalidation. No acknowledgement, no
redelivery, no ordering guarantees: the cheapest possible consumer.

## When not to

When losing a message matters. Core NATS has no ack and no redelivery: if the
handler traps, or the subscription buffer overflows, the message is gone
silently. Use `jetstream-consumer` for delivery guarantees: JetStream paces delivery
by acknowledgement, so a slow consumer is throttled instead of overrun.

## Build

```bash
make build     # componentize-go build (see docs/building.md)
make verify    # assert the handler export and the async ABI are really there
```

## Deploy

```bash
# Push the component wherever your cluster pulls from (Cosmonic Desktop's
# built-in registry shown), point `image:` in deploy/workload.yaml at it,
# and apply.
wash oci push --insecure oci-registry.localhost:8200/nats-core-subscriber:0.1.0 core-subscriber.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh        # drive one message, assert the effect
```

Rename every `nats-core-subscriber` occurrence when you fork, and narrow the grants -
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

- **Size `subscription-capacity` to the burst, not the rate.** It is
  denominated in messages, so the protection it buys is
  `capacity ÷ (arrival − drain)` seconds. Stock is 1024; a handler slower
  than arrival needs capacity above the whole burst - `65536` delivered a
  10,000-message burst completely. The shed warning prints
  `would_have_absorbed=` - the host computing the right value for you.
- **The shipped `poolSize: 1` + `max-in-flight: "8"` pair is deliberate.**
  Deliveries run on the instance pool, and each in-flight delivery beyond the
  warm pool occupies its own fresh instance; the pair measured clean at
  1,000 msg/s on a 512 Mi host. Raise `max-in-flight` only alongside host
  memory sized for it.
- **Required capacity varies enormously with drain rate** - about 1000×
  between a fast and a slow handler at the same load. Measure with your
  handler, not a placeholder.
- **Replicas do not add buffer.** Each replica gets its own subscription and,
  without a queue group, its own full copy of the traffic. Distribute with
  `core-subscriptions: subject:group`.

Measured envelope (details in [docs/tuning.md](docs/tuning.md); cross-pattern
guidance and the full error catalogue in
[`nats-tuning.md`](../../nats-tuning.md)):

| load | result |
|---|---|
| 1,000 msgs, stock | CLEAN 1000/1000 @ 1,000 msg/s |
| 10,000 msgs, `subscription-capacity: "65536"` | CLEAN 10000/10000 |

## Layout

```
├── Makefile                 # componentize-go build
├── componentize-go.toml     # world selection
├── docs/building.md         # toolchain setup
├── docs/limitations.md      # the timer trap - read before writing a handler
├── export_.../handler.go    # START HERE
├── .wash/config.yaml        # wash v2 / Cosmonic Desktop project config
├── .github/workflows/ci.yml # build + verify the async export is really there
├── README.md
├── LICENSE
├── deploy/workload.yaml     # the manifest - set `image:`, then kubectl apply
├── docs/tuning.md           # measured operational envelope
├── scripts/e2e.sh           # drive one message, assert the effect
├── skills/core-subscriber/SKILL.md
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
  name: "nats-core-subscriber"
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
        subject-allow: bench.core,done.core-sink.>
        # ---- Subscriptions: subject[:queue], comma separated. ------------
        # Add a queue group to round-robin across replicas instead of every
        # replica receiving every message.
        core-subscriptions: bench.core
        # Concurrent deliveries in flight. Each in-flight delivery beyond
        # the warm pool occupies its own fresh instance, so resident memory
        # scales with max-in-flight x instance footprint. 8 measured clean at
        # 1,000 msg/s on a 512Mi host; raise only alongside host memory
        # sized for it.
        max-in-flight: "8"
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
    - name: core-subscriber
      image: ghcr.io/your-org/nats-core-subscriber:0.1.0
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
