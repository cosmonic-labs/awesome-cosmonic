#!/usr/bin/env python3
"""'Prompt to Production' hero animation — day and night variants.

Geometry, radii, colours and motion curves come from the source design system
(hero-scenes.jsx / _ds tokens), scaled from its 1280x720 stage to 1920x1080.
The night palette is the source's own dark ("embed") variant. Both comps render
on a transparent background.

usage: build_hero_themed.py [day|night|both]
"""
import json
import sys
import time
import urllib.request

BASE = "http://127.0.0.1:8200/mcp"
DUR = 18.0
S = 1.5
LOGO_DIR = "/Users/liam/Documents/Cosmonic/product/desktop/explainer/prompt-to-production/logos/png"


def px(v):
    return round(v * S, 1)


def hexc(h):
    h = h.lstrip("#")
    return [int(h[i:i + 2], 16) / 255.0 for i in (0, 2, 4)]


PURPLE = hexc("#685bc7")
PURPLE300 = hexc("#aa9fe5")
PURPLE600 = hexc("#564bad")
PURPLE_SOFT = hexc("#f1eefb")
PURPLE_SOFT_FG = hexc("#453b8c")
GUN = hexc("#253746")
GUN_TILE = hexc("#33414f")
NODE_DARK = hexc("#16242d")
N0 = hexc("#ffffff")
N300 = hexc("#c1ccd1")
N400 = hexc("#9aa7af")
N500 = hexc("#768692")
N600 = hexc("#5c6a74")
GREEN = hexc("#1fa971")
GREEN_FG = hexc("#4cc795")   # --status-running-fg (dark)
YELLOW = hexc("#ffb600")
BLUE = hexc("#2a74c7")
BLUE_SOFT = hexc("#e8f1fb")
TS_PURPLE = hexc("#aa9fe5")
WHITE_74 = hexc("#c9cfd4")

# Per-theme tokens — taken directly from the Cosmonic Desktop design system
# (_ds/tokens/colors.css), so the video matches the app it plays inside.
#
# Every colour is opaque: the comps carry a full-bleed `app-background` layer set
# to --surface-app, and partially transparent fills would premultiply badly if
# that layer is switched off for a transparent render.
#
# Text colours were chosen against measured WCAG contrast, not by eye. The day
# CTA in particular was --text-disabled (2.29:1, failing); it is now
# --neutral-700 at 7.47:1.
THEMES = {
    "Day": dict(
        canvas=hexc("#f4f7f8"),        # --surface-app
        node_bg=hexc("#ffffff"),       # --surface-card
        node_stroke=hexc("#c1ccd1"),   # --border-strong
        title=hexc("#253746"),         # --text-primary        11.4:1
        sub=hexc("#5c6a74"),           # --text-secondary       5.6:1 on card
        chip_fill=hexc("#fafbfc"),
        chip_stroke=hexc("#c1ccd1"),
        rail=hexc("#c1ccd1"),
        label=hexc("#5c6a74"),         # was --text-disabled    2.3:1 -> 5.2:1
        hero_title=hexc("#253746"),
        hero_sub=hexc("#5c6a74"),      #                        5.2:1
        caption=hexc("#45525b"),       # --neutral-700  CTA     7.5:1
        accent=hexc("#685bc7"),        # --accent (purple-500)
        skills_tint=hexc("#f1eefb"), skills_glyph=hexc("#453b8c"),
        mcp_tint=hexc("#e8f1fb"), mcp_glyph=hexc("#2a74c7"),
        desk_stroke=None,
        arrow=hexc("#768692"),
        logo_tint=None, logo_alpha=100,
        harness_text=hexc("#253746"),  #                       11.4:1
    ),
    "Night": dict(
        canvas=hexc("#0f1a24"),        # --surface-app  (dark "cosmic" theme)
        node_bg=hexc("#16242d"),       # --surface-card
        node_stroke=hexc("#2a3d49"),   # --border-default
        title=hexc("#eef2f5"),         # --text-primary        15.6:1
        sub=hexc("#aab8c1"),           # --text-secondary       7.8:1 on card
        chip_fill=hexc("#1b252d"),
        chip_stroke=hexc("#3a4f5c"),   # --border-strong
        rail=hexc("#3a4f5c"),
        label=hexc("#7e8d97"),         # --text-muted           5.2:1
        hero_title=hexc("#eef2f5"),
        hero_sub=hexc("#aab8c1"),      #                        8.7:1
        caption=hexc("#aab8c1"),       # CTA                    8.7:1
        accent=hexc("#8a7cd9"),        # --accent (purple-400)
        skills_tint=hexc("#1d2d38"), skills_glyph=hexc("#b6acec"),
        mcp_tint=hexc("#1d2d38"), mcp_glyph=hexc("#a79fe5"),
        desk_stroke=hexc("#2a3d49"),
        arrow=hexc("#7e8d97"),
        logo_tint=N0, logo_alpha=100,
        harness_text=hexc("#eef2f5"),  #                       15.6:1
    ),
}

