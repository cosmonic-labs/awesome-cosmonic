# godot-mcp (Godot) — deferred, with a buildable design

**Status:** deferred (2026-09-02). **Reason:** every popular Godot MCP server
either spawns the Godot binary or talks to an editor addon over WebSocket /
raw TCP; a Cosmonic Desktop workload has no subprocesses, no raw TCP and no
WebSocket — only outbound `wasi:http` on an allow-list, plus mounted volumes.
Two shapes survive that constraint and are specified below so a builder can
start without re-researching:

- **Shape B — "Godot project" filesystem server** (buildable today, no Godot
  needed at runtime): mount the project directory via `spec.volumes`, parse and
  edit `project.godot` / `.tscn` / `.tres` / `.gd` with a lossless Rust parser,
  plus two keyless live tools against the Godot Asset Library API.
- **Shape A — proxy to an in-editor MCP addon over plain HTTP on loopback**
  (works today against one addon, needs a one-line upstream change for the
  other; needs the loopback door).

Name: `godot-mcp` (crate `godot_mcp`, wasm `godot_mcp.wasm`), ingress
`http://godot-mcp.localhost:8200/`, skill `skill://godot-mcp/SKILL.md`.

## What exists upstream (checked 2026-09-02)

| Project | License | Transport to Godot | MCP transport | Auth | Notes |
|---|---|---|---|---|---|
| [Coding-Solo/godot-mcp](https://github.com/Coding-Solo/godot-mcp) | MIT | spawns `godot --headless --script godot_operations.gd` (`GODOT_PATH`) | stdio (Node ≥18) | none | 18 tools: launch_editor, run_project, get_debug_output, stop_project, get_godot_version, list_projects, get_project_info, create_scene, add_node, edit_node, remove_node, load_sprite, export_mesh_library, save_scene, get_uid, update_project_uids. Godot 3.5+/4.x, UID tools 4.4+. Not reachable: subprocess. |
| [beckettlab/beckett-godot-mcp](https://github.com/beckettlab/beckett-godot-mcp) v1.15.0 (2026-09-01, AssetLib #5296) | MIT (Lite); Full is commercial | none — the addon **is** the server: hand-rolled HTTP/1.1 on `TCPServer`, `127.0.0.1:8770` (walks up to +10 if busy; live port in `res://.beckett/port`) | streamable HTTP (`2025-11-25`, also 2025-06-18/03-26), path `/mcp` or `/mcp/<token>` | optional bearer (`BECKETT_TOKEN` / `res://.beckett/token`, disable with `BECKETT_AUTH=0`); Origin gate; **Host gate: 403 unless Host is `127.0.0.1`/`localhost`/`::1`**; `Mcp-Session-Id` required after initialize (404 "unknown session") | 55 Lite tools (scene authoring, scripts, signals, files, project settings, screenshot, play/stop, runtime observation), 6 resources, 6 prompts. Godot 4.2+. |
| [regiellis/godot-mcp-go](https://github.com/regiellis/godot-mcp-go) v0.11.0 (2026-09-01, AssetLib #5367 "Godot MCP/CLI") | MIT | addon exposes **plain streamable HTTP** `POST /mcp` on `127.0.0.1:9100` (auto 9100-9115; setting `godot_mcp/network/http_port`), plus WebSocket 9080-9095 for its Go CLI | streamable HTTP (`2025-06-18`, `2025-03-26`) | none; Origin gate only (**no Host check** → a proxied request with no Origin passes) | 332 commands / 50 groups; typed tools (~52k tokens of schema) or a single generic `godot_run` tool (`http_typed=false`). Godot 4.3–4.8. |
| [hybridindie/godot-mcp](https://github.com/hybridindie/godot-mcp) 2026.09.02 (AssetLib #5434) | MIT | addon dials **out** as a WebSocket client to a Python bridge (`ws://127.0.0.1:9080`) | stdio, or HTTP service mode on 9090 (bearer `GODOT_MCP_AUTH_TOKEN` for non-loopback) | token | 180 tools / 29 toolsets, read_only/mutating(dry_run)/destructive(confirm) classes. The HTTP mode is a Python sidecar, not the editor — still a host process the user must run. |
| [mkdevkit/godot-mcp](https://github.com/mkdevkit/godot-mcp) | MIT | WebSocket 6505 | stdio (Node) | none | 173 tools / 26 categories. Godot 4.4+. |
| [KeeVeeG/godot-mcp](https://github.com/KeeVeeG/godot-mcp) | MIT | WebSocket 6505-6514 | stdio (Node) | none | 300+ tools / 40 modules. |
| [tomyud1/godot-mcp](https://github.com/tomyud1/godot-mcp) v0.6.0 | MIT | WebSocket 6505 (+ HTTP visualizer 6510) | stdio | none | 42 tools. |
| [hi-godot/godot-ai](https://github.com/hi-godot/godot-ai) v4 (2.1k stars) | MIT | addon WebSocket 9500 ← Python server HTTP 8000 | stdio | rotating capabilities on both hops | 46 tools / 120 ops; snap. Godot 4.7+. |
| [tugcantopaloglu/godot-mcp](https://github.com/tugcantopaloglu/godot-mcp) | MIT | headless CLI + raw TCP 9090 autoload | stdio | none | 157 tools. Godot 4.4+. |
| [IvanMurzak/Godot-MCP](https://github.com/IvanMurzak/Godot-MCP) | Apache-2.0 | SignalR from a C#/.NET addon to a shared GameDev-MCP-Server (cloud `ai-game.dev` or self-hosted) | streamableHttp or stdio | cloud account / custom | needs the .NET (mono) Godot build. |
| Godot MCP Pro (youichi-uda) | proprietary ($15) | — | — | — | 162 tools; ignore. |
| **Vendor (Godot Foundation)** | — | — | — | — | **No official MCP server exists** as of 2026-09; the engine's only network doors are the debug server (`--debug-server tcp://`), DAP (`--dap-port`) and LSP (`--lsp-port`) — all raw TCP, unusable from wasi:http. |

Rust building blocks (all MIT, pure Rust, wasm-friendly):

| Crate | Version | Deps | Use |
|---|---|---|---|
| [`tscn`](https://crates.io/crates/tscn) (hyprtuna/gdmerge) | 0.3.6 (2026-08-29) | serde, thiserror | Lossless `.tscn`/`.tres` parse → `Document` (sections, fields, `Value` incl. `ExtResource("id")`/`SubResource`/`NodePath`), `Scene` semantic model with `node_path`, `check()` (unresolved NodePaths etc.), `diff()`, `to_source()` byte-exact (814 public Godot 4 files round-trip). Grammar mirrors Godot's `VariantParser`. This is the core of Shape B. |
| [`godot-properties-parser`](https://crates.io/crates/godot-properties-parser) | 0.4.0 (2025-11-25) | nom 8 | `project.godot` (and tscn) sectioned key=value parser; alternative for `project.godot` if `tscn` does not handle `config_version=5` files (they share the ConfigFile grammar: `[section]` + `key=Variant`). |
| [`gdstyle`](https://crates.io/crates/gdstyle) | 0.2.5 | — | GDScript linter/formatter; optional `godot_lint_script` tool without running Godot (check it builds on wasm32-wasip2 first). |

File-format facts the builder needs (from `engine_details/file_formats/tscn.rst`):

- Header `[gd_scene load_steps=N format=3 uid="uid://…"]` (`format=3` = Godot 4; `format=2` = Godot 3 → refuse). Resources: `[gd_resource type="…" format=3 uid=…]`.
- `[ext_resource type="Texture2D" uid="uid://…" path="res://…" id="2_eorut"]` — Godot 4 ids are strings `<n>_<5 random>`; references are `ExtResource("2_eorut")`. Sub-resources: `[sub_resource type="CapsuleShape3D" id="CapsuleShape3D_fdxgg"]` and must precede referrers.
- Nodes: `[node name="X" type="Camera3D" parent="Player/Head"]`; root omits `parent`; direct children of root use `parent="."`; `instance=ExtResource("…")` for instanced scenes; `groups=[…]`, `index=`, `owner=`, `unique_id=` (4.6+ optional).
- Values: `Vector2(1, 2)`, `Color(r,g,b,a)`, `Transform3D(...)`, `NodePath("..")`, `PackedStringArray("a")`, `{ "k": v }`, `[a, b]`; `;` comments are dropped by the editor on save.
- `[connection signal="pressed" from="Button" to="." method="_on_pressed" flags=0 binds=[…]]`; `[editable path="Child"]`.
- `project.godot`: INI-like, starts with `config_version=5` (Godot 4), `[application] config/name="…"`, `run/main_scene="res://…"`, `config/features=PackedStringArray("4.7")` (the minimum engine version), `[autoload] Name="*res://…"` (`*` = singleton node), `[editor_plugins] enabled=PackedStringArray("res://addons/x/plugin.cfg")`, `[input] action={"deadzone": 0.5, "events": [Object(InputEventKey, …)]}`, `[display] window/size/viewport_width=…`. Multi-line strings are literal newlines inside quotes.
- `.godot/` is a regenerable cache (never edit; `uid_cache.bin` is binary). `*.import` are generated sidecars. Since 4.4 every `.gd`/`.gdshader` has a `.uid` sidecar containing `uid://…`; if a new script lacks one Godot creates it on next editor focus, so never invent UIDs — omit `uid=` on new files.

## Shape B — Godot project filesystem server (build this first)

Same mount model as `official-filesystem-mcp`: no network needed except the
optional Asset Library tools. Runs fine with the editor open; Godot rescans on
window focus (edits made on disk appear after the user alt-tabs back; an open
scene shows a "file changed on disk" reload prompt).

### Config

| Env | Kind | Required | Default | Meaning |
|---|---|---|---|---|
| `GODOT_PROJECT_DIR` | named config | no | `/project` | WASI preopen path of the mounted project root; must contain `project.godot`. |
| `GODOT_ALLOW_WRITE` | named config | no | `false` | `true` enables the gated write tools. Pair with `readOnly: false` on the mount. |
| `GODOT_MAX_FILE_BYTES` | named config | no | `524288` | Read/write size clamp (hard max 4 MiB). |
| `GODOT_ASSETLIB_BASE_URL` | named config | no | `https://godotengine.org/asset-library/api` | Override for the e2e fixture. |
| `MCP_ALLOWED_HOSTS` | named config | yes | `godot-mcp.localhost` | Template DNS-rebind guard. |

No secrets. `allowedHosts: ["https://godotengine.org"]` (only for the two
asset tools; leave empty if they are dropped).

Manifest grant:

```yaml
spec:
  volumes:
    - name: project
      hostPath: { path: /home/me/games/my-game }     # must exist; the folder with project.godot
  components:
    - name: godot-mcp
      localResources:
        volumeMounts:
          - { name: project, mountPath: /project, readOnly: false }
        environment:
          config: { GODOT_ALLOW_WRITE: "true", MCP_ALLOWED_HOSTS: godot-mcp.localhost }
        allowedHosts: ["https://godotengine.org"]
```

### Tools (all paths are `res://…` or project-relative; `..`, absolute paths, `.godot/`, `*.import`, `.beckett/` are rejected)

| Tool | Gated | Upstream | Params / clamps |
|---|---|---|---|
| `godot_project_info` | no | read `project.godot` | none → name, `run/main_scene`, `config/features` (engine version), autoloads, enabled plugins, input action names, display size, count of scenes/scripts, `.godot/` present?, git-ignored? |
| `godot_list_files` | no | walk mount | `kind` ∈ scenes\|scripts\|resources\|shaders\|all (default scenes), `dir` (default `res://`), `include_addons` (default false), `limit` 1–500 (default 200); skips `.godot/`, dirs with `.gdignore`, `.import` sidecars |
| `godot_read_file` | no | `std::fs::read` | `path`, `start_line`, `max_lines` 1–2000; refuses binary (`.scn`, `.res`, `.ctex`, images) with the hint to use `godot_scene_tree` only for text scenes; truncates at `GODOT_MAX_FILE_BYTES` |
| `godot_search` | no | regex over `.gd/.tscn/.tres/.gdshader/.cs/project.godot` | `pattern` (regex ≤ 256 chars, size-limited via `regex` crate), `glob`, `max_matches` 1–500 (default 100) — returns path:line:text |
| `godot_scene_tree` | no | `tscn::Document::parse` + `Scene` | `path` (.tscn) → root, nodes (path, type, instance source, script ext_resource path, groups), ext_resources, sub_resource summary (type counts), connections, `check()` issues |
| `godot_node` | no | same | `path`, `node` (scene-relative NodePath, `.` = root) → all properties as text values, resolved `ExtResource` paths |
| `godot_find_references` | no | scan headers + `preload("res://…")`/`load(`/`ExtResource(... path=` | `target` (`res://` path or `uid://…`), `limit` ≤ 500 → referencing files and lines; also resolves `uid://` ↔ path from `.uid` sidecars and scene/resource headers |
| `godot_set_node_property` | write | `Document` edit → `to_source()` → write | `path`, `node`, `property`, `value` (Godot literal text, e.g. `Vector2(10, 20)`, validated by re-parsing), `remove` (bool). Re-parses the result before writing; refuses if `check()` reports new errors |
| `godot_add_node` | write | append `[node]` section after the parent's subtree | `path`, `parent` (must exist), `name` (unique among siblings, no `/`,`:`,`@`,`.`), `type` (class name) or `instance` (`res://…tscn` — adds an `ext_resource` with a fresh `<n>_xxxxx` id and bumps `load_steps`), `properties` (map of literals) |
| `godot_remove_node` | write, destructive | remove the node + descendants + `[connection]`s that reference them | `path`, `node`, `confirm: true` required |
| `godot_connect_signal` | write | append `[connection]` | `path`, `from`, `to`, `signal`, `method`, `flags` (default 0); refuses duplicates |
| `godot_write_file` | write | `std::fs::write` (+ `.uid`-less new scripts) | `path` (`.gd`, `.gdshader`, `.tres`, `.tscn`, `.cs`, `.md`, `.txt`, `.json`, `.cfg`), `content` ≤ clamp, `create_dirs` (default false); `.tscn/.tres` content must round-trip through `tscn` (format=3) or the write is refused; `project.godot` allowed only when `allow_project_godot: true` |
| `godot_asset_search` | no (network) | `GET {ASSETLIB}/asset?filter=&type=&godot_version=&support=&sort=&max_results=&page=` | `query` (→ `filter`), `godot_version` (e.g. `4.4`), `type` any\|addon\|project, `sort` rating\|updated\|name\|cost, `max_results` 1–50 (upstream allows 500; clamp), `page` ≥ 0 → `asset_id,title,author,category,godot_version,cost(=license),version_string,modify_date` |
| `godot_asset_info` | no (network) | `GET {ASSETLIB}/asset/{id}` | `asset_id` (digits) → description, `download_url`, `browse_url`, `godot_version`, `support_level`, `previews[]` |

Rendering: scene trees as an indented text tree plus a compact JSON block;
never dump sub_resource bodies by default (meshes/curves are huge).

### Error catalogue

| Condition | Meaning | Action |
|---|---|---|
| `/project/project.godot` missing | volume not mounted, wrong `hostPath`, or the folder is a parent of the project | Say which path was checked; user fixes `spec.volumes.hostPath` (must be the folder containing `project.godot`). |
| `config_version=4` / scene `format=2` | Godot 3 project | Unsupported; tell the user to open it in Godot 4 to convert. |
| `tscn::ParseError { line, col }` | binary `.scn`, malformed hand-edit, or a construct the parser does not know | Report line/col; suggest saving from the editor to normalise; never overwrite the file. |
| `check()` issues after an edit (dangling `NodePath`, missing `ExtResource` id) | edit would break the scene | Refuse the write; show issues. |
| node not found / duplicate sibling name | wrong NodePath (scene-relative, `.` = root, no leading `/`) | List siblings of the nearest existing ancestor. |
| write requested with `GODOT_ALLOW_WRITE=false` | gated | Tell the user to set the config and `readOnly: false` on the mount. |
| `EROFS`/`EACCES` on write | mount is `readOnly: true` or host permissions | Point at `volumeMounts.readOnly`. |
| path contains `..`, starts with `/`, or targets `.godot/`, `*.import`, `.beckett/` | escapes the sandbox or edits generated caches | Reject with the allowed shapes. |
| file > `GODOT_MAX_FILE_BYTES` | oversized | Return the head plus `truncated: true` and the byte count; suggest `start_line`. |
| Asset Library 404 on `/asset/{id}` | no such asset | — |
| Asset Library 5xx / timeout | godotengine.org outage | Retry later; the rest of the server works offline. |
| `Failed to connect to godotengine.org` | `allowedHosts` lacks `https://godotengine.org` | Add it in both manifests. |

### Skill points (what an agent gets wrong without SKILL.md)

1. Node paths in `.tscn` are relative to the scene root and the root is `.`; `parent="Player/Head"` never includes the root's name, and the root node has no `parent` attribute at all.
2. Never invent `uid="uid://…"` values or `ext_resource` ids; new files get UIDs from Godot on the next editor focus, and `ext_resource` ids must be unique strings (`<n>_<5 chars>`) with `load_steps` = ext + sub resources + 1.
3. `.godot/`, `*.import`, `uid_cache.bin` and `.uid` sidecars are generated — edit the source file and let Godot regenerate; touching them causes reimport storms or lost references.
4. Nothing here runs Godot: to validate scripts run `godot --headless --path <project> --check-only --script res://x.gd` out of band (or use Shape A); a `.tscn` that parses is not proof it loads (unknown class names, wrong property types).
5. `project.godot` stores only non-default settings; a key that is absent is at its default, not unset — read `config/features` for the engine version, not the editor's.
6. `.gd` scripts attached in a scene appear as `script = ExtResource("id")` on the node; find the path via the `ext_resource` table, not by guessing `res://<NodeName>.gd`.
7. Asset Library `cost` is the license string (`MIT`, `GPLv3`…), `godot_version` on search results is a minimum, and `max_results` above 500 is rejected upstream — our clamp is 50.

### Hermetic e2e

- `mcp_harness_start --dir "$E2E_TMP/project::/project" --env GODOT_ALLOW_WRITE=true --env GODOT_ASSETLIB_BASE_URL=http://127.0.0.1:$FIXTURE_PORT`; guard instance started **without** the `--dir` (missing-volume path) and with `GODOT_ALLOW_WRITE` unset (gated-write path).
- Fixture project generated by the script: `project.godot` (config_version=5, autoload, plugin, input action, multi-line description), `main.tscn` (root + 3 nested nodes, one `instance=`, one `script=ExtResource`, a `SubResource` shape, two `[connection]`s, a `;` comment, a unicode node name `Игрок`), `enemy.tscn`, `player.gd` + `player.gd.uid`, `legacy.tscn` with `format=2`, a fake binary `blob.scn`, an `addons/x/plugin.cfg`, a `.godot/` folder with junk and a `.gdignore`d dir.
- Python `ThreadingHTTPServer` fixture: `GET /asset` echoes every query param inside the JSON (`echo: {filter, godot_version, max_results, page, type, sort}`) plus 2 canned results; `GET /asset/123` canned detail; `GET /asset/999` → 404; `GET /asset?filter=boom` → 500. Tests assert the clamp (`max_results=999` → fixture sees 50), URL-encoding of unicode/`&` in `filter`, and the `Accept: application/json` header.
- Tool cases: tree of `main.tscn`, node props, set property + re-read byte-exact except the changed line (diff the file), add node under nested parent then `godot_scene_tree` shows it, remove node cascades connections, duplicate-name rejection, `..` path rejection, `.godot/` rejection, `legacy.tscn` → format error, `blob.scn` → binary error, huge `content` → clamp, regex DoS-shaped pattern (`(a+)+$`) → bounded, references of `res://player.gd` found in both `.tscn` and `.gd`.
- `E2E_LIVE=1`: `godot_asset_search query=mcp godot_version=4.4` against the real API (keyless, verified 200 today). No local Godot install exists on this machine and none of the local services (podman API, Postgres) apply. Optional: download a Godot 4.x linux headless build (arm64 builds exist) to generate a real fixture project and to `--headless --import` the edited scenes as a validity check — not required.

### Effort

Shape B: ~3 days for one engineer — 0.5 d to port the mount/path-guard code from
`official-filesystem-mcp`, 1 d for the `tscn`-backed scene tools (the edit
primitives are the risky part: inserting sections while keeping byte-exact
output for everything else), 0.5 d for project.godot/search/references, 0.5 d
asset tools + fixture + e2e, 0.5 d SKILL.md/README. Add 0.5 d if `tscn` needs
patches (send them upstream; MIT).

## Shape A — proxy to an in-editor MCP addon over loopback HTTP

Two addons already serve MCP over plain HTTP from inside the editor, so the
wasm server is a thin authenticated proxy: our `tools/list` is built from the
addon's list (cached in a static on the warm instance), `tools/call` is
forwarded, and we add the Desktop-side value: allow-listing, a read-only mode,
the skill, and a stable `godot-mcp.localhost` URL.

| Target | Upstream call | Works from wasm? |
|---|---|---|
| regiellis/godot-mcp-go addon | `POST http://host.wasmcloud.internal:9100/mcp` (JSON-RPC; `initialize` with `protocolVersion: "2025-06-18"`, then `tools/list`, `tools/call`; `notifications/initialized` → 202) | **Yes today**: the addon checks only `Origin` (absent from our requests → allowed) and binds 127.0.0.1. No auth: safety rests on the loopback door + `allowedHostLoopbackPorts`. Default port may drift to 9101… if 9100 is busy — pin `godot_mcp/network/http_port` in Project Settings. Prefer `godot_mcp/network/http_typed=false` and expose a single `godot_run` passthrough to avoid a 52k-token schema. |
| beckettlab Beckett Lite | `POST http://host.wasmcloud.internal:8770/mcp` (`Authorization: Bearer <BECKETT_TOKEN>` or `/mcp/<token>`; `Mcp-Session-Id` from `initialize`, `protocolVersion: "2025-11-25"`) | **Not without an upstream change**: `_check_host` returns 403 `forbidden host` unless `Host` is `127.0.0.1`/`localhost`/`::1`, and wasmtime's `wasi-http` lists `Host` in `DEFAULT_FORBIDDEN_HEADERS` (crates/wasi-http/src/lib.rs), so the component cannot rewrite it. Needed: a `BECKETT_ALLOWED_HOSTS` env (add `host.wasmcloud.internal`) — ~10-line PR to `addons/beckett/core/mcp_server.gd` — or a Desktop-side Host rewrite (below). Its bearer token is the auth model we want. |

Config for Shape A (on top of Shape B's):

| Env | Kind | Required | Default |
|---|---|---|---|
| `GODOT_EDITOR_MCP_URL` | named config | no | `http://host.wasmcloud.internal:9100/mcp` |
| `GODOT_EDITOR_PROTOCOL_VERSION` | named config | no | `2025-06-18` (Beckett: `2025-11-25`) |
| `GODOT_EDITOR_TOKEN` | secret ref `godot-mcp-editor-token` | no (Beckett yes) | — ; value = contents of `<project>/.beckett/token` or `BECKETT_TOKEN` |
| `GODOT_EDITOR_ALLOW_WRITE` | named config | no | `false` — only forward tools whose upstream annotation is `readOnlyHint: true` (or a name allow-list from `references/tool-classes.md`) |

Grants: `allowedHosts: ["host.wasmcloud.internal:9100"]`,
`allowedHostLoopbackPorts: ["9100"]`, and Settings → Security → allow host
loopback. Tools: `godot_editor_tools` (cached `tools/list` with the upstream
class), `godot_editor_call {name, arguments}` (forward; 60 s timeout since
`play_scene`/`screenshot` are slow), `godot_editor_status` (`initialize` and
report engine version / addon / negotiated protocol).

Error catalogue for Shape A: connection refused → editor not running, plugin
not enabled, wrong port, or loopback door closed (list the three-step grant);
`403 forbidden host` → Beckett's Host gate (needs the PR / Desktop rewrite);
`403 forbidden origin` → an `Origin` header leaked into the request (never set
one); `401 unauthorized` → token missing/rotated (re-read `.beckett/token`);
`404 unknown session` → Beckett restarted, drop the cached `Mcp-Session-Id`
and re-`initialize`; `-32601` → tool disabled in the addon dock/allowlist;
`202 Accepted` on a notification is success.

Effort: ~1.5 days on top of Shape B (proxy + session cache in a static +
fixture faking `/mcp` with the Origin/Host/token/session rules above); plus the
Beckett PR if that target is wanted.

## User setup (Shape B)

1. Have a Godot 4 project on disk (folder containing `project.godot`).
2. Edit `deploy/workload.yaml`: `spec.volumes[0].hostPath.path` = that folder; set `GODOT_ALLOW_WRITE: "true"` and `readOnly: false` to allow edits.
3. Apply the workload; `curl http://godot-mcp.localhost:8200/` shows status; `claude mcp add --transport http godot-mcp http://godot-mcp.localhost:8200/`.
4. Keep the editor open if you like — changes show up on focus; commit `.godot/`-free.

## Risks

- `tscn` crate is young (0.3.x, one maintainer, 2026-08); byte-exact replay of *unchanged* sections is the load-bearing property — pin the version and keep a regression corpus of real scenes in the e2e.
- Hand-edited scenes can be structurally valid but semantically wrong (unknown class, wrong property type); the server cannot load them. Set expectations in SKILL.md and recommend a Shape-A or CLI validation step.
- Concurrent edits: the editor may overwrite a disk edit if the user saves an open scene afterwards; document "reload from disk".
- Large projects: walking tens of thousands of files per call; cache the file index in a static keyed by `project.godot` mtime, cap walks at 20k entries.
- Shape A has no auth against godot-mcp-go; the security boundary is the loopback door. Prefer Beckett once its Host gate accepts `host.wasmcloud.internal`.
- Asset Library API has no documented rate limit; keep `max_results ≤ 50` and no automatic pagination.

## What Desktop could add to make this easy

- A loopback egress option that rewrites `Host` to `127.0.0.1:<port>` (or lets the manifest set it) — every DNS-rebind-hardened local MCP server (Beckett, official SDK ≥ 0.25 behaviour) will otherwise 403 `host.wasmcloud.internal`.
- A UI "pick a project folder" that creates the `spec.volumes` entry and validates it contains `project.godot`.
- A generic "local MCP bridge" host interface (proxy streamable-HTTP endpoints on loopback with token injection) so Shape A needs no code per app.
- A managed-process capability (run `godot --headless …` on behalf of a workload with an allow-listed binary) would make the Coding-Solo tool set possible; today it is out of scope by design.
