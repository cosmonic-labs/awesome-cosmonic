# Tool reference

Progressive disclosure: this file is a supporting resource of the
`after-effects-mcp` skill. Clients pull it only when the SKILL.md body is not
enough. It is reachable at `skill://after-effects-mcp/references/TOOLS.md`,
which is also where the relative link in SKILL.md resolves to.

Per-argument JSON schemas come from `tools/list` and are not repeated here.
What is here is everything the schema cannot tell you: the calling model,
timing, the conventions shared across tools, and how each family fails.

## The transport

This server runs as a sandboxed WebAssembly component exporting
`wasi:http/handler@0.3.0` (WASI p3) and speaks MCP over the streamable HTTP
transport, **stateless** per the 2026-07-28 specification: every request is
self-contained, nothing you set on one call carries into the next, and there is
no session to resume. Responses stream as SSE when the client accepts it, so a
long-running tool reports progress rather than going silent.

## The calling model

Every tool except `get-help` goes through the panel:

```
tools/call ──▶ command queued in wasi:keyvalue
                    │
                    └─ MCP Bridge Auto panel (inside After Effects) polls every
                       ~2s, claims it, runs it against the AE scripting API,
                       POSTs the result back
                    ┌─
tool result ◀──────┘  (or a deadline message, if the wait ran out)
```

| Budget | Applies to | Value |
|---|---|---|
| Standard wait | most commands | 12 s |
| Slow wait | `run-batch`, `save-frame-png`, `save-project` | 240 s |
| Panel considered silent | any command | no poll for 30 s |

A wait that runs out is **not** a failure: the command stays queued and will
execute. Call `get-results` for its outcome rather than re-issuing it.

## Shared conventions

| Convention | Detail |
|---|---|
| Colors | `[r, g, b]`, each **0..1**. Not 0-255, not hex. |
| Positions | The layer's `[x, y]` **center**, in composition pixels, y down. |
| Opacity | 0–100 (`fillOpacity`, `strokeOpacity`), unlike colors. |
| Targeting a comp | `compName` — preferred. `compIndex` exists but item indices shift whenever footage is imported. Omit both to use the active composition. |
| Targeting a layer | `layerIndex`, 1-based within the composition. |
| Time | Seconds, not frames. |
| Overwrites | `save-frame-png` and `save-project` refuse to replace an existing file unless `overwrite: true`. |

## Panel health

| Tool | Use it when |
|---|---|
| `bridge-status` | Before any session of real work, and first whenever a call seems to hang. Reports `panelConnected`, `lastPollAgeMs`, whether a *stale* panel is polling, the `currentCommand`, and a `hint` naming the specific fix. |
| `get-results` | After any "queued" or "no result arrived" message. Returns the most recent result. |
| `get-help` | Setup steps, effect match names, effect template names, advanced script names. Answers locally — it is the one tool that works with no panel at all. |

A **stale** panel polling is a distinct state from no panel, and it needs a
different fix: reinstall (`./install-bridge.sh`) and reopen, not just reopen.
Both `bridge-status` and the `GET /` discovery document report it.

## Reading the project

| Tool | Returns |
|---|---|
| `get-project-info` | Project items and the active composition. |
| `list-compositions` | Every composition in the project. |
| `get-layer-info` | The layers of the active composition. |

Read these before writing. Rebuilding on an assumption about what is in the
project is how a second copy of a composition gets created.

## Compositions

`create-composition` takes `name`, `width` (1920), `height` (1080),
`pixelAspect` (1.0), `duration` (10 s), `frameRate` (30), `backgroundColor`.

`set-composition-properties` changes duration, frame rate, dimensions, or
background color of an existing comp.

**The background color is a preview backdrop only.** It never renders into the
alpha channel — a comp with no full-bleed layer is already transparent when
rendered with alpha. Use a solid layer if the design has a real background.

`delete-composition` deletes **every** composition with the given name. That is
what makes it useful for rebuilding a scene from scratch, and also what makes
it worth confirming before calling.

## Layers

| Tool | Notes |
|---|---|
| `create-text-layer` | `text`, `position` (baseline, default `[960, 540]`), `fontSize` (72), `color` (0..1, default white), `fontFamily` (default Arial), `startTime`, `duration`. An unresolvable `fontFamily` is substituted silently by After Effects — check it. |
| `create-shape-layer` | `shapeType` (rectangle/ellipse/polygon/star), `position` (center), `size` `[w, h]`, `fillColor`, `strokeColor`, `strokeWidth`, `strokeOpacity`, `dash` `[len, gap]`, `roundness` (rectangle corner radius, px), `fillOpacity`, `fillNone` for outline-only, `points` for polygon/star, `name`. |
| `create-solid-layer` | Solids and adjustment layers. |
| `add-image-layer` | `path` must be absolute. PNG/JPEG/TIFF — **not WebP**, which After Effects cannot read. Size with `height` or `width` in comp pixels (aspect preserved) or an explicit `scale` `[x, y]` percentage. Re-imports of the same path are reused rather than duplicated. |
| `set-layer-properties` | Change an existing layer rather than deleting and recreating it — recreating loses keyframes and effects. |

