# Adobe After Effects MCP server for Cosmonic Desktop

An [MCP](https://modelcontextprotocol.io) server that lets AI agents drive
Adobe After Effects — create compositions and layers, set keyframes and
expressions, apply effects — running as a sandboxed WebAssembly component on
[Cosmonic Desktop](https://cosmonic.com).

A Rust port of [Dakkshin/after-effects-mcp](https://github.com/Dakkshin/after-effects-mcp),
rearchitected for the Wasm sandbox: the original exchanged JSON files in
`~/Documents/ae-mcp-bridge`, which a sandboxed component cannot reach. Here the
transport is inverted — the After Effects panel polls the server over HTTP
instead, and no filesystem access is needed at all.

Built from the [Cosmonic MCP server template](https://github.com/cosmonic-labs/mcp-server-template-rs):
the official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK over
the streamable HTTP transport, exporting `wasi:http/handler@0.3.0` (WASI p3),
stateless per the 2026-07-28 MCP specification. Same shape as its sibling
[`illustrator-mcp`](../adobe-illustrator-mcp).

## Architecture

A sandboxed component cannot open After Effects, run AppleScript, or reach an
app-local socket — so the call is inverted: a panel running *inside* After
Effects polls the component for work.

```
MCP client ──tools/call──▶ ae-mcp (Wasm) ──queue──▶ wasi:keyvalue
                               ▲                        │
                               └──result── panel ◀──poll─┘
                                  (runs inside After Effects)
```

The panel is `bridge/mcp-bridge-auto.jsx`, a ScriptUI/ExtendScript window that
polls `GET /bridge/command` every ~2 s and posts results to
`POST /bridge/result`. A tool call queues a command and waits up to ~12 s
(4 minutes for batches, frame renders, and project saves) for the panel to
execute it, so most calls return the outcome directly. If the panel is closed
or slow, the call says so specifically and the result can be fetched later with
`get-results`.

## Setup

### 1. Build and deploy the server (Cosmonic Desktop must be running)

```bash
cargo build --target wasm32-wasip2 --release
wash oci push --insecure oci.localhost:8200/apps/ae-mcp:0.3.0 \
  target/wasm32-wasip2/release/after_effects_mcp.wasm
# then apply deploy/workload.yaml (e.g. via the cosmonic MCP server's
# cosmonic_apply_workload, or POST /v1/workloads)

# Verify — the default route reports identity, tools, skills, and whether
# the After Effects panel is actually connected:
curl -s -H 'Host: ae-mcp.localhost.cosmonic.sh' http://127.0.0.1:8200/
```

### 2. Install the After Effects bridge panel (the install step)

```bash
./install-bridge.sh
```

This copies `bridge/mcp-bridge-auto.jsx` into
`/Applications/Adobe After Effects <version>/Scripts/ScriptUI Panels/`
(falling back to `sudo` if needed). Then, in After Effects:

1. Settings → Scripting & Expressions → enable **Allow Scripts to Write Files
   and Access Network**, and restart After Effects.
2. Open **Window → mcp-bridge-auto.jsx** and leave the panel open. It shows
   its connection state and a log of executed commands.

### 3. Register the server with an MCP client

```bash
claude mcp add --transport http after-effects \
  http://ae-mcp.localhost.cosmonic.sh:8200/mcp
```

The transport is stateless — no sessions, no affinity — so the workload scales
out with `poolSize` alone.

## Tools

| Tool | Purpose |
|---|---|
| `get-help` | Usage guide, effect match names, templates |
| `bridge-status` | Is the AE panel connected and polling? |
| `get-results` | Fetch the last command's result |
| `get-project-info` / `list-compositions` / `get-layer-info` | Read project state |
| `create-composition` | New comp (size, duration, frame rate, bg color) |
| `create-text-layer` / `create-shape-layer` / `create-solid-layer` | New layers |
| ↳ shape styling | `roundness` (corner radius), `dash: [len, gap]` (dotted outlines), `fillOpacity`, `fillNone`, `strokeOpacity` |
| `set-layer-properties` | Position/scale/rotation/opacity/timing/text |
| `set-layer-keyframe` / `set-layer-expression` | Animation |
| `apply-effect` / `apply-effect-template` | Effects (by match name or preset template) |
| **`run-batch`** | **Run many commands in one round trip, in a single undo group — use this for anything non-trivial** |
| `save-frame-png` | Render a frame to PNG (visual verification) |
| `save-project` / `delete-composition` | Project management |
| `add-image-layer` | Import a PNG/JPEG/TIFF (**not WebP**) and place it, sized by target height/width |
| `set-composition-properties` | Duration, frame rate, dimensions, background colour |
| `run-script` | Advanced allowlisted scripts (`createCamera`, `duplicateLayer`, `deleteLayer`, `setLayerMask`, `batchSetLayerProperties`, `setCompositionProperties`, `bridgeTestEffects`) |

### Always batch

The bridge costs one ~2s poll cycle per command, so latency dominates anything
built one layer at a time. Building this repo's 279-command hero animation took
**~10 minutes** as individual calls and **~5 seconds** via `run-batch`.

Two rules when batching:
- Keep a batch to roughly 60 commands.
- **Never retry a `run-batch` call that timed out.** The panel cannot poll while
  it is executing, so a retry queues the same mutations a second time. Set the
  client timeout above the server's wait (240s) instead.

Batches get slower as a project accumulates expression-driven layers — a batch
that takes 2s in an empty project can take minutes once several hundred animated
layers exist. Delete superseded comps rather than letting them pile up.

### Prefer `compName` over `compIndex`

Project item indices shift whenever footage is imported, so a comp index captured
at the start of a build goes stale the moment `add-image-layer` runs.
`set-layer-expression` and `apply-effect` both accept `compName`; use it.

### Transparent backgrounds

A composition's background colour is a **preview backdrop only** — it never
renders into the alpha channel. A comp with no full-bleed background layer is
already transparent when rendered with alpha; there is no "make it transparent"
switch to flip. The colour does tint the RGB of fully transparent pixels though,
so set it to black (`set-composition-properties`) for anything meant to be
composited, and remember that a render **without** an alpha channel will then
come out on black rather than on that colour.

Also avoid *partially transparent fills* in a comp destined for transparent
output: they premultiply against nothing and go muddy once composited. Flatten
each translucent design token against the theme's canonical canvas and use the
resulting opaque colour instead (see `examples/build-hero-animation.py`).

## Skills over MCP

Alongside its tools, this server publishes **skills** — natural-language
playbooks that tell a connected agent *when* and *how* to use those tools.
They ride on the MCP `resources` primitive under `skill://` URIs
(`io.modelcontextprotocol/skills`), embedded in the component at compile time,
so the playbook can never drift from the implementation it documents.

| URI | Contents |
|---|---|
| `skill://index.json` | The catalog: names, trigger descriptions, file URIs. Read this first. |
| `skill://after-effects-mcp/SKILL.md` | The playbook: panel setup, batching, conventions, error shapes. |
| `skill://after-effects-mcp/references/TOOLS.md` | Per-tool detail, timing budgets, failure modes. |
| `skill://after-effects-mcp/references/HANDOFF.md` | **Rebuilding an Illustrator design as native, editable layers** — the counterpart to the same file in `adobe-illustrator-mcp`. |

Discovery is progressive: a client reads the catalog once, then pulls a
`SKILL.md` only when its description matches the task, and a `references/`
file only when it needs that depth.

```sh
curl -s -X POST -H 'Host: ae-mcp.localhost.cosmonic.sh' \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"resources/list"}' \
  http://127.0.0.1:8200/mcp
```

### Why HANDOFF.md exists

The failure mode it prevents: asked to bring an Illustrator design into After
Effects, an agent exports a flat PNG of the canvas and imports that. The
result cannot be animated — the boxes cannot move independently, the text
cannot be retyped, colours cannot change. The skill makes the rule explicit
(rebuild as shape and text layers; reserve raster imports for genuinely raster
assets) and carries the two conversions that rebuilds get wrong: top-left
box → centre position, and hex → `[r, g, b]` 0..1 floats.

## HTTP surface

| Endpoint | Who calls it | What |
|---|---|---|
| `GET /`, `GET /health` | anyone | **Default route**: JSON discovery document — server identity, MCP spec versions, endpoints, tool names, skills, and live panel status. No protocol handshake needed. |
| `POST /` or `POST /mcp` | MCP clients | Streamable HTTP transport (stateless, 2026-07-28; SSE when the client accepts it) |
| `GET /bridge/command` | AE panel | Pending command or `{"command": null}` |
| `POST /bridge/result?id=N` | AE panel | Result of command N |
| `GET /bridge/panel.jsx` | anyone | The panel source (handy for manual installs) |
| `GET /healthz` | anyone | Plain-text health check (`ok`) |

```sh
curl -s -H 'Host: ae-mcp.localhost.cosmonic.sh' http://127.0.0.1:8200/
```

## Project layout

```
src/lib.rs         HTTP routing + bridge endpoints (wasi:http@0.3.0, p3)
src/server.rs      MCP server: identity, capabilities, resources/* handlers
src/live.rs        the tools that drive a live After Effects instance
src/discovery.rs   the default route: JSON discovery document on GET / and /health
src/skills.rs      Skills over MCP: skill:// resources, embedded at compile time
src/state.rs       command queue & results in wasi:keyvalue
src/bridge.rs      tokio <-> component-model async bridge
src/telemetry.rs   structured JSON logs on stderr; optional wasi:otel spans
skills/server/     the SKILL.md playbook + references/ served over MCP
bridge/mcp-bridge-auto.jsx  ScriptUI panel (ExtendScript; polls over HTTP)
install-bridge.sh  copies the panel into After Effects
deploy/workload.yaml        the Workload manifest (local iteration needs only
                            .wash/config.yaml + cosmonic_dev/promote)
scripts/e2e.sh              full protocol + bridge suite under wasmtime
testing/kv-stub/            file-backed wasi:keyvalue provider, tests only
```

The Cargo package is `after-effects-mcp` — the identity `initialize` reports
and the authority in every `skill://` URI, so the two cannot drift. The
Workload is still named `ae-mcp` and still served at
`ae-mcp.localhost.cosmonic.sh`, so existing client config keeps working.

Notes:
- **ExtendScript sockets strip carriage returns.** `Socket.read()` returns HTTP
  responses LF-only, so the canonical `\r\n\r\n` header separator is never
  present. `extractBody()` in the panel tries every blank-line spelling and
  falls back to the JSON payload; do not "simplify" it back to a single
  `indexOf("\r\n\r\n")`.
- `GET /bridge/command` only hands out commands to panels that pass `?v=2`.
  An older panel that cannot parse responses would otherwise consume a command
  and silently drop it, because the server marks it dispatched on read.
- Each panel instance sends a `client=<id>` (its load timestamp) and the server
  serves only the highest id it has seen. Reloading the panel supersedes the
  previous instance instead of the two racing for commands — the old one's
  `app.scheduleTask` keeps polling until After Effects restarts.
- A panel can be launched without reinstalling (useful while iterating):
  `osascript -e 'tell application "Adobe After Effects 2026" to DoScript (read POSIX file "<repo>/bridge/mcp-bridge-auto.jsx" as «class utf8»)'`
- The command queue lives in the host's in-memory keyvalue store; it is
  transient by design and survives across the component's pooled instances.
- `poolSize: 4` matters: a tool call blocks while waiting for the panel, and
  the panel's poll must be served concurrently.
- The panel targets `127.0.0.1:8200` with a `Host: ae-mcp.localhost.cosmonic.sh`
  header (see the constants at the bottom of `bridge/mcp-bridge-auto.jsx`), so
  it works even offline; only the MCP client URL relies on DNS.
- `MCP_ALLOWED_HOSTS` is the transport's DNS-rebinding guard and must list the
  ingress host. It does **not** cover `/bridge/*`, `/healthz`, or the default
  route, so the panel and health probes work regardless of what they send.

## Configuration

| Environment variable | Default | Description |
|---|---|---|
| `MCP_ALLOWED_HOSTS` | localhost only | DNS-rebinding guard; must list the ingress host (`ae-mcp.localhost.cosmonic.sh`) |
| `MCP_BRIDGE_KEY_PREFIX` | `ae` | Keyvalue namespace (two bridges must not share one) |
| `MCP_BRIDGE_BUCKET` | `in_memory` | Keyvalue bucket identifier |
| `RUST_LOG` | `info` | Log level |

## Build and test

```sh
cargo build --target wasm32-wasip2 --release
cargo clippy --release --target wasm32-wasip2
scripts/e2e.sh                 # full protocol + bridge suite under wasmtime
                               # (wac composes testing/kv-stub; curl plays
                               #  the part of the After Effects panel)
```

Deployment is Cosmonic Desktop only. Two routes:

- **Local iteration** — `cosmonic_dev` then `cosmonic_promote`. Both read
  `.wash/config.yaml`, which carries the labels, environment and outbound
  allow-list; promote returns a digest-pinned Workload draft to apply.
- **Published image** — edit `deploy/workload.yaml` to point at your registry
  and apply it with `cosmonic_apply_workload`.

The Workload needs both hostInterfaces — `wasi:http` (`handler`, p3) and
`wasi:keyvalue` (`store`) — or the component will not instantiate. It also
wants `poolSize` above 1: a tool call blocks waiting on the panel, and the
panel's poll has to be served while it waits. That is a component-level field,
so it lives in `deploy/workload.yaml`, not `.wash/config.yaml`.
