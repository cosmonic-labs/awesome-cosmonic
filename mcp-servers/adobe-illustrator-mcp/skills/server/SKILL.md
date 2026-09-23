---
name: illustrator-mcp
description: Operate a live Adobe Illustrator instance — inspect a document's layers, text frames, artboards and swatches; draw shapes and text; transform and style selections; export. Use when connected to this server, when a request involves Illustrator or a .ai file, and ESPECIALLY when artwork is being moved into After Effects or another app, where the structure must be rebuilt as editable layers rather than exported as a flat image.
---

# Using the illustrator-mcp MCP server

This server drives a **real, running copy of Adobe Illustrator** on the user's
machine. It is not a renderer: everything it does happens in the app the user
is looking at, and every change is visible to them and undoable by them.

The MCP transport is **stateless** — nothing you set on one call carries into
the next. The Illustrator *document*, of course, is not: it is the state, and
it persists across calls, across sessions, and across clients.

## The one thing to get right first

Illustrator has no remote API. A **bridge** runs inside Illustrator and polls
this server every ~2 seconds; a tool call queues a command and waits for the
bridge to claim it, run it, and post the result back.

So: **if no bridge is running, every live tool queues and times out.** Call
`bridge_status` before a session of real work, and read its `hint` — it names
the specific fix. `GET /` on this server reports the same thing without a
protocol handshake.

Two consequences shape everything below:

1. **Every live call costs at least one ~2 s poll cycle.** Fifty tool calls is
   two minutes of latency. `run_batch` is one round trip for the whole list —
   use it for anything more than a couple of operations.
2. **A tool that reports "queued but the bridge is not listening" did not
   fail.** The command is still queued and will run when a bridge appears.
   Do not re-queue it; fix the bridge, then call `get_results`.

## Moving artwork to After Effects (or anywhere else)

**Read [references/HANDOFF.md](references/HANDOFF.md) before you start one.**

The short version, because it is the mistake that matters most here:

> When a user asks to bring an Illustrator design into After Effects, they
> almost never want a picture of it. They want the **artwork rebuilt as
> editable layers** — the boxes as shape layers, the headline as a live text
> layer, each asset as its own layer that can be moved, retimed, and
> keyframed. A flattened PNG of the canvas is unanimatable and unfixable: the
> text cannot be corrected, a rectangle cannot change color, nothing can move
> independently.

So the handoff is a **structure read, not an image export**:

```
list_artboards      → canvas size, and the origin every coordinate is relative to
list_layers         → the layer stack, and what is hidden or locked
list_page_items     → every object: type, name, bounds, fill, stroke
list_text_frames    → the strings themselves, with font, size and color
list_swatches       → the palette, so colors are reused rather than eyeballed
```

That set is enough to reconstruct the design natively in the destination app.
Export a raster **only** for things that genuinely are raster (a photograph, a
placed image, a gradient mesh that will not survive translation) and for a
side-by-side visual check of the rebuild — never as the deliverable itself.

## Tools

Grouped by what they need. Full schemas and failure modes:
[references/TOOLS.md](references/TOOLS.md).

| Group | Tools |
|---|---|
| **No bridge needed** (pure arithmetic) | `hex_to_rgb`, `rgb_to_hex`, `convert_units` |
| **Bridge health & help** | `bridge_status`, `get_results`, `get_help` |
| **Read the document** | `get_document_info`, `list_documents`, `list_page_items`, `list_text_frames`, `list_artboards`, `list_layers`, `list_swatches`, `list_fonts` |
| **Documents & files** | `new_document`, `open_document`, `save_document`, `close_document`, `export_document`, `place_image` |
| **Artboards & layers** | `add_artboard`, `set_active_artboard`, `add_layer`, `set_layer`, `delete_layer` |
| **Draw** | `draw_rectangle`, `draw_ellipse`, `draw_line`, `draw_polygon`, `draw_star`, `add_text`, `set_text_frame` |
| **Select & transform** | `select_all`, `deselect_all`, `select_by_name`, `get_selection`, `move_selection`, `scale_selection`, `rotate_selection`, `duplicate_selection`, `delete_selection` |
| **Appearance & arrange** | `set_fill`, `set_stroke`, `set_opacity`, `group_selection`, `ungroup_selection`, `bring_to_front`, `send_to_back` |
| **History & bulk** | `undo`, `redo`, `run_batch` |
| **Escape hatches** | `run_script`, `run_jsx` (raw ExtendScript; gated by `MCP_ALLOW_RAW_JSX`) |

## Conventions that will bite you otherwise

- **Coordinates are points from the ACTIVE artboard's top-left, y increasing
  downward.** This is screen convention, not Illustrator's native y-up space —
  the command library converts. If a document has several artboards, call
  `set_active_artboard` first: the same numbers mean different places on
  different artboards.
- **Colors are CSS hex strings** (`"#1a73e8"`), or the literal `"none"`.
  Use `hex_to_rgb` when you need Illustrator's 0-255 triple for something else.
- **Names are how you address things later.** Name shapes and layers as you
  create them; `select_by_name` is the only stable handle, since indices shift
  as objects are added and removed.
- **Verify visually.** After building anything non-trivial, `export_document`
  to a PNG and look at it. Bounds arithmetic that reads correctly is regularly
  wrong on screen.

## How to work with this server

1. **Read before you write.** `get_document_info` and `list_page_items` on the
   real document beat any assumption about what is on the canvas.
2. **Batch.** One `run_batch` with 40 commands beats 40 calls by about two
   minutes, and it lands in a single undo group, so the user can back it out
   with one Cmd-Z.
3. **Keep batches around 100 commands.** Longer ones get slower as the
   document grows and start colliding with the result deadline.
4. **Reuse the document's own palette.** `list_swatches` first; matching an
   existing swatch is what makes added artwork look like it belongs.
5. **Prefer the schema-checked tools over `run_jsx`.** The raw hatch is there
   for the DOM corners nothing else reaches, and it may be disabled entirely
   (`MCP_ALLOW_RAW_JSX=false`) on a locked-down deployment.

## Reading errors

- `"isError": true` inside a `result` — the tool ran and Illustrator refused.
  The text carries Illustrator's own message; surface it.
- **"queued but the bridge is not listening"** — not a failure. The command
  waits. Fix the bridge as the message says, then `get_results`.
- **"no result arrived within Ns"** — the bridge is probably mid-batch. Do not
  re-queue; `bridge_status`, then `get_results`.
- JSON-RPC `-32602` — the call itself was malformed. Fix the arguments.
- HTTP `403` before any JSON-RPC response — the DNS-rebinding guard rejected
  the `Host` header; the deployment's `MCP_ALLOWED_HOSTS` does not list it.
- HTTP `413` — the request body exceeded the transport limit. Usually a
  `run_batch` that is too large; split it.

## Server metadata without a protocol handshake

`GET /` returns a JSON discovery document: server name and version, MCP spec
version, endpoint paths, tool names, the skills served, **and the live bridge
status**. It is the cheapest way to confirm both that the deployment is up and
that Illustrator is actually reachable.
