# Tool reference

Progressive disclosure: this file is a supporting resource of the
`illustrator-mcp` skill. Clients pull it only when the SKILL.md body is not
enough. It is reachable at `skill://illustrator-mcp/references/TOOLS.md`,
which is also where the relative link in SKILL.md resolves to.

Per-argument JSON schemas come from `tools/list` and are not repeated here.
What is here is everything the schema cannot tell you: the calling model,
timing, the conventions shared across tools, and how each family fails.

## The calling model

Every tool other than the three pure-arithmetic helpers goes through the
bridge:

```
tools/call ──▶ command queued in wasi:keyvalue
                    │
                    └─ bridge (inside Illustrator) polls every ~2s, claims it,
                       runs it against the ExtendScript DOM, POSTs the result
                    ┌─
tool result ◀──────┘  (or a deadline message, if the wait ran out)
```

| Budget | Applies to | Value |
|---|---|---|
| Standard wait | most commands | 12 s |
| Slow wait | `run_batch`, `export_document`, `open_document`, `save_document`, `place_image`, `run_jsx` | 240 s |
| Bridge considered silent | any command | no poll for 30 s |

A wait that runs out is **not** a failure: the command stays queued and will
execute. Call `get_results` for its outcome rather than re-issuing it.

## Shared conventions

| Convention | Detail |
|---|---|
| Coordinates | Points from the **active artboard's top-left**, y increasing downward. `set_active_artboard` changes what those numbers mean. |
| Colors | CSS hex (`"#1a73e8"`, `"#fff"`, `"#1a73e8ff"`), or the literal `"none"`. |
| `opacity` | 0–100, not 0–1. |
| `name` | Optional on every creation tool. Supply it: `select_by_name` is the only stable handle, since indices shift. |
| `layer` | Optional on every creation tool; targets a layer by name. Defaults to the active layer. |
| Overwrites | Every file-writing tool (`save_document`, `export_document`) refuses to replace an existing file unless `overwrite: true`. |

## Pure-compute helpers — no bridge needed

`hex_to_rgb`, `rgb_to_hex`, `convert_units` answer from the request alone.
They work with Illustrator closed, and they never consume a poll cycle.

- `hex_to_rgb` returns both `rgba_255` and `rgba_float` (0..1). The float form
  is what After Effects and most other tools want.
