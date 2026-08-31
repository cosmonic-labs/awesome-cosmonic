# Request / Reply (Go)

Answer NATS requests - an RPC endpoint that scales to zero between calls.

> **Go note:** a handler must not park on a Go runtime timer - `time.Sleep`,
> `time.After`, `context.WithTimeout` all trap the instance. Await the host
> clock (`wasi:clocks/monotonic-clock@0.3.0`) instead; details in
> [docs/limitations.md](docs/limitations.md).

## When to use this

You want a service other components or clients call and wait on. The host
delivers the request; you publish the answer to the requester's reply subject.
It costs nothing when idle, and it was clean at every payload size measured,
up to 5 MB, on stock settings.

## When not to

When the work outlasts the caller's timeout - the caller sees a timeout, not
an error - or for fire-and-forget notifications, where a reply nobody awaits
is wasted work.

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
wash oci push --insecure oci-registry.localhost:8200/nats-request-reply:0.1.0 request-reply.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh        # drive one message, assert the effect
```

Rename every `nats-request-reply` occurrence when you fork, and narrow the grants -
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

- **`poolSize` is the latency knob.** Cold instantiation per request is a
  Go cost above all: Go measured p50 3,190 µs cold where Rust measured 458 µs
  at the same 10,000 req/s (a Rust instance is ~1 MiB and far cheaper to
  build). `poolSize: 8` took Go to **433 µs**, and even the shipped
  `poolSize: 1` cut Go p50 38%. Reuse means package-level state persists
  between requests - treat it as a cache.
- **`max-in-flight` is the admission limit** - each request occupies an
  instance until it replies. Callers that wait for the answer self-limit; a
  fire-hose of callers does not, so bound it on a small host.
- **Core NATS has no error channel.** A handler error is logged host-side and
  the caller just times out. Put failure detail in the reply body or headers
  (the NATS micro convention uses `Nats-Service-Error`).
- **At ≥1 MB, raise the caller's timeout to 5-10 s** - a large reply takes
  longer to move.
- **Scale out with a queue group** so requests round-robin: clean at 1, 2,
  and 3 replicas with no duplication.

Measured envelope (details in [docs/tuning.md](docs/tuning.md); cross-pattern
guidance and the full error catalogue in
[`nats-tuning.md`](../../nats-tuning.md)):

| load | result |
|---|---|
| 5,000 requests, 1-3 replicas (queue group) | CLEAN at every replica count |
| 10,000 req/s, `poolSize: 8` (Go) | p50 433 µs (Rust cold at the same rate: 458 µs) |
| 1,000 requests, `poolSize: 1` (Go) | CLEAN, p50 734 µs |
| 500 × 5 MB requests, stock | CLEAN 500/500 |

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
├── skills/request-reply/SKILL.md
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
  name: "nats-request-reply"
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
        # _INBOX.> is what lets this component publish replies.
        subject-allow: svc.echo,_INBOX.>
        # ---- Subscriptions: subject[:queue], comma separated. ------------
        # Add :group so multiple replicas round-robin requests.
        core-subscriptions: svc.echo
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
    - name: request-reply
      image: ghcr.io/your-org/nats-request-reply:0.1.0
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
