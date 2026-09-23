# nats-mcp-server-v1

A **Model Context Protocol (MCP) server for NATS**, built as a **WebAssembly
component** that runs on [Cosmonic Desktop] (wasmCloud v2). It gives an agent
25 tools across core NATS, JetStream, KV, and **diagnostics**.

The component reaches NATS through the host's `wasmcloud:nats@0.1.0` binding —
**not** over the network. It opens no sockets and holds no credentials. What it
may touch is decided entirely by grants in the Workload manifest, which the
guest cannot widen. Its outbound HTTP allow-list is empty unless you opt into
[server-wide telemetry](#server-wide-telemetry-optional), which adds exactly
one host.

## Tools

**Diagnostics** — verdicts rather than counters. Every one of these samples
twice, because a single reading cannot tell an idle stream from a filling one,
or a busy consumer from a drowning one.

| Tool | What it does |
|---|---|
| `nats_diagnose` | Sample, apply rules, return findings with **evidence and a remedy** each. Catches unbounded streams (with a time-to-fill estimate), consumers falling behind, ack saturation, redelivery loops, slow-consumer drops, storage pressure. Start here. |
| `jetstream_stream_rate` | Measured ingest: msgs/sec and bytes/sec from the sequence delta, so retention discarding does not mask live traffic. |
| `jetstream_consumer_lag` | `caught-up` / `draining` / `working` / `falling-behind` / `stalled`, with backlog trend and a drain estimate. |
| `nats_check_access` | Probes what this workload can actually reach. Grants are deny-by-default and the guest cannot read them, so this tries each stream and bucket and reports `granted` / `denied` with the grant key to widen. |

Every diagnostic report carries a **`capability`** block naming the rules that
could not run and the one change that would enable them — so a thin report is
never mistaken for a clean one.

**Server-wide** — needs the optional monitoring endpoint (see below).

| Tool | What it does |
|---|---|
| `nats_server_info` | Version, uptime, health, connection and subscription counts, slow-consumer drops, memory/CPU, JetStream usage against its limits. |
| `nats_server_streams` | Every stream with retention config, state, and consumers — **the inventory the binding cannot produce**, since it has no list call. |
| `nats_connections` | Connected clients: subscriptions held and `pending_bytes`, the number that identifies a slow consumer before the server drops it. |

**Core NATS**

| Tool | What it does |
|---|---|
| `nats_publish` | Fire-and-forget publish. Resolves on write to the connection, not on delivery. |
| `nats_request` | Request/reply with a timeout. Distinguishes `no-responders` from `timeout`. |

**JetStream**

| Tool | What it does |
|---|---|
| `jetstream_publish` | Durable publish, returns the stream ack (stream + sequence). `msg_id` gives idempotency. |
| `jetstream_scan` | Replay messages by sequence. Creates no consumer, acks nothing. |
| `jetstream_get_message` | Read one stored message by sequence. |
| `jetstream_stream_info` | Configured subjects, message/byte counts, first/last sequence. |
| `jetstream_list_subjects` | Subjects the stream *actually holds*, with counts. |
| `jetstream_consumer_info` | Consumer filters, provisioned limits, and live counters. |
| `jetstream_fetch` | Pull a batch from an existing consumer and settle it (`none`/`ack`/`nak`/`term`). |

**Key/Value**

| Tool | What it does |
|---|---|
| `kv_get` / `kv_put` / `kv_create` | Read, last-write-wins write, create-if-absent. |
| `kv_update` | Compare-and-swap on revision; a conflict reports the current revision. |
| `kv_delete` / `kv_purge` | Tombstone (keeps history) / remove history too. |
| `kv_keys` / `kv_history` / `kv_status` | List keys, all retained revisions, bucket state. |

### Reading vs. consuming

`jetstream_scan` and `jetstream_fetch` both return stream messages, and the
difference matters: **scan** is a stateless read — no consumer, no acks,
nothing moves. **fetch** drives a real pull consumer, and `settle: "ack"`
consumes messages permanently. `fetch` therefore defaults to `settle: "none"`,
which consumes nothing (at the cost of stalling the consumer for its
`ack_wait` before redelivery). To browse a stream, use `scan`.

### Payloads

NATS bodies are arbitrary bytes; JSON is not. So bodies round-trip as
`payload`/`value` (UTF-8 text) or `payload_base64`/`value_base64` (anything
else), and results come back as `{"text": …}` or `{"base64": …}` with the true
`bytes` count. Anything past `MCP_NATS_MAX_PAYLOAD_BYTES` (64 KiB default) is
truncated and flagged `truncated: true`, so a clipped payload is never
mistaken for a short one.

## Grants: the security boundary

Every subject, stream, and bucket is checked **host-side** before it reaches
NATS, and everything is denied by default. The three grants are separate on
purpose:

| Grant | Covers |
|---|---|
| `subject-allow` | publish + request subjects, and the subjects a stored JetStream message may be read from |
| `stream-allow` | JetStream stream reads |
| `bucket-allow` | KV buckets |

They live in the Workload's `wasmcloud:nats` `hostInterface` config, set by
whoever deploys the workload. A refusal comes back naming the grant that would
have to change, e.g.:

```
denied: subject "secrets.exfil" — no grant on this workload's binding covers
it. Widen the grant in the Workload's wasmcloud:nats hostInterface config —
that is an operator change, not something the server can do for you (the
relevant key is `subject-allow`)
```

Three things worth knowing before you widen anything:

- **Granting `>` hands every agent that can reach this server the run of the
  whole NATS deployment**, including whatever else shares it. Grant the
  subjects, streams, and buckets it actually needs.
- **A KV grant implies its backing stream.** `bucket-allow: demo-kv` also
  makes `KV_demo-kv` readable through the JetStream tools — same data, reached
  another way.
- **A consumer whose filter is broader than your grant is invisible.** A
  consumer filtered on `>` cannot be read under `subject-allow: demo.>`;
  provision it with a filter inside the grant.

Stream, consumer, and bucket **lifecycle is deliberately absent** — not
exported-and-denied. Provision them out-of-band with the `nats` CLI or your
deployment tooling; these tools read and write what already exists.

## Progressive capability

The binding has **no list call**, so the server cannot enumerate what exists
unless it is told or shown. Rather than require the richest source, it degrades
in steps and reports which step it is on:

| `capability.level` | Source | What runs |
|---|---|---|
| `caller-supplied` | names passed to the tool | consumer, backlog and pruning rules on what you named |
| `hinted` | `MCP_NATS_STREAMS` / `_CONSUMERS` / `_BUCKETS` | the same, with zero-argument calls working |
| `monitoring` | the NATS monitoring port | every stream on the server, plus retention and server-wide rules |

Nothing below `monitoring` requires an operator to open anything. The hint
variables restate names already in the `stream-allow` / `bucket-allow` grants —
duplication that exists because a guest cannot read its own grants, and it is
the only way a zero-argument `nats_diagnose` can know where to look without a
monitoring port. **Hints grant nothing:** a hinted name outside the grants is
still denied, which `nats_check_access` shows plainly.

Where the retention rules cannot run, the binding path substitutes
`stream-no-pruning-observed` — an inference from `first_sequence` never having
advanced, which is the closest evidence available without reading
configuration. The finding says it is an inference and names the command that
confirms it.

## Default route

`GET /` and `GET /health` return a JSON discovery document — server identity,
the MCP revision spoken, endpoint paths, tool names, and the skills served —
so a pasted URL, a load balancer probe, and a crawler all get something useful
instead of a 404.

It is answered before the transport, the request lock, and the body read, so a
health probe never queues behind an in-flight MCP exchange (a diagnostic tool
holds one for its whole sampling window). Every `POST` falls through to the MCP
transport unchanged.

The route sits outside the `MCP_ALLOWED_HOSTS` guard so probes work under any
`Host`. That is safe: it carries only what a successful `initialize` returns
anyway, and the component emits no CORS headers, so a browser page cannot read
it cross-origin.

## Skills over MCP

The server publishes a **skill** — a natural-language playbook telling a
connected agent when and how to use these tools — over the MCP resources
primitive (`io.modelcontextprotocol/skills`). Discovery is progressive:

1. `skill://index.json` — the catalog, read once at session start.
2. `skill://nats-mcp-server-v1/SKILL.md` — the playbook, read when a request
   matches its description.
3. `skill://nats-mcp-server-v1/references/TOOLS.md` — per-tool detail, pulled
   in only if the model needs that depth.

Files live under `skills/server/` and are embedded with `include_str!`, so the
component is self-contained and the playbook can never drift from the tools it
documents.

## Server-wide telemetry (optional)

Connection counts, slow consumers, and the stream inventory live behind `$SYS`
and the JetStream API. On the `wasmcloud:nats` binding both are **reserved for
the host** — a denial there says *"No grant can open this one"*. That is a
deliberate boundary, not a missing grant, so the four binding-only tools above
get this data from a second road instead: the NATS server's own monitoring
port, over plain HTTP.

It is off by default. Without it, `nats_server_*` and `nats_connections` return
the enablement steps and every other tool works unchanged; `nats_diagnose` and
`nats_check_access` fall back to examining only what you name.

To turn it on:

1. Start NATS with a monitoring port: `nats-server -m 8222`.
2. Point the workload at it and allow that one host:

```yaml
localResources:
  environment:
    config:
      MCP_NATS_MONITOR_URL: "http://192.168.1.43:8222"
  allowedHosts: ["192.168.1.43"]
```

**Picking the address is the fiddly part.** It must be one the component's
*outbound HTTP* path can resolve, and two tempting spellings do not work:

- `127.0.0.1` is the guest's own virtual network, never the machine.
- `host.wasmcloud.internal`, the reserved name for the machine's loopback, is
  resolved on the `wasi:sockets` path only. The `wasi:http` client used here
  builds a plain connector over the system resolver and never consults the
  reserved zone, so it fails with a DNS error (`address not available`) no
  matter what `allowedHostLoopbackPorts` says or whether the host's loopback
  switch is on.

`nats-server -m` binds every interface, so the machine's **LAN address** reaches
it — that is what the shipped manifests use. A LAN address moves with DHCP, so
prefer a stable hostname or fixed address anywhere but a laptop. A remote NATS
server needs nothing special: name its host and list it in `allowedHosts`.

The endpoints are read-only by construction — `/varz`, `/jsz`, `/connz`,
`/healthz` expose no mutating verbs.

## Layout

```
├── .cargo/config.toml   # default target wasm32-wasip2 + tokio_unstable cfg
├── .wash/config.yaml    # wash v2 / Cosmonic Desktop project config + grants
├── deploy/workload.yaml # deploy manifest for a published image
├── docs/auth.md         # authorization options for the Desktop use-case
├── scripts/smoke.sh     # end-to-end test of all 25 tools + the grant boundary
├── src/
│   ├── lib.rs           # wasi:http/handler export, streaming response pump
│   ├── bridge.rs        # tokio ↔ component-model-async bridge (job queue)
│   ├── nats.rs          # wasmcloud:nats bindings, views, and operations
│   ├── server.rs        # the 25 tools — start here
│   └── telemetry.rs     # tracing/OTEL wiring
├── wit/                 # wasmcloud:nats@0.1.0 WIT + the imports-only world
└── workload.yaml        # local-dev Workload manifest (built-in registry)
```

## Prerequisites

- Rust 1.94+ with the wasip2 target (Cosmonic Desktop's Preflight doctor
  provisions this)
- [Cosmonic Desktop] with its NATS plugin pointed at a server
  (Settings → Built-in plugins → NATS), JetStream enabled
- The `nats` CLI, to provision the demo resources

## Build

```console
$ cargo build --release
```

`.cargo/config.toml` defaults the target to `wasm32-wasip2`. The output at
`target/wasm32-wasip2/release/nats_mcp_server_v1.wasm` exports
`wasi:http/handler@0.3.0` and imports `wasmcloud:nats/{core,jetstream,kv}`
(verify with `wash inspect <path>`).

Because it imports `wasmcloud:nats`, **only a wasmCloud/Cosmonic host can run
it** — `wasmtime serve` cannot satisfy that import.

## Deploy

Provision the demo resources the shipped grants name:

```console
$ nats stream add DEMO --subjects 'demo.>' --storage file --defaults
$ nats consumer add DEMO worker --pull --deliver all --ack explicit \
    --filter 'demo.>' --defaults
$ nats kv add demo-kv --history=5 --storage file --defaults
```

Then push and apply:

```console
$ wash oci push --insecure \
    oci.localhost:8200/apps/nats-mcp-server-v1:0.2.0 \
    ./target/wasm32-wasip2/release/nats_mcp_server_v1.wasm
$ cosmonic workload apply workload.yaml
```

`workload.yaml` targets Desktop's built-in registry; `deploy/workload.yaml` is
the same shape for a published image. **Bump the image tag on every re-push** —
an applied Workload stays pinned to the digest a tag first resolved to, so
re-using `:0.2.0` keeps serving the old build.

The server is then at:

```
http://nats-mcp-server-v1.localhost.cosmonic.sh:8200/
```

### Talk to it

Point an MCP client at that URL, or call it directly. The stateless
2026-07-28 transport wants three things on every request: the `Mcp-Method`
header (plus `Mcp-Name` on `tools/call`), a client `_meta` block, and an
`Accept` that permits SSE — which is how replies come back.

```console
$ printf '%s' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
    "name":"jetstream_stream_info","arguments":{"stream":"DEMO"},
    "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
             "io.modelcontextprotocol/clientCapabilities":{}}}}' \
  | curl -sS -X POST http://127.0.0.1:8200/ \
      -H 'Host: nats-mcp-server-v1.localhost.cosmonic.sh' \
      -H 'Content-Type: application/json' \
      -H 'Accept: application/json, text/event-stream' \
      -H 'MCP-Protocol-Version: 2026-07-28' \
      -H 'Mcp-Method: tools/call' -H 'Mcp-Name: jetstream_stream_info' \
      --data-binary @-
```

## Testing

```console
$ ./scripts/smoke.sh              # against the deployed workload
$ ./scripts/smoke.sh --provision  # create the demo resources first
```

60 checks: the handshake, all 25 tools, the default route, skills over MCP, binary round-trip, CAS conflict
handling, the grant boundary, and 8 concurrent calls across the warm pool.

## How it works

The component hosts two async worlds on one thread, and they cannot await each
other directly:

- the **component-model** world — the `wasi:http` export and the
  `wasmcloud:nats` imports, driven by the host through the WASI p3 async ABI;
- the **tokio** world — `rmcp`'s protocol machinery and the tool functions.

`bridge.rs` is the crossing. Tool code calls `bridge::submit(work)`, which
queues a closure; the driver picks it up once it is back in component-model
context, runs it against the real host bindings, and resumes the tokio future
with plain data. That indirection is load-bearing: NATS resource handles
(`kv::Bucket`, `MessageHandle`) are neither `Send` nor valid outside
component-model context, so every operation in `nats.rs` opens what it needs,
uses it, and drops it before returning. No handle ever reaches a tool.

Two build details follow from this and are easy to get wrong:

- The NATS bindings **must** be generated by the same `wit-bindgen` the
  `wasip3` crate uses. The async canonical ABI keeps task state in that
  crate's runtime; a second, differently-versioned generator would give the
  NATS imports a different executor from the HTTP export and trap on the first
  `await`.
- `wit-bindgen`'s `inter-task-wakeup` feature is required. Without it, any
  outbound call traps under concurrent same-instance invocation.

Each instance serves one MCP exchange at a time, so concurrency comes from the
warm pool (`poolSize: 4`) rather than from inside an instance.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `MCP_ALLOWED_HOSTS` | localhost only | Host headers the transport accepts (DNS-rebinding guard). Must cover the ingress host, or every request 421s. |
| `MCP_NATS_MAX_PAYLOAD_BYTES` | `65536` | Largest body rendered into a tool result before truncation. |
| `MCP_NATS_MONITOR_URL` | unset (off) | NATS monitoring base URL, e.g. `http://192.168.1.43:8222`. Enables the server-wide tools; see [above](#server-wide-telemetry-optional) for why the address cannot be `127.0.0.1` or `host.wasmcloud.internal`. |
| `MCP_NATS_STREAMS` | unset | Comma-separated stream names for zero-argument diagnostics. See [Progressive capability](#progressive-capability). |
| `MCP_NATS_CONSUMERS` | unset | Comma-separated `stream/consumer` pairs, same purpose. |
| `MCP_NATS_BUCKETS` | unset | Comma-separated KV bucket names, same purpose. |
| `RUST_LOG` | `info` | Log filter. |

Caller-supplied timeouts are clamped to 30s: while a NATS call is in flight
the instance is inside component-model context with the request lock held, so
a timeout is a lease on the whole instance. Diagnostic sampling windows
(`sample_ms`) are clamped to 15s for the same reason.

## Observability

`tracing` spans with structured JSON logs on stderr, which the host exports as
OTLP log records. Each tool is instrumented with its subject/stream/bucket, so
a denial or timeout is attributable. Build with `--features wasi-otel` for
native host-joined traces on a host that implements `wasi:otel`.

## License

Apache-2.0. See [LICENSE](LICENSE).

[Cosmonic Desktop]: https://cosmonic.com/docs/desktop
[mcp-server-template-rs]: https://github.com/cosmonic-labs/mcp-server-template-rs
[`rmcp`]: https://github.com/modelcontextprotocol/rust-sdk
[2026-07-28 MCP specification]: https://modelcontextprotocol.io/specification/2026-07-28
[`wasi:http/handler@0.3.0`]: https://github.com/WebAssembly/wasi-http