Layers stack: each new layer goes on top. Create in back-to-front order and the
z-order comes out right with no reordering.

## Animation

`set-layer-keyframe` sets one property at one time. `set-layer-expression`
attaches an expression to a property — the route to wiggle, loops, and
follow-the-leader motion without a keyframe per frame.

Expression-heavy projects get slower to modify as they grow, which is why
`run-batch`'s budget is 240 s rather than 12.

## Effects

`apply-effect` takes an After Effects **match name** — the internal identifier,
not the UI label. `get-help` lists the common ones (`"ADBE Gaussian Blur 2"`,
`"ADBE Glow"`, `"ADBE Drop Shadow"`, …).

`apply-effect-template` takes a friendlier preset name: `gaussian-blur`,
`directional-blur`, `color-balance`, `brightness-contrast`, `curves`, `glow`,
`drop-shadow`, `cinematic-look`, `text-pop`. Prefer these unless you need a
specific parameter the template does not set.

## Output

`save-frame-png` renders one frame of a composition to an absolute `.png`
path. **This is the verification tool** — render and look, rather than
asserting the result is correct. `time` selects the moment in seconds.

`save-project` writes the `.aep`. With no `path`, saves in place.

## Batching

`run-batch` takes `commands: [{command, args}, …]` using the underlying script
names (`createShapeLayer`, `createTextLayer`, …, **not** the hyphenated tool
names) and runs them in order in one round trip inside one undo group.

- Keep batches to roughly **100 commands**. Longer ones slow down as the
  project accumulates expression-driven layers, and can outrun even the 240 s
  budget.
- `continueOnError: true` keeps going past a failing entry; the default stops
  at the first failure.
- `undoGroup` labels the undo step the user sees in After Effects. Set it to
  something they will recognize.
- Too large a batch can exceed the transport's request-body limit. Split it.

## Escape hatch

`run-script` runs one of the predefined scripts by name with raw parameters —
the same surface the typed tools use. It is the route to the commands with no
dedicated tool: `createCamera`, `duplicateLayer`, `deleteLayer`,
`setLayerMask`, `batchSetLayerProperties`, `bridgeTestEffects`.

The script name is checked against an allow-list; anything else is rejected
with the list of what is permitted. There is **no** arbitrary-ExtendScript
hatch on this server.

## Failure modes

Results are **structured**: every tool returns `structuredContent` alongside
its text, so read the fields rather than parsing prose.

| Condition | What you get |
|---|---|
| Panel has never polled | `isError: true`, `status: "queued-not-executed"` + install instructions. The command IS queued and will run. |
| Panel silent > 30 s | `isError: true`, `status: "queued-not-executed"` + poll age + "open the panel". Also still queued. |
| Stale panel polling | as above, but the hint says *reinstall*, since reopening will not help |
| Wait ran out mid-execution | `isError: false`, `status: "pending"` with the `commandId` — the panel is probably still working. Use `get-results`. |
| After Effects raised an error | `isError: true`, carrying the app's own message (no active comp, bad match name, file exists, …) |
| Bad arguments, unknown enum value, script not on the allow-list | `isError: true`, text `failed to deserialize parameters: …` — rejected in the SDK's deserialization step, so **nothing was queued** |
| Unknown tool or method | JSON-RPC `-32601` |
| No resource at a `skill://` URI | JSON-RPC `-32002` |
| Keyvalue store unreachable | JSON-RPC `-32603` — infrastructure, not your arguments |
| Body over the transport limit | HTTP `413` before any JSON-RPC response. Usually a `run-batch` that is too large; split it. |
| `Host` not in `MCP_ALLOWED_HOSTS` | HTTP `403` before any JSON-RPC response |

Note the distinction that matters most: **"queued-not-executed" is an error but
not a loss.** The command sits in the queue and runs the moment a panel
appears. Do not re-issue it — fix the panel, then `get-results`.

## Resources (skills)

This server publishes its skills over the MCP resources primitive:

| URI | Contents |
|---|---|
| `skill://index.json` | The catalog: skill names, trigger descriptions, and file URIs. |
| `skill://after-effects-mcp/SKILL.md` | The playbook. |
| `skill://after-effects-mcp/references/TOOLS.md` | This file. |
| `skill://after-effects-mcp/references/HANDOFF.md` | Rebuilding an Illustrator design as native, editable layers. |

`GET /` returns the same catalog, plus live panel status, without a protocol
handshake. It is served outside the `Host` guard, so a probe reaches it under
any hostname.
