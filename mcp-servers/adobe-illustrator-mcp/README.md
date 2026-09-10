# illustrator-mcp

An MCP server for **Adobe Illustrator**, running as a sandboxed WebAssembly
component on [Cosmonic Desktop](https://cosmonic.com/docs/desktop). ~50 tools
covering documents, artboards, layers, shapes, text, selection transforms,
appearance, arrange, assets, history, batching, and a gated raw-ExtendScript
escape hatch.

## Architecture

A sandboxed component cannot open Illustrator, run AppleScript, or reach an
app-local socket — so the call is inverted: a bridge running *inside*
Illustrator polls the component for work.

```
MCP client ──tools/call──▶ illustrator-mcp (Wasm) ──queue──▶ wasi:keyvalue
                                   ▲                             │
                                   └──result── bridge ◀───poll───┘
                                        (runs inside Illustrator)
```

Two bridge vehicles ship, speaking one protocol and one command library
(`bridge/commands.jsx`):

| Vehicle | What it is | Trade-off |
|---|---|---|
| **Shuttle** (`/bridge/shuttle.sh`) | A terminal script that claims commands over HTTP and relays them into the running Illustrator via AppleScript | Zero install, no restart; runs only while you leave it running (Ctrl-C stops it) |
| **CEP panel** (`bridge/cep/`) | Window > Extensions > Illustrator MCP Bridge; polls every ~2 s on a Chromium timer | Needs `./install-bridge.sh` + one Illustrator restart; then always-on |
| **Pump script** (`/bridge/pump.jsx`) | File > Scripts > Other Script…; drains queued commands for ~2 min, then exits | Illustrator **2022 and older only** — 2023+ removed ExtendScript's `Socket` class |

## Quick start

```sh
# 1. Build + push + deploy on Cosmonic Desktop (or use the MCP tools:
#    cosmonic_dev -> cosmonic_promote -> cosmonic_apply_workload; the
#    promote draft is built from .wash/config.yaml)
wash build

# 2. Verify the server — the default route reports identity, tools, skills,
#    and whether an Illustrator bridge is actually connected:
curl -s -H 'Host: illustrator-mcp.localhost' http://127.0.0.1:8200/

# 3. Start a bridge (macOS):
./install-bridge.sh --shuttle  # zero-install shuttle (run it in a terminal)
./install-bridge.sh            # or: CEP panel (restart Illustrator after)

# 4. Register with your MCP client:
claude mcp add --transport http illustrator http://illustrator-mcp.localhost:8200/
```

## Conventions

- **Coordinates**: points from the **active artboard's top-left**, y
  increasing **downward** (screen convention; the command library converts to
  Illustrator's y-up space). 1 px at 72 ppi == 1 pt.
- **Colors**: CSS hex strings (`"#ff8800"`) or `"none"`.
- **Fonts**: PostScript names (`Helvetica-Bold`, `ArialMT`) — `list_fonts
  contains=...` finds them.
- **Batching**: every live call costs a ~2 s poll cycle; `run_batch` sends a
  whole scene in one round trip.

## Tools

| Area | Tools |
|---|---|
| Helpers (no bridge needed) | `hex_to_rgb`, `rgb_to_hex`, `convert_units` |
| Bridge plumbing | `bridge_status`, `get_results`, `get_help`, `run_script` |
| Read | `get_document_info`, `list_documents`, `list_page_items`, `list_text_frames`, `list_artboards`, `list_layers`, `list_swatches`, `list_fonts`, `get_selection` |
| Documents | `new_document`, `open_document`, `save_document`, `close_document`, `export_document` (svg/png/jpg/pdf), `place_image` |
| Artboards | `list_artboards`, `add_artboard`, `set_active_artboard` |
| Layers | `add_layer`, `set_layer`, `delete_layer` |
| Draw | `draw_rectangle`, `draw_ellipse`, `draw_line`, `draw_polygon`, `draw_star`, `add_text`, `set_text_frame` |
| Selection | `select_all`, `deselect_all`, `select_by_name`, `move_selection`, `scale_selection`, `rotate_selection`, `duplicate_selection`, `delete_selection` |
| Appearance | `set_fill`, `set_stroke`, `set_opacity` |
| Arrange | `group_selection`, `ungroup_selection`, `bring_to_front`, `send_to_back` |
| History | `undo`, `redo` |
| Batch | `run_batch` |
| Escape hatch | `run_jsx` (arbitrary ExtendScript; requires `MCP_ALLOW_RAW_JSX=true`) |

## Skills over MCP

Alongside its tools, this server publishes **skills** — natural-language
playbooks that tell a connected agent *when* and *how* to use those tools.
They ride on the MCP `resources` primitive under `skill://` URIs
(`io.modelcontextprotocol/skills`), embedded in the component at compile time,
so the playbook can never drift from the implementation it documents.

| URI | Contents |
|---|---|
| `skill://index.json` | The catalog: names, trigger descriptions, file URIs. Read this first. |
| `skill://illustrator-mcp/SKILL.md` | The playbook: bridge setup, batching, conventions, error shapes. |
| `skill://illustrator-mcp/references/TOOLS.md` | Per-tool detail, timing budgets, failure modes. |
| `skill://illustrator-mcp/references/HANDOFF.md` | **Moving artwork to After Effects as editable layers** — the counterpart to the same file in `adobe-after-effects-mcp`. |

Discovery is progressive: a client reads the catalog once, then pulls a
`SKILL.md` only when its description matches the task, and a `references/`
file only when it needs that depth.

```sh
curl -s -X POST -H 'Host: illustrator-mcp.localhost' \
  -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"resources/list"}' \
  http://127.0.0.1:8200/mcp
```

### Why HANDOFF.md exists

The failure mode it prevents: asked to move an Illustrator design into After
Effects, an agent exports a flat PNG of the canvas and hands that over. The
result cannot be animated — the boxes cannot move independently, the text
cannot be retyped, colours cannot change. The skill makes the rule explicit
(read the structure with `list_layers` / `list_page_items` /
`list_text_frames` / `list_artboards` and rebuild it natively) and carries the
two conversions that rebuilds get wrong: top-left box → centre position, and
hex → `[r, g, b]` 0..1 floats.

## HTTP surface

| Endpoint | Who calls it | What |
|---|---|---|
| `GET /`, `GET /health` | anyone | **Default route**: JSON discovery document — server identity, MCP spec version, endpoints, tool names, skills, and live bridge status. No protocol handshake needed. |
| `POST /` or `POST /mcp` | MCP clients | Streamable HTTP transport (stateless, 2026-07-28) |
| `GET /bridge/command` | the bridge | Pending command or `{"command": null}` |
| `POST /bridge/result?id=N` | the bridge | Result of command N |
| `GET /bridge/shuttle.sh` | anyone | The zero-install shuttle script |
| `GET /bridge/pump.jsx` | anyone | The pump script (Illustrator 2022 and older) |
| `GET /bridge/commands.jsx` | anyone | The shared ExtendScript command library |
| `GET /healthz` | anyone | Plain-text health check (`ok`) |

```sh
curl -s -H 'Host: illustrator-mcp.localhost' http://127.0.0.1:8200/
```

## Configuration

| Environment variable | Default | Description |
|---|---|---|
| `MCP_ALLOWED_HOSTS` | localhost only | DNS-rebinding guard; must list the ingress host (`illustrator-mcp.localhost`) |
| `MCP_BRIDGE_KEY_PREFIX` | `illustrator-mcp` | Keyvalue namespace (two bridges must not share one) |
| `MCP_BRIDGE_BUCKET` | `in_memory` | Keyvalue bucket identifier |
| `MCP_ALLOW_RAW_JSX` | off | Set `true` to enable the `run_jsx` escape hatch |
| `RUST_LOG` | `info` | Log level |

## Project layout

```
src/lib.rs         HTTP routing + bridge endpoints (wasi:http@0.3.0, p3)
src/server.rs      MCP server: pure-compute tools + resources/* handlers
src/live.rs        the tools that drive a live Illustrator instance
src/discovery.rs   the default route: JSON discovery document on GET / and /health
src/skills.rs      Skills over MCP: skill:// resources, embedded at compile time
src/state.rs       command queue & results in wasi:keyvalue
src/bridge.rs      tokio <-> component-model async bridge
skills/server/     the SKILL.md playbook + references/ served over MCP
bridge/            the in-Illustrator vehicles (CEP panel, pump, shuttle)
```

## Build and test

```sh
cargo build --release          # component at target/wasm32-wasip2/release/
cargo clippy --release --target wasm32-wasip2
scripts/e2e.sh                 # full protocol + bridge suite under wasmtime
                               # (wac composes testing/kv-stub; curl plays
                               #  the part of the Illustrator bridge)
```

Deployment is Cosmonic Desktop only. Two routes:

- **Local iteration** — `cosmonic_dev` then `cosmonic_promote`. Both read
  `.wash/config.yaml`, which carries the labels, environment and outbound
  allow-list; promote returns a digest-pinned Workload draft to apply.
- **Published image** — edit `deploy/workload.yaml` to point at your registry
  and apply it with `cosmonic_apply_workload`.

The Workload needs both hostInterfaces — `wasi:http` (`handler`, p3) and
`wasi:keyvalue` (`store`) — or the component will not instantiate. It also
wants `poolSize` above 1: a tool call blocks waiting on the bridge, and the
bridge's poll has to be served while it waits. That is a component-level
field, so it lives in `deploy/workload.yaml`, not `.wash/config.yaml`.
