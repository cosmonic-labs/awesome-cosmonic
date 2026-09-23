# Fan-Out / Amplifier (Go)

Receive one message and republish it to many - the classic scatter pattern.

> **Go note:** a handler must not park on a Go runtime timer - `time.Sleep`,
> `time.After`, `context.WithTimeout` all trap the instance. Await the host
> clock (`wasi:clocks/monotonic-clock@0.3.0`) instead; details in
> [docs/limitations.md](docs/limitations.md).

## When to use this

One input event needs to become many units of downstream work: notify N
subscribers, shard a job, trigger a parallel pipeline. Fan-out is a
composition - something else consumes what it produces - and sizing that
downstream is most of the job.

## When not to

Without sizing capacity and memory first. The downstream arrival rate is
`input rate × fan-out factor`, and at large payloads resident memory is
`fan-out × payload` - both are quantified below, and both have bitten at
stock settings.

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
wash oci push --insecure oci-registry.localhost:8200/nats-fan-out:0.1.0 fan-out.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh        # drive one message, assert the effect
```

Rename every `nats-fan-out` occurrence when you fork, and narrow the grants -
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

- **Size every hop the burst traverses.** Raising only the receiver's
  capacity relocates the loss upstream to the amplifier's own input
  subscription. Measured at 2,000 in × 25: receiver capacity `65536` took
  delivery to 50,000/50,000 clean once the amplifier's input was sized too.
- **The shipped `poolSize: 1` + `max-in-flight: "8"` pair keeps overload
  survivable**: under overload the host sheds visibly (`shed_total=`
  warnings) and stays up. Pair any *large* `max-in-flight` with host memory
  sized for it - each in-flight delivery beyond the warm pool occupies its
  own instance.
- **Size host memory from the fan-out factor at large payloads.** Measured
  ×25 peaks: 860 Mi at 1 MB, 1,360 Mi at 2 MB, 2,676 Mi at 5 MB - roughly
  linear in payload. Give ×25 at 1 MB a 2 Gi pod.
- **Prefer a JetStream downstream.** It absorbs the burst durably and paces
  delivery; core simply drops, and a slow-consumer drop is reported without
  subject or count - instrument your own receipts if you stay on core.

Measured envelope (details in [docs/tuning.md](docs/tuning.md); cross-pattern
guidance and the full error catalogue in
[`nats-tuning.md`](../../nats-tuning.md)):

| shape | result |
|---|---|
| 2,000 in × 25 = 50,000 out, capacity `65536` sized at every hop | CLEAN 50,000/50,000 |
| ×25 at 1/2/5 MB payloads | peaks 860/1,360/2,676 Mi - size the pod by the factor |

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
├── skills/fan-out/SKILL.md
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
  name: "nats-fan-out"
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
      interfaces: [types, core, core-handler]
      config:
        # ---- Grants: operator-declared ceilings, deny-by-default. --------
        # A workload may ask for a subset of what the hostgroup's
        # wasmcloud-nats plugin entry declares, never more. subject-allow
        # covers publish/request, stream-allow covers stream reads,
        # bucket-allow covers KV -- and they are separate: publishing to a
        # subject does not grant reading the stream that captures it.
        subject-allow: fan.in,fan.work
        # ---- Subscriptions: subject[:queue], comma separated. ------------
        core-subscriptions: fan.in
        # Concurrent deliveries in flight. Each in-flight delivery beyond
        # the warm pool occupies its own fresh instance, so resident memory
        # scales with max-in-flight x instance footprint. 8 measured clean at
        # 1,000 msg/s on a 512Mi host; raise only alongside host memory
        # sized for it.
        max-in-flight: "8"
        # Size the RECEIVER's capacity to the whole fan-out burst, and this
        # amplifier's own input capacity to the input burst -- tuning only
        # one hop moves the loss to the other.
        #subscription-capacity: "65536"
        # Byte-denominated twin (default 32 MiB); binds first at >=1 MB.
        #subscription-capacity-bytes: "33554432"
        # Timeout for the driver's own NATS requests (publish acks above
        # all). The measured fix for ack timeouts on values/messages >=1 MB.
        #request-timeout-ms: "10000"
  components:
    - name: fan-out
      image: ghcr.io/your-org/nats-fan-out:0.1.0
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
