# KV Watcher (Rust)

React to changes in a NATS KV bucket - put, delete, and purge events.

## When to use this

You want a component invoked whenever a key changes: cache invalidation,
config reload, projection updates, change-data-capture. The host maintains
the watch; you just handle events. Measured 100% watch delivery in every cell,
both languages, with flat memory - the watch itself costs effectively
nothing.

## When not to

As a work queue. Watch delivery follows KV semantics, not queue semantics: a
purge or a history-trimmed key can collapse several logical changes into one
event. And not when you only need the value now - a plain `get` is far
cheaper than a watch.

## Build

```bash
cargo build --release --target wasm32-wasip1     # or: wash build
# verify the export is really there:
wasm-tools component wit target/wasm32-wasip1/release/kv_watcher.wasm | grep 'export wasmcloud:nats/'
```

## Deploy

```bash
# Push the component wherever your cluster pulls from (Cosmonic Desktop's
# built-in registry shown), point `image:` in deploy/workload.yaml at it,
# and apply.
wash oci push --insecure oci-registry.localhost:8200/nats-kv-watcher:0.1.0 target/wasm32-wasip1/release/kv_watcher.wasm
kubectl apply -f deploy/workload.yaml
./scripts/e2e.sh        # drive one message, assert the effect
```

Rename every `nats-kv-watcher` occurrence when you fork, and narrow the grants -
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

- **Scope the prefix on the binding, not in code** (`kv-watches:
  bucket:filter`). A prefix that is too broad wakes the component on every
  unrelated change.
- **Handle deletes and purges.** Both arrive with an empty value and an
  operation marker; handlers that assume a value misbehave on them.
- **Be idempotent.** A watch can redeliver, and a restart replays from the
  bucket's current state rather than from where you left off.
- **The event carries the new value and a revision, not a diff.** Keep the
  prior value yourself, or read history, if you need before/after.
- **Writing back to the bucket at ≥1 MB values?** Carry the same
  `request-timeout-ms: "10000"` the kv-store pattern needs.

Measured envelope (details in [docs/tuning.md](docs/tuning.md); cross-pattern
guidance and the full error catalogue in
[`nats-tuning.md`](../../nats-tuning.md)):

| load | result |
|---|---|
| every KV cell measured, both languages | 100% watch delivery, no drops |
| watch cost | peak host memory flat |

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
├── skills/kv-watcher/SKILL.md
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
  name: "nats-kv-watcher"
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
      interfaces: [types, kv, jetstream, kv-handler]
      config:
        # ---- Grants: operator-declared ceilings, deny-by-default. --------
        # A workload may ask for a subset of what the hostgroup's
        # wasmcloud-nats plugin entry declares, never more. subject-allow
        # covers publish/request, stream-allow covers stream reads,
        # bucket-allow covers KV -- and they are separate: publishing to a
        # subject does not grant reading the stream that captures it.
        subject-allow: done.kv-watch.>
        bucket-allow: appkv
        # ---- Watches: bucket[:filter], comma separated. An omitted filter
        # is `>` (the whole bucket).
        kv-watches: appkv:>
        # Concurrent deliveries in flight (driver default 64). Each in-flight
        # delivery beyond the warm pool occupies its own fresh instance --
        # bound it on a small host (8 measured safe at 512Mi).
        #max-in-flight: "8"
        # Timeout for the driver's own NATS requests (publish acks above
        # all). The measured fix for ack timeouts on values/messages >=1 MB.
        #request-timeout-ms: "10000"
  components:
    - name: kv-watcher
      image: ghcr.io/your-org/nats-kv-watcher:0.1.0
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