R_PROMPT, R_DESK, R_NODE, R_CHIP, R_TILE, R_ICON = (
    px(14), px(16), px(12), px(12), px(10), px(8))

FONT = "WorkSans-Regular"
FONT_MED = "WorkSans-Medium"
FONT_SEMI = "WorkSans-SemiBold"
FONT_BOLD = "WorkSans-Bold"
MONO = "Menlo-Regular"

ENTER_DUR = 0.55


def enter_opacity(alpha=100, d=ENTER_DUR):
    return f"clamp(((time - inPoint) / {d}) * 1.8, 0, 1) * {alpha}"


def pop_opacity(alpha=100):
    return f"clamp(((time - inPoint) / 0.6) * 2.2, 0, 1) * {alpha}"


def enter_position(x, y, dy=12, d=ENTER_DUR):
    return (f"p = clamp((time - inPoint) / {d}, 0, 1);\n"
            f"e = 1 - Math.pow(1 - p, 3);\n"
            f"[{x}, {y} + (1 - e) * {px(dy)}]")


POP_SCALE = ("p = clamp((time - inPoint) / 0.6, 0, 1);\n"
             "c = 1.70158 + 1;\n"
             "e = 1 + c * Math.pow(p - 1, 3) + 1.70158 * Math.pow(p - 1, 2);\n"
             "s = (0.9 + 0.1 * e) * 100;\n"
             "[s, s]")

DRAW_SCALE = ("p = clamp((time - inPoint) / 0.5, 0, 1);\n"
              "e = p < 0.5 ? 4 * p * p * p : 1 - Math.pow(-2 * p + 2, 3) / 2;\n"
              "[e * 100, 100]")


# The 2026-07-28 MCP transport requires all three of these on every request:
# an Accept that includes text/event-stream (responses stream as SSE), an
# Mcp-Method header naming the JSON-RPC method, and a `_meta` in params
# carrying the protocol version. A request missing any of them is rejected
# before it reaches a tool.
PROTOCOL_VERSION = "2026-07-28"
META = {
    "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
    "io.modelcontextprotocol/clientCapabilities": {},
}


