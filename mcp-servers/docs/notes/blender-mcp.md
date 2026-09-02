# blender-mcp — deferred (research notes, 2026-09-02)

**Status: DEFERRED.** Not buildable as a pure Wasm component on Cosmonic
Desktop 0.5.27 today. Reason: every maintained Blender bridge talks to the
running Blender process over a **raw TCP JSON socket** (no HTTP), and a
component has only `wasi:http` outbound. The one HTTP-native option
(dcc-mcp-blender) is itself a full MCP server, so a component in front of it
would only be an MCP-to-MCP proxy. Everything below is what a future build
needs so nobody has to re-research it.

Intended name/URL when built: `blender-mcp`, `http://blender-mcp.localhost:8200/`.

## 1. What exists upstream (checked 2026-09-02)

| Project | License | Transport to Blender | Auth | Notes |
|---|---|---|---|---|
| [ahujasid/blender-mcp](https://github.com/ahujasid/blender-mcp) v1.9.1 (PyPI 2026-09-02, 26.7k stars) | MIT | Add-on opens TCP `localhost:9876`; **raw JSON with no delimiter** (`{"type":..,"params":{..}}` → `{"status":"success"|"error","result"|"message"}`), receiver accumulates `recv(8192)` chunks until `json.loads` succeeds; 180 s socket timeout; commands run on Blender's main thread from a 0.05 s `bpy.app.timers` queue. Python MCP server is stdio (`BLENDER_HOST`/`BLENDER_PORT` env). Add-on `bl_info` version 1.6, `ADDON_PROTOCOL_VERSION = 5`. | none on the socket; asset providers use keys stored in add-on prefs or env (`BLENDERMCP_SKETCHFAB_API_KEY`, `BLENDERMCP_POLYPIZZA_API_KEY`, `BLENDERMCP_HYPER3D_API_KEY`, `BLENDERMCP_HUNYUAN3D_*`) | De-facto standard; the tool surface to mirror. `execute_code` is arbitrary Python (`exec` with `bpy` in namespace; optional `BLENDER_MCP_SAFE_MODE=1`). |
| [Blender Lab `lab/blender_mcp`](https://projects.blender.org/lab/blender_mcp) v1.0.0 (2026-04-27; docs at blender.org/lab/mcp-server) — the **official** Blender Foundation one | **GPL-3.0-or-later** (add-on and server) | Add-on extension (`blender_version_min = "5.1.0"`) listens TCP `localhost:9876`, **null-byte-delimited JSON**; single request type `{"type":"execute","code":..,"strict_json":bool}`; 10 s client timeout; deferred (background-job) responses in interactive mode. Server is stdio, shipped as `.mcpb` (`uv run blender-mcp`). | none | Every tool is Python "toolcode" the server sends to the add-on. 27 tools: `execute_blender_code`, `get_objects_summary`, `get_object_detail_summary`, `get_blendfile_summary_*` (+`_for_cli` variants that open a file in background Blender), `get_screenshot_of_{area,window}_as_image`, `get_screenshot_of_window_as_json`, `jump_to_*`, `render_thumbnail_to_path`, `render_viewport_to_path`, `get_python_api_docs`, `search_api_docs`, `search_manual_docs`. GPL: **do not borrow code**; surface ideas only. |
| [dcc-mcp/dcc-mcp-blender](https://github.com/dcc-mcp/dcc-mcp-blender) v0.2.3 (2026-08-25) | MIT (source/PyPI); the Blender-Extensions ZIP is GPL-3.0-or-later | **Streamable HTTP MCP server embedded in Blender**: each Blender instance binds an OS-assigned port and registers with a local gateway at `http://127.0.0.1:9765/mcp` (`dcc-mcp-core`; host `127.0.0.1`, `endpoint_path` `/mcp`, `DCC_MCP_GATEWAY_PORT` / `DCC_MCP_BLENDER_PORT` env). MCP 2025-03-26 with sessions. | none on `/mcp` (`DCC_MCP_API_KEY` exists in core constants; undocumented) | 200+ tools (`list_objects`, `get_scene_info`, `get_object_info`, `execute_python`, `capture_viewport`, `render_scene`, …). Blender 3.6–4.4 badges, extension install for 4.2+. Kill switches `DCC_MCP_BLENDER_DISABLE_EXECUTE_PYTHON`, `…_DISABLE_ARBITRARY_SCRIPT`. Reachable from a component via loopback grant on 9765, but you would be proxying MCP over MCP. |
| [PatrykIti/blender-ai-mcp](https://github.com/PatrykIti/blender-ai-mcp) (2026-06-27) | Apache-2.0 | JSON-RPC over TCP to add-on, port 8765; server has `MCP_TRANSPORT_MODE=streamable` (stateful HTTP) | none | Blender 4.0+/5.0. Atomic/macro/workflow tool layers. Same TCP problem. |
| [djeada/blender-mcp-server](https://github.com/djeada/blender-mcp-server) (2026-06-21) | MIT | TCP `localhost:9876`, stdio server | none | 27 tools / 7 namespaces. |
| [emeryporter/blender-mcp](https://github.com/emeryporter/blender-mcp) (2026-01) | MIT (README) / no LICENSE file | claims Streamable HTTP served by the add-on on `localhost:9876` | none | 1 star, unmaintained; 86 tools. Not a basis. |
| [Oli97430/blender-mcp-addon](https://github.com/Oli97430/blender-mcp-addon) v1.3.0 (2026-05-04) | GPL-3.0-or-later | TCP 9876, null-byte-delimited JSON, single `execute` command | none | Minimal; GPL. |
| seehiong `blender-mcp-bridge` (n8n) | ? | Streamable HTTP on `0.0.0.0:8008/mcp` (own add-on, 70+ tools) | none | Another MCP-in-Blender; same proxy objection. |

Asset providers the ahujasid add-on wraps (these are plain HTTPS APIs a component
**can** call directly):

| API | Base | Auth | Verified behaviour |
|---|---|---|---|
| Poly Haven public API ([Public-API](https://github.com/Poly-Haven/Public-API), assets CC0, API code AGPL, ToS requires a unique User-Agent/Referer per app and a visible "Poly Haven" credit in the UI) | `https://api.polyhaven.com` | none | `GET /types` → `["hdris","textures","models"]`; `GET /assets?type=hdris|textures|models|all&categories=a,b` returns the **whole catalog as one object keyed by slug** (~700–1000 entries, `type` 0=hdri 1=texture 2=model, `categories`, `tags`, `authors`, `max_resolution`, `polycount`, `dimensions`); `GET /search?q=&t=&limit=&min=` (vector+keyword; `{"results":[{"slug","score"}],"total"}`); `GET /categories/{type}` → `{name:count}`; `GET /info/{id}`; `GET /files/{id}` → nested `{kind:{resolution:{format:{url,size,md5,include:{...}}}}}` (hdri: `hdri.1k.hdr|exr`; models: `blend|gltf|fbx|usd` with `include` texture maps) on `dl.polyhaven.org`. Errors are plain text: 404 `No asset with id X`, 400 `Unsupported asset type: X. Must be: hdris/textures/models/all`. `cache-control: max-age=43200`, no rate-limit headers seen. |
| Sketchfab Data API v3 | `https://api.sketchfab.com` | `Authorization: Token <API_TOKEN>` (from account settings → Password & API) or OAuth Bearer. **Search and model metadata are keyless**; only `/download` needs a token. | `GET /v3/search?type=models&q=&downloadable=true&categories=&count=` → `{cursors:{next,previous},next,previous,results:[{uid,name,isDownloadable,license:{label},user:{username},faceCount,vertexCount,animationCount,archives:{glb,gltf,usdz,source:{size,...}},thumbnails:{images:[...]}}]}`; `count` is silently clamped to 24 upstream; cursor pagination. `GET /v3/models/{uid}` full metadata; `GET /v3/models/{uid}/download` → temporary `gltf|glb|usdz|source` URLs with `expires: 300` s. Errors: 401 `{"detail":"Authentication credentials were not provided."}` / `{"detail":"Invalid API token"}`; 404 `{"detail":"Not found."}`. Categories: `GET /v3/categories` (slugs like `furniture-home`). Rate limits are not published. |
| Poly Pizza API v1.1 (docs at poly.pizza/docs/api/v1.1, behind Cloudflare JS — read in a browser) | `https://api.poly.pizza/v1.1` | `x-auth-token: <key>` on **every** call (free key from poly.pizza/settings/api) | Without a key: 401 `{"error":"You need an API key to do that dingus"}`; bad key: 401 `{"error":"API key not valid 😭"}`. Endpoints used by wrappers: `GET /search/{query}` (ahujasid: capitalized filters `?Category=0&License=1&Animated=1&Limit=`) / `GET /search?category=&license=&animated=&limit=&page=` (MatthewHallCom/Poly-Pizza-MCP), `GET /model/{id}`. Model: `{id,title,attribution,thumbnail,download (GLB on static.poly.pizza),triCount,creator{name,url},category(0-11),license,animated}`; search → `{total,results[]}`. Verify the exact search path/param casing in the browser before building. |

Hyper3D Rodin and Hunyuan3D generation (paid, job polling) are deliberately out of the first build.

## 2. Why the component cannot do it today

- ahujasid / Blender Lab / djeada / blender-ai-mcp: raw TCP. The component has no `wasi:sockets`; outbound is `wasi:http` only, and the loopback grant (`host.wasmcloud.internal:<port>`) only helps when the thing on that port speaks HTTP.
- dcc-mcp-blender / blender-mcp-bridge: HTTP, but the endpoint is a *stateful* MCP 2025-03-26 server. Our stateless 2026-07-28 component would have to run an MCP client with session handling against it, per request, from a warm-instance cache. Possible, low value, and the 200-tool surface is not ours to curate.
- The add-on runs code on Blender's main thread; a tool call can legitimately take tens of seconds (heavy `execute_code`, asset download+import). That is fine for the outbound deadline but must be bounded (see timeouts).

## 3. Design that would work on Cosmonic Desktop

### 3a. Recommended: HTTP shim sidecar next to Blender (no add-on changes)

A ~150-line Python script `blender-http-bridge.py` the user runs on the host
(same machine as Blender, python3 stdlib only). It listens on
`127.0.0.1:9877` and forwards to the ahujasid add-on socket `127.0.0.1:9876`
(one TCP connection per request; send JSON, accumulate until `json.loads`
succeeds, 180 s timeout, exactly what `BlenderConnection.receive_full_response`
does).

Bridge contract (what the component dials):

| Route | Body | Returns |
|---|---|---|
| `GET /health` | – | `{"ok":true,"blender":{"pong":true},"addon":<get_addon_info result>}` or 503 `{"ok":false,"error":"connection refused: is Blender running with the MCP add-on server started?"}` |
| `POST /command` | `{"type":"<addon command>","params":{...}}` | the add-on's response verbatim (`{"status":"success","result":{..}}` / `{"status":"error","message":".."}`); HTTP 200 for both — the component maps `status`. HTTP 502 when the socket fails, 504 on the 180 s timeout. |
| `POST /command` with `type: get_viewport_screenshot` | `{"max_size":800,"format":"png"}` | the shim adds `"image_base64"` (it reads the `filepath` the add-on wrote to a temp dir, then deletes it) so the component can return `ImageContent`. |

Auth: optional shared token. If started with `--token <t>` the shim requires
`Authorization: Bearer <t>`; the component sends it when `BLENDER_BRIDGE_TOKEN`
is set. Recommend on, because `execute_code` is remote code execution in Blender.
Size cap 8 MiB on responses (`get_world_state_snapshot` can carry 4000 objects).

Alternative shim shape: fork the add-on to serve this HTTP contract itself
(`http.server` on a daemon thread + the existing `bpy.app.timers` queue). Same
contract, one less process, but a fork to maintain — only worth it if upstream
declines a PR adding an HTTP listener.

Component side:

- Base URL `BLENDER_BRIDGE_URL` default `http://host.wasmcloud.internal:9877`.
- `deploy/workload.yaml` and `.wash/config.yaml`: `allowedHosts: ["host.wasmcloud.internal:9877", "https://api.polyhaven.com", "https://api.sketchfab.com", "https://api.poly.pizza"]`, component `allowedHostLoopbackPorts: ["9877"]`, and the user turns on Settings → Security → allow host loopback (`PUT /v1/egress {"allow_host_loopback": true}`).
- Statics on the warm instance cache the `/health` result for ~10 s and the Poly Haven `/assets` catalog per type for 12 h (it is a 1 MB blob with `max-age=43200`).

### 3b. Alternative: polling bridge over the component's own ingress (after-effects pattern)

No loopback grant, no security toggle. The component exposes, next to the MCP
route, `GET /bridge/next` (long-poll ≤ 25 s) and `POST /bridge/result/{id}`,
both guarded by `BLENDER_BRIDGE_TOKEN`; the queue lives in `wasi:keyvalue`
(filesystem-backed, shared across instances). A small poller on the host —
either a fork of the add-on or a 60-line sidecar that bridges to TCP 9876 —
loops: fetch next job from `http://blender-mcp.localhost:8200/bridge/next`,
run it in Blender, post the result. A tool call enqueues and then waits (polling
keyvalue with the bridge deadline) for the result. Costs: ~250 ms extra latency
per call, a keyvalue schema (`job:{id}` → request / `res:{id}` → result, TTL
cleanup), and the poller still has to be written. Pick this if the loopback door
is a deployment blocker (managed machines) — otherwise 3a is simpler.

### 3c. Phase 0 that is buildable today

The Poly Haven / Sketchfab-search / Poly Pizza tools are plain HTTPS and need no
Blender. They could ship first as a `3d-assets-mcp` (Poly Haven ToS credit
required). They are only half useful without the import step, which is why
this is not a separate server yet.

## 4. Tool surface to mirror (component)

| Tool | Upstream | Params / clamps | Gated |
|---|---|---|---|
| `blender_status` | `GET {bridge}/health` (+ add-on `get_addon_info`) | – | no |
| `get_scene_info` | `POST /command {"type":"get_scene_info"}` | – (add-on returns first 10 objects: name, type, location; `object_count`, `materials_count`) | no |
| `get_world_state_snapshot` | `{"type":"get_world_state_snapshot"}` | `max_objects` 1..4000 (default 500; component truncates the add-on's list) | no |
| `get_object_info` | `{"type":"get_object_info","params":{"name"}}` | `object_name` exact `bpy` name, 1..256 chars | no |
| `get_viewport_screenshot` | `{"type":"get_viewport_screenshot","params":{"max_size","format":"png"}}` | `max_size` 64..2000 (default 800); returns `ImageContent` from the shim's `image_base64` | no |
| `execute_blender_code` | `{"type":"execute_code","params":{"code"}}` | `code` ≤ 64 KiB; result is captured stdout | **yes**: `BLENDER_ALLOW_EXECUTE_CODE=true` |
| `get_polyhaven_categories` | direct `GET api.polyhaven.com/categories/{type}` | `asset_type` ∈ hdris/textures/models | no |
| `search_polyhaven_assets` | direct `GET /search?q=&t=&limit=` when `query` given, else `GET /assets?type=&categories=` filtered locally | `asset_type` ∈ hdris/textures/models/all, `categories` list, `limit` 1..50 (default 20) | no |
| `get_polyhaven_files` | direct `GET /files/{id}` (+ `/info/{id}`) | `asset_id` slug `[a-z0-9_]+` | no |
| `download_polyhaven_asset` | `{"type":"download_polyhaven_asset","params":{"asset_id","asset_type","resolution","file_format"}}` (Blender downloads+imports; needs the add-on's Poly Haven checkbox) | `resolution` ∈ 1k/2k/4k/8k/16k, `file_format` hdr/exr/blend/gltf/fbx/jpg/png | yes (mutates scene): `BLENDER_ALLOW_IMPORT=true` |
| `set_texture` | `{"type":"set_texture","params":{"object_name","texture_id"}}` | – | yes (mutates scene) |
| `search_sketchfab_models` | direct `GET api.sketchfab.com/v3/search?type=models&q=&downloadable=&categories=&count=&cursor=` | `count` 1..24, `cursor` from previous `cursors.next` | no |
| `get_sketchfab_model` | direct `GET /v3/models/{uid}` | `uid` 32 hex | no |
| `get_sketchfab_download_urls` | direct `GET /v3/models/{uid}/download` with `Authorization: Token` | – ; URLs expire in 300 s | no (needs `SKETCHFAB_API_TOKEN`) |
| `download_sketchfab_model` | `{"type":"download_sketchfab_model","params":{"uid","target_size"}}` (Blender uses its own key from add-on prefs) | `target_size` 0.01..1000 m | yes (mutates scene) |
| `search_polypizza_models` | direct `GET api.poly.pizza/v1.1/search/...` with `x-auth-token` | `limit` 1..50, `page` ≥ 1, `category` 0..11, `licence` CC0/CC-BY/CC-BY-SA, `animated` | no (needs `POLYPIZZA_API_KEY`) |
| `download_polypizza_model` | `{"type":"download_polypizza_model","params":{"model_id","normalize_size","target_size"}}` | `target_size` 0.01..1000 | yes (mutates scene) |

Left out on purpose: Hyper3D/Hunyuan generation (paid, extra secrets, polling),
telemetry/trajectory tools, `drain_human_activity`.

## 5. Configuration (per CONVENTIONS.md)

| Env | Kind | Required | Default | Notes |
|---|---|---|---|---|
| `BLENDER_BRIDGE_URL` | named config | no | `http://host.wasmcloud.internal:9877` | e2e overrides to the fixture |
| `BLENDER_BRIDGE_TOKEN` | secret ref `blender-mcp-bridge-token` | no | – | shared bearer the shim was started with |
| `BLENDER_ALLOW_EXECUTE_CODE` | named config | no | `false` | gates `execute_blender_code` |
| `BLENDER_ALLOW_IMPORT` | named config | no | `true` | gates scene-mutating asset imports |
| `BLENDER_COMMAND_TIMEOUT_SECS` | named config | no | `60` | outbound deadline to the bridge (add-on side is 180 s) |
| `SKETCHFAB_API_TOKEN` | secret ref `blender-mcp-sketchfab-token` | no | – | sketchfab.com → Settings → Password & API → API token; only `get_sketchfab_download_urls` needs it |
| `POLYPIZZA_API_KEY` | secret ref `blender-mcp-polypizza-key` | no (required for polypizza tools) | – | poly.pizza/settings/api (free) |
| `POLYHAVEN_BASE_URL`, `SKETCHFAB_BASE_URL`, `POLYPIZZA_BASE_URL` | test override | no | real hosts | fixture routing |

Hardcode `User-Agent: blender-mcp (cosmonic-desktop)` on Poly Haven calls (ToS 2.4).

Secret registration:

```console
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs -H 'Content-Type: application/json' \
    -d '{"name":"blender-mcp-bridge-token","uri":"keychain://cosmonic/blender-mcp-bridge-token","env":"BLENDER_BRIDGE_TOKEN","value":"<token>"}'
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs -H 'Content-Type: application/json' \
    -d '{"name":"blender-mcp-sketchfab-token","uri":"keychain://cosmonic/blender-mcp-sketchfab-token","env":"SKETCHFAB_API_TOKEN","value":"<token>"}'
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs -H 'Content-Type: application/json' \
    -d '{"name":"blender-mcp-polypizza-key","uri":"keychain://cosmonic/blender-mcp-polypizza-key","env":"POLYPIZZA_API_KEY","value":"<key>"}'
```

## 6. User setup steps (design 3a)

1. Blender 3.0+ (ahujasid supports 3.0+; Lab add-on needs 5.1). Install `addon.py` from ahujasid/blender-mcp v1.9.x via Edit → Preferences → Add-ons → Install; enable "Interface: Blender MCP".
2. In the 3D viewport sidebar (N) → BlenderMCP tab: tick Poly Haven / Sketchfab / Poly Pizza as wanted (Sketchfab and Poly Pizza keys go into the add-on preferences or `BLENDERMCP_*` env before launching Blender), then **Connect to MCP server** (starts the TCP server on 9876).
3. Run the shim: `python3 blender-http-bridge.py --listen 127.0.0.1:9877 --blender 127.0.0.1:9876 --token <t>`.
4. Desktop: Settings → Security → allow host loopback; register the secret refs; apply `deploy/workload.yaml` (`allowedHosts` + `allowedHostLoopbackPorts: ["9877"]`).
5. `claude mcp add --transport http blender-mcp http://blender-mcp.localhost:8200/`; call `blender_status` first.

## 7. Error catalogue (what the component should map)

| Condition | Meaning | Action |
|---|---|---|
| bridge connect refused / 503 from `/health` | shim not running, or Blender not running / "Connect to MCP server" not pressed | start Blender, click Connect in the BlenderMCP sidebar, start the shim |
| outbound blocked / DNS failure for `host.wasmcloud.internal` | loopback door closed or `allowedHostLoopbackPorts` missing 9877 | enable Settings → Security host loopback; check manifest |
| bridge 401 | token mismatch | register `blender-mcp-bridge-token` = the shim's `--token` |
| `{"status":"error","message":"Unknown command type: X"}` | integration checkbox off (Poly Haven / Sketchfab / Poly Pizza handlers are only registered when enabled) or add-on too old | tick the checkbox in the sidebar / update addon.py |
| `addon.protocol_version != 5` in `blender_status` | add-on/server protocol drift | install addon.py matching the mirrored version |
| bridge 504 / deadline exceeded | Blender busy (modal operator, heavy code, big download) | split the work; never sleep in `execute_code`; raise `BLENDER_COMMAND_TIMEOUT_SECS` |
| `execute_code` error message with traceback | Python raised inside Blender | fix the script; state may be half-applied — user should have saved first |
| Poly Haven 404 `No asset with id X` | wrong slug | use `search_polyhaven_assets` slugs verbatim |
| Poly Haven 400 `Unsupported asset type: X. Must be: hdris/textures/models/all` | bad `asset_type` | enum only |
| Sketchfab 401 `Authentication credentials were not provided.` / `Invalid API token` | download endpoint without/with bad token | register `blender-mcp-sketchfab-token`; search does not need it |
| Sketchfab 404 `Not found.` | bad uid or model removed | – |
| Poly Pizza 401 `You need an API key to do that dingus` / `API key not valid 😭` | missing / bad `x-auth-token` | register `blender-mcp-polypizza-key` |
| response > 8 MiB (snapshot) | scene too large | lower `max_objects` |

## 8. Effort estimate

- HTTP shim sidecar (Python stdlib, tests against a fake add-on socket): 1 day.
- Component: `src/blender.rs` (bridge client) + `src/assets.rs` (3 HTTPS APIs), 17 tools, SKILL.md + references, threaded fixture e2e: 2–3 days.
- Polling-bridge variant (3b) instead of 3a: +2 days (keyvalue queue, ingress routes, poller).
- Total 3–5 developer-days; plus a browser session to pin the Poly Pizza search route.

## 9. Risks

- `execute_blender_code` is arbitrary code execution in the user's Blender session (file I/O, network, subprocess). Ship gated off by default; document "save first"; consider mirroring ahujasid's safe-mode blocklist in the shim.
- No auth on the add-on socket: any local process can drive Blender once the server is started; the shim token only protects the HTTP leg.
- Upstream churn: ahujasid bumps `ADDON_PROTOCOL_VERSION` (5 today) and reshapes commands often (1.8.5 → 1.9.1 within weeks); pin the mirrored version and check `get_addon_info` at startup.
- Long-running commands vs. the outbound deadline; screenshot/import payload sizes.
- Poly Haven ToS: unique User-Agent and a visible credit in any UI that surfaces its content; Sketchfab model licenses (many are CC-BY-NC-ND) must be shown to the user before import.
- Poly Pizza API docs are Cloudflare-gated; the exact search route was inferred from two wrappers, not the docs.
- Blender Lab's official server is GPL — code cannot be borrowed; only ahujasid (MIT) and dcc-mcp-blender (MIT source) are borrowable.

## 10. Fixture / test plan (for the eventual build)

Threaded Python fixture serving four prefixes, each echoing `path`, `query`,
and selected request headers inside its JSON so tests can assert on encoding,
clamping, and auth:

- `/bridge`: `GET /health` (200 / 503 toggle), `POST /command` returning canned results per `type` (`get_scene_info`, `get_object_info` with unknown-name → `{"status":"error","message":"Object not found: X"}`, `get_viewport_screenshot` → tiny PNG base64, `execute_code` → echoes `code` length and a fake stdout, `get_world_state_snapshot` → N objects to exercise truncation and the size cap, `download_*` → success/`Unknown command type`), a `?slow=1` variant that sleeps past the deadline, and Bearer-token checking (401 when the fixture is started with `--token`).
- `/polyhaven`: `/types`, `/assets` (small catalog with 3 types, checks `categories` comma encoding), `/search`, `/categories/{t}`, `/files/{id}`, `/info/{id}`, with the verbatim 404/400 text bodies above; asserts the `User-Agent`.
- `/sketchfab`: `/v3/search` (echo `count` — assert the component clamps to 24, `q` unicode/URL-encoding, cursor round-trip), `/v3/models/{uid}`, `/v3/models/{uid}/download` (401 JSON without `Authorization: Token …`, echo the header value on success, `expires: 300`).
- `/polypizza`: `/v1.1/search…` and `/v1.1/model/{id}` returning 401 `{"error":"You need an API key to do that dingus"}` without `x-auth-token`.

Guard instance: started without `POLYPIZZA_API_KEY` / `SKETCHFAB_API_TOKEN` to
assert the actionable missing-secret messages; `BLENDER_ALLOW_EXECUTE_CODE`
unset to assert the gate.

Live options on this machine: `E2E_LIVE=1` smoke against `api.polyhaven.com`
(keyless, CDN-cached) and `api.sketchfab.com/v3/search` (keyless). There is no
Blender binary here; a live bridge test can run Blender headless in podman
(`linuxserver/blender` or `blender --background --python addon.py` in an
Ubuntu image with `pip`-less stdlib) publishing 9876, plus the shim, via the
Docker API at `127.0.0.1:2375` — optional, not part of the hermetic suite.

## 11. What Desktop could add to make this easy

- A host-side **local-app bridge**: an allow-listed `tcp://127.0.0.1:<port>` → HTTP adapter (or `wasi:sockets` outbound limited to granted loopback ports) so components could speak the add-on's socket protocol without a user-run sidecar.
- A per-workload loopback grant instead of the global Settings → Security toggle.
- A built-in "poll queue" capability (the after-effects pattern as a host interface) so live-app bridges do not each reinvent keyvalue job queues and long-poll routes.
- Bundled helper installers: Desktop could ship/launch the sidecar as a managed host process next to a workload.

Sources checked: ahujasid/blender-mcp `addon.py` + `src/blender_mcp/server.py` + PyPI (1.9.1, 2026-09-02); projects.blender.org `lab/blender_mcp` (v1.0.0 release 2026-04-27, `mcp_to_blender_server.py`, `blender_manifest.toml`, `mcp/manifest.json`); dcc-mcp-blender README/`server.py` + dcc-mcp-core `mcp_http_config.py`/`options.py`/`constants.py` (v0.2.3, 2026-08-25); api.polyhaven.com swagger + live calls; api.sketchfab.com live calls; api.poly.pizza live 401s + MatthewHallCom/Poly-Pizza-MCP `src/index.ts`/`types.ts`.
