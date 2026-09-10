# Handing Illustrator artwork to After Effects (or any other app)

Progressive disclosure: this file is a supporting resource of the
`illustrator-mcp` skill, reachable at
`skill://illustrator-mcp/references/HANDOFF.md`. Read it whenever artwork is
leaving Illustrator for another application — most often After Effects, but
the same reasoning applies to Premiere, Figma, a web build, or a slide deck.

## The rule

**Rebuild the artwork as native, editable layers in the destination app. Do
not export a flat image and call it a handoff.**

When someone says "bring this design into After Effects", what they want next
is to *animate* it: slide the card in, type the headline on, pulse the accent
color, retime the whole thing. Every one of those needs the design to still be
made of parts:

| Rebuilt as layers | Flattened to a PNG |
|---|---|
| Rectangle animates, changes color, gets a stroke | One pixel grid; nothing moves independently |
| Headline is live text — retype it, restyle it, animate per-character | Text is baked; a typo means going back to Illustrator |
| Each element keyframes on its own schedule | The whole frame moves or nothing does |
| Scales to any comp size | Resamples and softens |
| Client asks for "the blue a bit darker" — one property | Round trip through Illustrator and re-export |

A screenshot of the canvas is the one deliverable that cannot be edited by the
app it was delivered to. Produce it for *verification*, never as the handoff.

## What to actually do

### 1. Read the structure out of Illustrator

Five calls describe a design completely enough to rebuild it. Batch them.

| Call | What it gives you |
|---|---|
| `list_artboards` | Canvas size and origin. The destination comp should match. |
| `list_layers` | The layer stack, plus what is hidden or locked (hidden art is usually *deliberately* not part of the design — ask before rebuilding it). |
| `list_page_items` with `detail: true` | Every object: `type`, `name`, `x`, `y`, `width`, `height`, `fill`, `stroke`, `strokeWidth`, `opacity`. |
| `list_text_frames` | The strings themselves, with `font`, `fontSize`, `color`, and bounds. |
| `list_swatches` | The document palette by name, so the rebuild reuses the brand colors rather than approximating them. |

Call `set_active_artboard` first if the document has more than one: every
coordinate you get back is relative to the **active** artboard's top-left.

### 2. Translate the coordinate and color conventions

These two conversions are where rebuilds go wrong. Get them right once, up
front.

**Position — top-left box to center point.** Illustrator reports each item's
top-left corner (`x`, `y`) plus `width`/`height`, y increasing downward. After
Effects positions a shape layer by its **center**:

```
ae_position = [ai_x + ai_width / 2, ai_y + ai_height / 2]
ae_size     = [ai_width, ai_height]
```

Both are y-down from the top-left of the canvas, so no axis flip is needed —
only the corner-to-center shift. Skip it and everything lands down-right by
half its own size, which reads as "the layout is subtly wrong" rather than as
an obvious bug.

**Color — hex to 0..1 floats.** Illustrator speaks CSS hex; After Effects
wants three floats in 0..1. `hex_to_rgb` returns exactly that as
`rgba_float` — take the first three components. Do not divide by 255 by hand
per color; batch the conversions.

**Units.** Illustrator points and After Effects pixels are 1:1 at
Illustrator's 72 ppi convention, so a 1920×1080 pt artboard is a 1920×1080
comp. If the document is in mm or inches, run the artboard dimensions through
`convert_units` to points first.

**Text baselines.** Illustrator gives you the top of a text frame's bounding
box; After Effects positions a text layer at its **baseline**, left-justified
by default. Start at roughly `ae_y = ai_y + 0.8 × fontSize`, then verify and
nudge — font metrics vary and this is an approximation, not a formula.

**Fonts.** Illustrator reports the PostScript name (`list_text_frames` →
`font`). Pass that same name through. If the destination cannot resolve it,
say so and ask — silently substituting a font changes the design.

### 3. Rebuild in the destination

For After Effects, via the `after-effects-mcp` server:

```
create-composition       ← artboard width/height; ask for frame rate + duration
create-shape-layer       ← one per rectangle/ellipse/polygon/star
create-text-layer        ← one per text frame, with its own font/size/color
add-image-layer          ← only for genuinely raster content (see below)
```

Build **bottom layer first**: After Effects stacks new layers on top, so
creating in Illustrator's back-to-front order reproduces the z-order for free.

Send it all in one `run-batch` on the After Effects side. It is one round trip
and one undo group instead of one poll cycle per layer, and a 30-layer design
is the difference between seconds and a minute.

Name every layer as you create it, using the Illustrator item's name where it
has one. Layer names are what makes the result workable for a human afterwards
— and they are the only stable handle for a later edit.

### 4. What legitimately stays raster

Rebuild vector geometry and text. Export an image only for:

- **Photographs and placed raster assets** — export the item on its own and
  bring it in with `add-image-layer` (PNG/JPEG/TIFF; After Effects cannot read
  WebP).
- **Gradient meshes, complex blends, live effects, intricate custom paths** —
  things with no clean equivalent in the destination. Export **each one as its
  own transparent PNG**, so it is still an independently animatable layer.
  That is very different from flattening the whole canvas.
- **Verification** — see below.

When you export a raster, export it at 2× the comp size and scale down. It
costs nothing and survives a later scale-up.

### 5. Verify by comparison, not by assertion

1. `export_document` the Illustrator artboard to a PNG.
2. `save-frame-png` the rebuilt After Effects comp at the same moment.
3. Look at both. Check element positions, text content, colors, and z-order.
4. Fix with property edits on the existing layers — `set-layer-properties`,
   `run-batch` — not by deleting and rebuilding.

State plainly what did not survive the translation (an unsupported effect, a
substituted font, a gradient approximated as a solid). A rebuild that is 95%
right and honest about the other 5% is useful; one presented as pixel-perfect
when it is not sends the user hunting.

## If the user really does want a flat image

Sometimes they do — a thumbnail, a reference still, a background plate.
`export_document` is the tool, and that is a fine answer to that question. The
point of this file is that it is the **wrong** answer to "get this design into
After Effects", which is the request that actually comes up. If it is
genuinely ambiguous, ask — one question is cheaper than an unanimatable comp.