- `convert_units` takes `value`, `from`, `to` over `pt | px | in | mm | cm |
  pica`. `pt` and `px` are 1:1 (Illustrator's 72 ppi convention).

## Bridge health

| Tool | Use it when |
|---|---|
| `bridge_status` | Before any session of real work, and first whenever a call seems to hang. Reports `panelConnected`, `lastPollAgeMs`, whether a *stale* bridge is polling, and a `hint` naming the specific fix. |
| `get_results` | After any "queued" or "no result arrived" message. Returns the most recent result. |
| `get_help` | Setup steps for both bridge vehicles. |

A **stale** bridge polling is a distinct state from no bridge, and it needs a
different fix: reinstall (`./install-bridge.sh`), not just reopen. Both
`bridge_status` and the `GET /` discovery document report it.

## Reading the document

| Tool | Returns |
|---|---|
| `get_document_info` | Name, dimensions, color mode, artboard count. |
| `list_documents` | Every open document. |
| `list_page_items` | `{type, name, x, y, width, height, index, layer, hidden, locked}` per item. With `detail: true`, adds `fill`, `stroke`, `strokeWidth`, `opacity`, `closed`, `pathPoints`, and text contents/font/size. `layer` restricts to one layer; `limit` defaults to 200. |
| `list_text_frames` | Every text frame with `contents` (first 120 chars), `font` (PostScript name), `fontSize`, `color`, and bounds. |
| `list_artboards` | Position and size of each artboard. |
| `list_layers` | Name, visibility, lock state. |
| `list_swatches` | The document palette: `name`, `type`, `hex`. |
| `list_fonts` | Installed fonts by PostScript name; filter with `contains`. |

`x`/`y` are the item's **top-left**, not its center. Converting to a
center-anchored app is `[x + width/2, y + height/2]` — see
[HANDOFF.md](HANDOFF.md).

Text `contents` is truncated to 120 characters. For the full string of a long
text frame, read it with `run_jsx` (if enabled) or ask the user.

## Drawing

`draw_rectangle` / `draw_ellipse` take `x`, `y`, `width`, `height` (top-left
box). `draw_star` takes `center_x`, `center_y`, `radius` — a center, not a
box — plus `inner_radius`, `points`, and `polygon: true` for a regular
polygon. `draw_polygon` takes `points` as `[[x, y], …]` and closes the path
unless `closed: false`.

`add_text` is point text unless both `width` and `height` are given, which
makes it an area text frame. `font` is a PostScript name — check it against
`list_fonts` first; an unresolvable font is an Illustrator-side error.

`set_text_frame` addresses an existing frame by `index` or `name` and can
change content, font, size, color, and position independently.

## Selection and transforms

The transform tools act on **the current selection**, which is Illustrator's
own selection state — shared with the human at the keyboard. Always establish
it explicitly (`select_by_name`, `select_all`) rather than assuming what is
selected, and `deselect_all` when finished so the user is not left with a
surprise selection.

`move_selection` takes a delta in points (`dy` positive = down).
`scale_selection` takes percentages (100 = unchanged). `rotate_selection`
takes degrees, positive counter-clockwise.

## Batching

`run_batch` takes `commands: [{command, args}, …]` using the underlying script
names (`drawRectangle`, `addText`, …, not the tool names) and runs them in one
round trip inside one undo group.

- Keep batches to roughly **100 commands**. Longer ones slow down as the
  document grows and can outrun even the 240 s budget.
- `continue_on_error: true` keeps going past a failing entry; the default
  stops at the first failure.
- A batch containing `runJsx` is refused wholesale when `MCP_ALLOW_RAW_JSX` is
  not enabled.
- Too large a batch can exceed the transport's request-body limit and come
  back as HTTP `413`. Split it.

## Escape hatches

`run_script` runs one bridge script by name with raw arguments — the same
surface the typed tools use, for anything whose typed wrapper does not fit.

`run_jsx` executes arbitrary ExtendScript inside Illustrator. It is gated by
the deployment's `MCP_ALLOW_RAW_JSX` environment variable; when disabled, both
it and any batch containing it are refused with a message saying so. That is a
policy decision, not a transient failure — do not retry it.

## Failure modes

| Condition | What you get |
|---|---|
| No bridge has ever polled | `isError: false`, text: command queued + install instructions |
| Bridge silent > 30 s | `isError: false`, text: command queued + poll age + "open the panel" |
| Stale bridge polling | as above, but the hint says *reinstall*, since reopening will not help |
| Wait ran out mid-execution | `isError: false`, text: "queued but no result arrived within Ns" — use `get_results` |
| Illustrator raised an error | `isError: true`, carrying Illustrator's own message (no open document, bad font, path exists, …) |
| Bad arguments | JSON-RPC `-32602` |
| `run_jsx` while disabled | `isError: true`, text naming `MCP_ALLOW_RAW_JSX` |
| Body over the transport limit | HTTP `413` before any JSON-RPC response |
| `Host` not in `MCP_ALLOWED_HOSTS` | HTTP `403` before any JSON-RPC response |

## Resources (skills)

This server publishes its skills over the MCP resources primitive:

| URI | Contents |
|---|---|
| `skill://index.json` | The catalog: skill names, trigger descriptions, and file URIs. |
| `skill://illustrator-mcp/SKILL.md` | The playbook. |
| `skill://illustrator-mcp/references/TOOLS.md` | This file. |
| `skill://illustrator-mcp/references/HANDOFF.md` | Moving artwork to After Effects or another app as editable layers. |

`GET /` returns the same catalog, plus live bridge status, without a protocol
handshake.
