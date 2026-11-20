# Rebuilding an Illustrator design as After Effects layers

Progressive disclosure: this file is a supporting resource of the
`after-effects-mcp` skill, reachable at
`skill://after-effects-mcp/references/HANDOFF.md`. Read it whenever artwork is
arriving from another application — most often Adobe Illustrator, but the same
reasoning applies to Figma, a PDF, a Sketch file, or a design someone describes
to you.

## The rule

**Rebuild the artwork as native, editable After Effects layers. Do not import
a flat image of the design and animate that.**

The reason someone brings a design into After Effects is to make it move. That
requires the design to still be made of parts:

| Rebuilt as layers | Imported as a flat PNG |
|---|---|
| Each box is a shape layer: keyframe its position, scale, color, stroke | One rectangle of pixels; the box cannot move on its own |
| Headline is a live text layer: retype it, restyle it, animate per character | Text is baked in; a typo means going back to the design tool |
| Elements enter and exit on their own schedules | Everything moves together or nothing does |
| Scales to any comp size, renders crisp | Resamples and softens |
| "Make the blue darker" is one property | Round trip through the design tool and re-import |

An imported screenshot is the one asset After Effects cannot meaningfully
animate. Produce it for *verification*, never as the import.

## What to actually do

### 1. Get the structure, not a picture

If the design is in Illustrator and the `illustrator-mcp` server is connected,
five calls describe it completely enough to rebuild:

| Call | What it gives you |
|---|---|
| `list_artboards` | Canvas size — the composition dimensions. |
| `list_layers` | The layer stack, and what is hidden or locked. |
| `list_page_items` with `detail: true` | Every object: `type`, `name`, `x`, `y`, `width`, `height`, `fill`, `stroke`, `strokeWidth`, `opacity`. |
| `list_text_frames` | The strings, with `font` (PostScript name), `fontSize`, `color`, bounds. |
| `list_swatches` | The palette by name, so colors are reused rather than eyeballed. |

If the source is not reachable that way, ask the user to export the structure
(an SVG, or the layer list) rather than a PNG. A description of the design is
more useful here than a picture of it.

### 2. Translate the conventions

These two conversions are where rebuilds go wrong. Get them right once, up
front — for the whole design, not per layer.

**Position — top-left box to center point.** Illustrator (and most design
tools) report an item's top-left corner plus its size. After Effects positions
a layer by its **center**:

```
ae_position = [src_x + src_width / 2, src_y + src_height / 2]
ae_size     = [src_width, src_height]
```

Both are y-down from the top-left of the canvas, so there is no axis flip —
only the corner-to-center shift. Skip it and every element lands down-right by
half its own size, which reads as "the layout is subtly wrong" rather than as
an obvious bug.

**Color — hex to 0..1 floats.** Design tools speak CSS hex; every color
argument here (`fillColor`, `strokeColor`, `color`) is `[r, g, b]` with each
component in **0..1**. Not 0-255, not a hex string. The `illustrator-mcp`
server's `hex_to_rgb` returns this directly as `rgba_float` — take the first
three components. Convert the whole palette in one pass before you start
building.

**Units.** Illustrator points and After Effects pixels are 1:1 at
Illustrator's 72 ppi convention, so a 1920×1080 pt artboard is a 1920×1080
comp. Other units need converting to points first.

**Text baselines.** A design tool gives you the top of a text frame's bounding
box; `create-text-layer` positions at the **baseline**, left-justified by
default. Start at roughly `ae_y = src_y + 0.8 × fontSize`, then verify with a
frame render and nudge. Font metrics vary — this is a starting point, not a
formula.

**Fonts.** `fontFamily` must resolve on this machine. If the design's font is
not installed, After Effects substitutes silently and the result looks wrong
for a reason nobody can see. Say which font could not be resolved rather than
letting it slide.

### 3. Build it

```
create-composition        ← artboard width/height. ASK for frame rate and
                            duration; the design does not carry them.
run-batch                 ← every layer, in one round trip:
  createSolidLayer        ← background, if the design has one
  createShapeLayer        ← per rectangle / ellipse / polygon / star
  createTextLayer         ← per string
  addImageLayer           ← per genuinely raster asset
```

Four things make the difference between a usable rebuild and a mess:

1. **Batch it.** `run-batch` takes the whole layer list as
   `[{command, args}, …]` using the underlying script names, runs it in one
   round trip, and puts it in one undo group. A 30-layer design is seconds
   instead of a minute — and one Cmd-Z for the user if they hate it.
2. **Build bottom-up.** After Effects stacks each new layer on top, so
   creating in the design's back-to-front order reproduces the z-order with no
   reordering afterwards.
3. **Name every layer.** Use the source item's name where it has one. Layer
   names are what make the comp workable for the human who opens it next, and
   they are the handle for every later edit.
4. **Keep the comp background black and empty.** A composition's background
   color is a preview backdrop only — it never renders into the alpha channel.
   If the design has a real background, that is a solid layer, not the comp
   background.

### 4. What legitimately stays raster

Rebuild vector geometry and text. Import an image only for:

- **Photographs and placed raster assets** — `add-image-layer`, PNG/JPEG/TIFF
  (After Effects cannot read WebP). Size with `height`/`width` in comp pixels
  to keep the aspect, or an explicit `scale`.
- **Gradient meshes, complex blends, live effects, intricate custom paths** —
  things with no clean shape-layer equivalent. Ask for **each one as its own
  transparent PNG**, so it is still an independently animatable layer. That is
  very different from flattening the whole canvas.

Ask for those exports at 2× the comp size and scale down; it costs nothing and
survives a later scale-up.

### 5. Verify by comparison, not by assertion

1. `save-frame-png` the rebuilt comp at a moment where everything is on screen.
2. Compare against a render of the source design.
3. Check element positions, text content, colors, and z-order.
4. Fix with `set-layer-properties` (or a `batchSetLayerProperties` in a
   `run-batch`) on the **existing** layers. Deleting and recreating loses any
   keyframes and effects already applied.

State plainly what did not survive the translation — an unsupported effect, a
substituted font, a gradient approximated as a solid fill. A rebuild that is
95% right and honest about the other 5% is useful; one presented as
pixel-perfect when it is not sends the user hunting.

### 6. Then animate

Only once the rebuild verifies should you add motion — and this is the payoff
for having done it as layers:

- `set-layer-keyframe` for position, scale, opacity, rotation over time.
- `set-layer-expression` for driven motion (wiggle, follow, loop).
- `apply-effect` / `apply-effect-template` for looks (`glow`, `drop-shadow`,
  `text-pop`, `cinematic-look`).
- Stagger `startTime` per layer for a build-on sequence.

None of that is available on an imported flat image, which is the whole reason
for the rule at the top of this file.

## If the user really does want a flat image

Sometimes they do — a reference still, a background plate, a texture.
`add-image-layer` is the tool, and that is a fine answer to that question. The
point of this file is that it is the **wrong** answer to "bring this design
into After Effects", which is the request that actually comes up. If it is
genuinely ambiguous, ask — one question is cheaper than an unanimatable comp.
