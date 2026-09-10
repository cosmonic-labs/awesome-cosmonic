---
name: after-effects-mcp
description: Operate a live Adobe After Effects instance — create compositions, shape/text/solid layers, keyframes, expressions and effects; render frames to verify. Use when connected to this server, when a request involves After Effects or motion graphics, and ESPECIALLY when a design is arriving from Illustrator or another tool, where it must be rebuilt as native editable layers rather than imported as a flat image.
---

# Using the after-effects-mcp MCP server

This server drives a **real, running copy of Adobe After Effects** on the
user's machine. It is not a renderer: everything it does happens in the app the
user is looking at, and every change is visible to them and undoable by them.

The MCP transport is **stateless** — nothing you set on one call carries into
the next, and there is no session to resume. The After Effects *project*, of
course, is not: it is the state, and it persists across calls, across sessions,
and across clients.

Every tool returns `structuredContent`, so read the fields rather than parsing
prose out of the text block.

## The one thing to get right first

After Effects has no remote API. The **MCP Bridge Auto panel** runs inside
After Effects and polls this server every ~2 seconds; a tool call queues a
command and waits for the panel to claim it, run it, and post the result back.

So: **if the panel is not open, every tool queues and times out.** Call
`bridge-status` before a session of real work, and read its `hint` — it names
the specific fix. `GET /` on this server reports the same thing without a
protocol handshake.

Two consequences shape everything below:

1. **Every call costs at least one ~2 s poll cycle.** Fifty tool calls is two
   minutes of latency. `run-batch` is one round trip for the whole list — use
   it for anything more than a couple of operations.
2. **A tool that reports "queued but the panel is not listening" did not
   fail.** The command is still queued and will run when the panel appears.
   Do not re-queue it; fix the panel, then call `get-results`.

## Bringing a design in from Illustrator (or anywhere else)

**Read [references/HANDOFF.md](references/HANDOFF.md) before you start one.**

The short version, because it is the mistake that matters most here:

> When a user brings an Illustrator design into After Effects, they want to
> **animate** it. That requires the artwork to still be made of parts: each box
> a shape layer, each headline a live text layer, each asset its own layer that
> can be moved, retimed, and keyframed independently. Importing a flat PNG of
> the design and animating that gives you one rectangle of pixels — the text
> cannot be corrected, a color cannot change, nothing moves on its own.

So the import is a **rebuild, not a picture**:

```
create-composition   ← artboard size; ask for frame rate and duration
create-shape-layer   ← one per rectangle / ellipse / polygon / star
create-text-layer    ← one per string, with its own font, size and color
create-solid-layer   ← backgrounds and adjustment layers
add-image-layer      ← ONLY for genuinely raster assets, each on its own layer
```

Send it all in one `run-batch`: one round trip, one undo group, seconds
instead of minutes. Build **bottom layer first** — After Effects stacks new
layers on top, so creating in the source's back-to-front order reproduces the
z-order for free — and name every layer as you go.

Then verify with `save-frame-png` and compare against the source render.

## Tools

Full argument details and failure modes: [references/TOOLS.md](references/TOOLS.md).

| Group | Tools |
|---|---|
| **Panel health & help** | `bridge-status`, `get-results`, `get-help` |
| **Read the project** | `get-project-info`, `list-compositions`, `get-layer-info` |
| **Compositions** | `create-composition`, `set-composition-properties`, `delete-composition` |
| **Layers** | `create-text-layer`, `create-shape-layer`, `create-solid-layer`, `add-image-layer`, `set-layer-properties` |
| **Animation** | `set-layer-keyframe`, `set-layer-expression` |
| **Effects** | `apply-effect`, `apply-effect-template` |
| **Output** | `save-frame-png`, `save-project` |
| **Bulk & escape hatch** | `run-batch`, `run-script` |

`run-script` reaches the commands with no dedicated tool: `createCamera`,
`duplicateLayer`, `deleteLayer`, `setLayerMask`, `batchSetLayerProperties`,
`bridgeTestEffects`.

## Conventions that will bite you otherwise

- **Colors are `[r, g, b]` floats in 0..1**, not 0-255 and not hex. A design
  tool will hand you hex; convert once, up front. (The `illustrator-mcp`
  server's `hex_to_rgb` returns exactly this form as `rgba_float`.)
- **Positions are the layer's `[x, y]` *center*** in composition pixels, y
  increasing downward. A source that reports top-left corners plus a size
  needs `[x + w/2, y + h/2]`.
- **Text layers position at the baseline**, left-justified by default — not at
  the top of the text box. Expect to nudge; verify with `save-frame-png`.
- **Address compositions by `compName`, not `compIndex`.** Item indices shift
  whenever footage is imported.
- **A composition's background color is a preview backdrop only.** It never
  renders into the alpha channel. A comp with no full-bleed layer is already
  transparent when rendered with alpha — do not add a solid to "fix" that
  unless the user wants an opaque background.
- **After Effects cannot read WebP.** `add-image-layer` takes PNG, JPEG, or
  TIFF.

## How to work with this server

1. **Read before you write.** `list-compositions` and `get-layer-info` on the
   real project beat any assumption about what is there.
2. **Batch.** One `run-batch` with 40 commands beats 40 calls by about two
   minutes, and it lands in a single undo group the user can back out with one
   Cmd-Z.
3. **Keep batches to roughly 100 commands.** Longer ones get slower as the
   project accumulates expression-driven layers.
4. **Verify visually, always.** `save-frame-png` and look at the result.
   Position arithmetic that reads correctly is regularly wrong on screen.
5. **Rebuild by editing, not by deleting.** When a frame check shows something
   off, fix it with `set-layer-properties` on the existing layer. Deleting and
   recreating loses keyframes and effects already applied.

## Reading errors

Check `structuredContent.status` first — it tells the outcomes apart, and they
have different fixes.

- **`status: "queued-not-executed"`** (with `isError: true`) — no panel is
  listening. **This is not a loss**: the command is queued and runs the moment
  a panel appears. Do not re-issue it. Open the panel as the message says,
  then `get-results`.
- **`status: "pending"`** (no `isError`) — the panel took longer than the
  budget, probably mid-batch. Do not re-queue; `bridge-status`, then
  `get-results` with the `commandId` you were given.
- **`isError: true` with an After Effects message** — the command reached the
  app and it refused (no active composition, bad match name, file exists).
  Surface the app's own text.
- **"An outdated bridge panel is polling"** — a distinct state from no panel,
  and reopening will not help. Run `./install-bridge.sh`, then close and
  reopen the panel.
- **`isError: true` with "failed to deserialize parameters"** — the arguments
  did not fit the schema, so the SDK rejected them **before any tool body
  ran**: nothing was queued and nothing happened in After Effects. The message
  names the problem (an unknown `shapeType` or `templateName`, a missing
  required field). Fix the call.
- JSON-RPC `-32603` — the keyvalue store failed. Infrastructure, not your
  arguments.
- JSON-RPC `-32002` — no resource at that `skill://` URI. Read
  `skill://index.json` for what is actually served.
- HTTP `413` — the request body exceeded the transport limit. Usually a
  `run-batch` that is too large; split it.
- HTTP `403` before any JSON-RPC response — the DNS-rebinding guard rejected
  the `Host` header; the deployment's `MCP_ALLOWED_HOSTS` does not list it.

## Server metadata without a protocol handshake

`GET /` returns a JSON discovery document: server name and version, MCP spec
versions, endpoint paths, tool names, the skills served, **and the live panel
status**. It is the cheapest way to confirm both that the deployment is up and
that After Effects is actually reachable.