def rpc(method, params, timeout, name=None):
    params = dict(params, _meta=META)
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": PROTOCOL_VERSION,
        "Mcp-Method": method,
        "Host": "ae-mcp.localhost.cosmonic.sh",
    }
    if name:
        headers["Mcp-Name"] = name
    req = urllib.request.Request(BASE, data=body, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        payload = r.read().decode()
    # Responses arrive as SSE when the server streams; take the first data
    # frame, which carries the JSON-RPC message.
    for line in payload.splitlines():
        if line.startswith("data: "):
            return json.loads(line[6:])
    return json.loads(payload)


def tool(name, args):
    """Mutating batches are never retried — a retry while the panel is still
    executing queues the same work twice."""
    mutating = name == "run-batch"
    timeout = 300 if mutating else 60
    attempts = 1 if mutating else 3
    for attempt in range(attempts):
        try:
            result = rpc("tools/call", {"name": name, "arguments": args}, timeout, name)["result"]
            # Every tool returns structuredContent; the text block is a
            # fallback for clients that do not read it.
            if "structuredContent" in result:
                return result["structuredContent"]
            text = result["content"][0]["text"]
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                return {"_message": text}
        except Exception as e:
            if attempt == attempts - 1:
                raise RuntimeError(f"tool {name} failed: {e}")
            print(f"    retry {attempt + 1}: {e}", flush=True)
            time.sleep(2)


SCRIPT = {
    "create-composition": "createComposition",
    "create-text-layer": "createTextLayer",
    "create-shape-layer": "createShapeLayer",
    "set-layer-expression": "setLayerExpression",
    "add-image-layer": "addImageLayer",
    "apply-effect": "applyEffect",
}


class Builder:
    BATCH_MAX = 60

    def __init__(self, theme_name):
        self.theme_name = theme_name
        self.T = THEMES[theme_name]
        self.comp = f"Prompt to Production Hero - {theme_name}"
        self.queue = []
        self.count = 0
        self.comp_index = None

    # --- plumbing ---------------------------------------------------------
    def flush(self, label="batch"):
        if not self.queue:
            return
        commands = [{"command": SCRIPT[n], "args": a} for _, n, a in self.queue]
        r = tool("run-batch", {"commands": commands, "undoGroup": f"{self.comp}: {label}"})
        if r.get("_message"):
            print(f"BRIDGE PROBLEM: {r['_message'][:220]}", flush=True)
            sys.exit(1)
        if r.get("status") != "success":
            for e in r.get("results", []):
                if e.get("status") == "error":
                    lbl = self.queue[e["i"]][0] if e["i"] < len(self.queue) else "?"
                    print(f"FAILED [{lbl}] {e.get('command')}: {e.get('message')}", flush=True)
            sys.exit(1)
        self.count += len(commands)
        print(f"   {label}: {len(commands)} commands ({self.count} total)", flush=True)
        self.queue = []

    def step(self, what, name, args):
        self.queue.append((what, name, args))
        if len(self.queue) >= self.BATCH_MAX:
            self.flush("chunk")

    def step_now(self, what, name, args):
        self.count += 1
        print(f"[{self.count}] {what}", flush=True)
        r = tool(name, args)
        if r.get("status") == "error" or r.get("success") is False:
            print(f"FAILED: {json.dumps(r)[:300]}", flush=True)
            sys.exit(1)
        if any(k in r.get("_message", "") for k in ("no result arrived", "never connected", "has not polled")):
            print(f"BRIDGE PROBLEM: {r['_message'][:220]}", flush=True)
            sys.exit(1)
        return r

    def expr(self, prop, expression):
        self.step(f"  expr {prop}", "set-layer-expression",
                  {"compName": self.comp, "layerIndex": 1,
                   "propertyName": prop, "expressionString": expression})

    # --- primitives -------------------------------------------------------
    def rect(self, name, cx, cy, w, h, t, fill=None, radius=0, stroke=None, stroke_w=0,
             dash=None, fill_none=False, fill_opacity=None, stroke_opacity=None,
             motion="pop", dy=12, alpha=100):
        args = {"compName": self.comp, "shapeType": "rectangle", "name": name,
                "position": [cx, cy], "size": [w, h], "fillColor": fill or N0,
                "roundness": radius, "strokeColor": stroke or N300, "strokeWidth": stroke_w,
                "startTime": t, "duration": DUR - t}
        if dash:
            args["dash"] = dash
        if fill_none:
            args["fillNone"] = True
        if fill_opacity is not None:
            args["fillOpacity"] = fill_opacity
        if stroke_opacity is not None:
            args["strokeOpacity"] = stroke_opacity
        self.step(f"rect {name} @ {t}", "create-shape-layer", args)
        if motion == "pop":
            self.expr("Opacity", pop_opacity(alpha))
            self.expr("Scale", POP_SCALE)
        elif motion == "draw":
            self.expr("Opacity", enter_opacity(alpha, 0.3))
            self.expr("Scale", DRAW_SCALE)
        else:
            self.expr("Opacity", enter_opacity(alpha))
            self.expr("Position", enter_position(cx, cy, dy))

    def ellipse(self, name, cx, cy, d, t, fill, alpha=100):
        self.step(f"dot {name} @ {t}", "create-shape-layer", {
            "compName": self.comp, "shapeType": "ellipse", "name": name,
            "position": [cx, cy], "size": [d, d], "fillColor": fill,
            "strokeWidth": 0, "startTime": t, "duration": DUR - t})
        self.expr("Opacity", pop_opacity(alpha))

    def star(self, name, cx, cy, d, t, fill, points=4, alpha=100):
        self.step(f"star {name} @ {t}", "create-shape-layer", {
            "compName": self.comp, "shapeType": "star", "name": name,
            "position": [cx, cy], "size": [d, d], "points": points,
            "fillColor": fill, "strokeWidth": 0, "startTime": t, "duration": DUR - t})
        self.expr("Opacity", pop_opacity(alpha))

    def text(self, content, x, y, size, color, t, align="left", font=None, dy=12, alpha=100):
        label = content.split("\n")[0][:28]
        self.step(f"text '{label}' @ {t}", "create-text-layer", {
            "compName": self.comp, "text": content, "position": [x, y], "fontSize": size,
            "color": color, "fontFamily": font or FONT, "alignment": align,
            "startTime": t, "duration": DUR - t})
        self.expr("Opacity", enter_opacity(alpha))
        self.expr("Position", enter_position(x, y, dy))

    def image(self, name, path, cx, cy, height, t, alpha=100, tint=None, scale=None):
        args = {"compName": self.comp, "path": path, "name": name,
                "position": [cx, cy], "startTime": t, "duration": DUR - t}
        if scale is not None:
            args["scale"] = [scale, scale]
        else:
            args["height"] = height
        self.step(f"logo {name} @ {t}", "add-image-layer", args)
        if tint:
            # Fill preserves the alpha shape and forces a single colour, so a
            # black mark stays visible on a dark background.
            self.step(f"  tint {name}", "apply-effect", {
                "compName": self.comp, "compIndex": self.comp_index, "layerIndex": 1,
                "effectMatchName": "ADBE Fill",
                "effectSettings": {"Color": tint + [1.0]}})
        self.expr("Opacity", enter_opacity(alpha))
        self.expr("Position", enter_position(cx, cy, 10))

    def grid_glyph(self, cx, cy, t, color):
        o = px(4)
        for i, (dx, dy) in enumerate([(-o, -o), (o, -o), (-o, o), (o, o)]):
            self.step(f"glyph-grid-{i}", "create-shape-layer", {
                "compName": self.comp, "shapeType": "rectangle", "name": f"glyph-grid-{i}",
                "position": [cx + dx, cy + dy], "size": [px(6), px(6)],
                "fillColor": color, "roundness": px(1.5), "strokeWidth": 0,
                "startTime": t, "duration": DUR - t})
            self.expr("Opacity", pop_opacity())

    def plug_glyph(self, cx, cy, t, color):
        self.step("glyph-plug-body", "create-shape-layer", {
            "compName": self.comp, "shapeType": "rectangle", "name": "glyph-plug-body",
            "position": [cx, cy + px(2)], "size": [px(13), px(10)],
            "fillColor": color, "roundness": px(3), "strokeWidth": 0,
            "startTime": t, "duration": DUR - t})
        self.expr("Opacity", pop_opacity())
        for i, dx in enumerate((-px(3.5), px(3.5))):
            self.step(f"glyph-plug-prong-{i}", "create-shape-layer", {
                "compName": self.comp, "shapeType": "rectangle", "name": f"glyph-plug-prong-{i}",
                "position": [cx + dx, cy - px(6)], "size": [px(2.5), px(6)],
                "fillColor": color, "roundness": px(1), "strokeWidth": 0,
                "startTime": t, "duration": DUR - t})
            self.expr("Opacity", pop_opacity())

    # --- the scene --------------------------------------------------------
    def build(self):
        T = self.T
        print(f"\n=== {self.comp} ===", flush=True)
        tool("delete-composition", {"compName": self.comp})
        self.step_now("create composition", "create-composition", {
            "name": self.comp, "width": 1920, "height": 1080,
            "frameRate": 30, "duration": DUR,
            "backgroundColor": {"r": 0, "g": 0, "b": 0}})
        info = self.step_now("locate comp", "get-project-info", {})
        for i, item in enumerate(info.get("items", []), start=1):
            if item.get("name") == self.comp:
                self.comp_index = i
        if self.comp_index is None:
            print("comp not found")
            sys.exit(1)

        # Full-bleed backdrop first, so it lands at the bottom of the stack.
        # Named so it can be switched off to restore a transparent render.
        self.step("app-background", "create-shape-layer", {
            "compName": self.comp, "shapeType": "rectangle", "name": "app-background",
            "position": [960, 540], "size": [1920, 1080], "fillColor": T["canvas"],
            "strokeWidth": 0, "startTime": 0, "duration": DUR})

        CT = 96
        N = {"prompt": (48, CT + 78, 300, 190), "skills": (392, CT + 136, 176, 76),
             "mcp": (616, CT + 136, 200, 76), "desk": (876, CT + 65, 330, 216)}

        def box(k):
            x, y, w, h = N[k]
            return px(x + w / 2), px(y + h / 2), px(w), px(h)

        def left(k):
            return px(N[k][0])

        def top(k):
            return px(N[k][1])

        CHIP_W, CHIP_H = px(181), px(72)
        CHIP_PITCH = px(208)
        CHIP_CX0 = px(22 + 196 / 2)
        CHIP_CY = px(CT + 434 + 36)
        ARROW_CX = [CHIP_CX0 + CHIP_PITCH * i + CHIP_PITCH / 2 for i in range(5)]
        FLOW_CY = px(CT + 136 + 38)

        # 1. Prompt card
        cx, cy, w, h = box("prompt")
        self.rect("prompt-card", cx, cy, w, h, 0.0, fill=PURPLE, radius=R_PROMPT)
        self.rect("prompt-icon", left("prompt") + px(27), top("prompt") + px(27),
                  px(26), px(26), 0.15, fill=PURPLE600, radius=px(7))
        self.star("glyph-spark", left("prompt") + px(27), top("prompt") + px(27), px(15), 0.18, N0)
        self.text("Prompt", left("prompt") + px(52), top("prompt") + px(33), px(15), N0, 0.2, font=FONT_BOLD)
        self.rect("prompt-llm-pill", left("prompt") + w - px(38), top("prompt") + px(27),
                  px(48), px(22), 0.25, fill=PURPLE600, radius=px(5))
        self.text("LLM", left("prompt") + w - px(38), top("prompt") + px(32), px(11), N0, 0.3,
                  align="center", font=FONT_SEMI)
        self.text("Build an image-resize service\nwith a beautiful interface and\nrun it on Cosmonic Desktop.",
                  left("prompt") + px(16), top("prompt") + px(78), px(14.5), N0, 0.35)
        self.ellipse("prompt-dot", left("prompt") + px(20), top("prompt") + px(168), px(7), 0.5, N0)
        self.text("routing to skills", left("prompt") + px(32), top("prompt") + px(172), px(12),
                  PURPLE_SOFT, 0.5)

        # 2. Prompt -> Skills
        x1, x2 = left("prompt") + w, left("skills")
        self.rect("link-prompt-skills", (x1 + x2) / 2, FLOW_CY, x2 - x1, px(2), 1.2,
                  fill=T["accent"], motion="draw")
        self.ellipse("link-1-cap", x1, FLOW_CY, px(9), 1.2, T["accent"])
        scx, scy, sw, sh = box("skills")
        self.rect("skills-card", scx, scy, sw, sh, 1.4, fill=T["node_bg"], radius=R_NODE,
                  stroke=T["node_stroke"], stroke_w=px(1.5), stroke_opacity=100)
        self.rect("skills-icon", left("skills") + px(29), scy, px(30), px(30), 1.5,
                  fill=T["skills_tint"], radius=R_ICON, fill_opacity=100)
        self.grid_glyph(left("skills") + px(29), scy, 1.52, T["skills_glyph"])
        self.text("Skills", left("skills") + px(55), scy - px(6), px(14), T["title"], 1.55,
                  font=FONT_BOLD)
        self.text("4 loaded", left("skills") + px(55), scy + px(14), px(11), T["sub"], 1.55,
                  font=MONO)

        # 3. Artifact rail + dotted boxes, one per second, arrows between
        rail_top, rail_y = top("skills") + sh, px(CT + 410)
        chip_top = CHIP_CY - CHIP_H / 2
        LW = px(1.5)
        self.rect("rail-drop", scx, (rail_top + rail_y) / 2, LW, rail_y - rail_top, 2.2,
                  fill=T["rail"], motion="enter", dy=0)
        self.rect("rail-run", (CHIP_CX0 + scx) / 2, rail_y, scx - CHIP_CX0 + LW, LW, 2.35,
                  fill=T["rail"], motion="enter", dy=0)
        self.rect("rail-stub", CHIP_CX0, (rail_y + chip_top) / 2, LW, chip_top - rail_y, 2.5,
                  fill=T["rail"], motion="enter", dy=0)
        self.text("GENERATED FOR YOU", px(22), px(486), px(12.5), T["label"], 2.3,
                  font=FONT_BOLD)

        ARTIFACTS = [("git repo", "main - 6 commits"), ("SBOM", "spdx-2.3.json"),
                     ("packages", "wasi:http 0.2.3"), ("OCI artifact", "ghcr.io/acme"),
                     ("CI pipeline", "build - test"), ("signature", "cosign attested")]
        for i, (title, sub) in enumerate(ARTIFACTS):
            t0 = 2.6 + i * 1.0
            ccx = CHIP_CX0 + CHIP_PITCH * i
            cleft = ccx - CHIP_W / 2
            self.rect(f"chip-{title}", ccx, CHIP_CY, CHIP_W, CHIP_H, t0, fill=T["chip_fill"],
                      radius=R_CHIP, stroke=T["chip_stroke"],
                      stroke_w=px(1.5), dash=[px(3), px(5)], motion="enter", dy=10)
            self.rect(f"chip-icon-{title}", cleft + px(28), CHIP_CY, px(32), px(32), t0 + 0.05,
                      fill_none=True, radius=R_ICON, stroke=T["chip_stroke"], stroke_w=px(1.5),
                      dash=[px(3), px(5)],
                      motion="enter", dy=10)
            self.text(title, cleft + px(52), CHIP_CY - px(7), px(14.5), T["title"], t0 + 0.1,
                      font=FONT_BOLD, dy=10)
            self.text(sub, cleft + px(52), CHIP_CY + px(13), px(11.5), T["sub"], t0 + 0.1,
                      font=MONO, dy=10)
            if i < 5:
                self.text("→", ARROW_CX[i], CHIP_CY + px(5), px(22), T["arrow"], t0 + 0.6,
                          align="center", dy=0)

        # 4. Skills -> MCP
        x1, x2 = left("skills") + sw, left("mcp")
        self.rect("link-skills-mcp", (x1 + x2) / 2, FLOW_CY, x2 - x1, px(2), 8.6,
                  fill=T["accent"], motion="draw")
        self.ellipse("link-2-cap", x1, FLOW_CY, px(9), 8.6, T["accent"])
        mcx, mcy, mw, mh = box("mcp")
        self.rect("mcp-card", mcx, mcy, mw, mh, 8.8, fill=T["node_bg"], radius=R_NODE,
                  stroke=T["node_stroke"], stroke_w=px(1.5), stroke_opacity=100)
        self.rect("mcp-icon", left("mcp") + px(29), mcy, px(30), px(30), 8.9,
                  fill=T["mcp_tint"], radius=R_ICON, fill_opacity=100)
        self.plug_glyph(left("mcp") + px(29), mcy, 8.92, T["mcp_glyph"])
        self.text("MCP", left("mcp") + px(55), mcy - px(6), px(14), T["title"], 8.95,
                  font=FONT_BOLD)
        self.text("cosmonic-desktop", left("mcp") + px(55), mcy + px(14), px(11), T["sub"], 8.95,
                  font=MONO)

        # 5. MCP -> Cosmonic Desktop
        x1, x2 = left("mcp") + mw, left("desk")
        self.rect("link-mcp-desk", (x1 + x2) / 2, FLOW_CY, x2 - x1, px(2), 9.8,
                  fill=T["accent"], motion="draw")
        self.ellipse("link-3-cap", x1, FLOW_CY, px(9), 9.8, T["accent"])
        dcx, dcy, dw, dh = box("desk")
        self.rect("desk-card", dcx, dcy, dw, dh, 10.0, fill=GUN, radius=R_DESK,
                  stroke=T["desk_stroke"] or N0, stroke_w=px(1) if T["desk_stroke"] else 0,
                  stroke_opacity=100)
        self.rect("desk-divider", dcx, top("desk") + px(44), dw, px(1), 10.1,
                  fill=N0, fill_opacity=9, motion="enter", dy=0)
        self.rect("desk-brand", left("desk") + px(22), top("desk") + px(22), px(17), px(17),
                  10.15, fill=PURPLE, radius=px(5))
        self.text("Cosmonic Desktop", left("desk") + px(34), top("desk") + px(27), px(14), N0,
                  10.2, font=FONT_BOLD)
        self.ellipse("desk-status-dot", left("desk") + dw - px(72), top("desk") + px(22), px(8),
                     10.3, GREEN)
        self.text("Running", left("desk") + dw - px(62), top("desk") + px(27), px(11), GREEN_FG,
                  10.3, font=MONO)

        TILES = [("http-gateway", "wasi:http", "Rs", YELLOW, 0, 0),
                 ("img-resize", "wasi:blobstore", "Rs", YELLOW, 1, 0),
                 ("kv-cache", "wasi:keyvalue", "Go", BLUE, 0, 1),
                 ("metrics-tap", "wasi:observe", "Ts", TS_PURPLE, 1, 1)]
        TILE_W, TILE_H = px(149), px(70)
        GRID_X, GRID_Y = left("desk") + px(12), top("desk") + px(56)
        for i, (name, world, lang, lang_color, col, row) in enumerate(TILES):
            t0 = 10.5 + i * 0.3
            tcx = GRID_X + TILE_W / 2 + col * (TILE_W + px(8))
            tcy = GRID_Y + TILE_H / 2 + row * (TILE_H + px(8))
            tleft = tcx - TILE_W / 2
            self.rect(f"tile-{name}", tcx, tcy, TILE_W, TILE_H, t0, fill=GUN_TILE, radius=R_TILE)
            self.rect(f"tile-lang-{name}", tleft + px(20), tcy - px(16), px(20), px(20),
                      t0 + 0.05, fill=lang_color, radius=px(5))
            self.text(lang, tleft + px(20), tcy - px(12), px(10), GUN, t0 + 0.05,
                      align="center", font=FONT_BOLD)
            self.text(name, tleft + px(34), tcy - px(11), px(11.5), N0, t0 + 0.1, font=MONO)
            self.ellipse(f"tile-dot-{name}", tleft + px(13), tcy + px(21), px(6), t0 + 0.15, GREEN)
            self.text(world, tleft + px(24), tcy + px(24), px(11), WHITE_74, t0 + 0.15, font=MONO)

        # 6. Hero text
        self.text("Prompt", 874, 186, px(29), T["hero_title"], 11.9, align="right",
                  font=FONT_BOLD, dy=14)
        self.text("→", 910, 186, px(29), T["accent"], 12.0, align="center", font=FONT_BOLD, dy=14)
        self.text("Production", 946, 186, px(29), T["hero_title"], 12.05, align="left",
                  font=FONT_BOLD, dy=14)
        self.text("with guardrails that you can trust", 960, 230, px(15.5), T["hero_sub"], 12.35,
                  align="center", font=FONT_MED, dy=10)
        self.text("...or just tell the harness of your choice to build it on Cosmonic Desktop",
                  960, 969, px(13), T["caption"], 12.7, align="center", dy=8,
                  alpha=100)

        # 7. Harness logos under the prompt card. They land once the card's
        #    white prompt text has settled, then the "and more" line beneath.
        #
        #    optical_centre is where the middle 80% of each mark's ink actually
        #    sits, as a fraction of its height. Centring the bounding boxes does
        #    NOT line them up: Gemini's sparkle pads the top of its box, pushing
        #    its wordmark's optical centre to 0.655 against ChatGPT's 0.502.
        pcx, _, _, _ = box("prompt")
        card_bottom = top("prompt") + px(190)
        LOGO_H = px(15)
        #         name        file                              src_w src_h  optical  scale%
        logos = [("claude",  f"{LOGO_DIR}/claude_ai-trim.png",    960,  207,  0.536,  None),
                 ("gemini",  f"{LOGO_DIR}/google_gemini-trim.png", 960, 352,  0.655,  8.0),
                 ("chatgpt", f"{LOGO_DIR}/chatgpt-trim.png",       883, 204,  0.502,  None)]
        gap = px(22)
        sizes = []
        for _, _, sw, sh, _, sc in logos:
            if sc is None:
                sizes.append((LOGO_H * (sw / sh), LOGO_H))
            else:
                sizes.append((sw * sc / 100.0, sh * sc / 100.0))
        total = sum(w for w, _ in sizes) + gap * (len(logos) - 1)
        logo_cy = card_bottom + px(38)
        x = pcx - total / 2
        for i, ((name, path, sw, sh, optical, sc), (rw, rh)) in enumerate(zip(logos, sizes)):
            self.image(name, path, x + rw / 2, logo_cy - (optical - 0.5) * rh,
                       LOGO_H, 1.0 + i * 0.18, alpha=100, tint=T["logo_tint"], scale=sc)
            x += rw + gap

        self.text("OpenShell, Hermes Agent,\nOpenClaw, and more..",
                  pcx, logo_cy + px(30), px(13.5), T["harness_text"], 1.6,
                  align="center", font=FONT_MED, dy=10)


        self.flush("final")
        print(f"Done: {self.count} commands.", flush=True)


which = (sys.argv[1] if len(sys.argv) > 1 else "both").lower()
targets = ["Day", "Night"] if which == "both" else [which.capitalize()]
for name in targets:
    Builder(name).build()
