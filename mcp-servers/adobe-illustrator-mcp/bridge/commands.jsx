// commands.jsx — Illustrator MCP bridge command library.
//
// Shared verbatim by both bridge vehicles: the CEP panel (loaded via the
// manifest's ScriptPath) and the pump script (concatenated into
// /bridge/pump.jsx by the server). Every command takes a plain-object `args`
// and returns a JSON string with at least {status: "success"|"error"}.
//
// Conventions (mirrored by the server's tool schemas):
// - Coordinates arrive as points from the ACTIVE ARTBOARD's top-left corner
//   with y increasing DOWNWARD; helpers below convert to Illustrator's y-up
//   document space.
// - Colors arrive as CSS hex strings ("#ff8800") or "none".

// --- JSON polyfill (ExtendScript is ES3; some hosts lack a JSON object) ----
if (typeof JSON === "undefined") { JSON = {}; }
if (typeof JSON.stringify !== "function") {
    JSON.stringify = function (value) {
        function esc(s) {
            return '"' + String(s)
                .replace(/\\/g, "\\\\").replace(/"/g, '\\"')
                .replace(/\n/g, "\\n").replace(/\r/g, "\\r").replace(/\t/g, "\\t") + '"';
        }
        function str(v) {
            if (v === null || v === undefined) { return "null"; }
            var t = typeof v;
            if (t === "number") { return isFinite(v) ? String(v) : "null"; }
            if (t === "boolean") { return String(v); }
            if (t === "string") { return esc(v); }
            if (v instanceof Array) {
                var parts = [];
                for (var i = 0; i < v.length; i++) { parts.push(str(v[i])); }
                return "[" + parts.join(",") + "]";
            }
            if (t === "object") {
                var kv = [];
                for (var k in v) {
                    if (v.hasOwnProperty(k) && typeof v[k] !== "function") {
                        kv.push(esc(k) + ":" + str(v[k]));
                    }
                }
                return "{" + kv.join(",") + "}";
            }
            return "null";
        }
        return str(value);
    };
}
if (typeof JSON.parse !== "function") {
    JSON.parse = function (text) {
        // json2-style guard, then eval — the input comes from this server.
        if (/^[\],:{}\s]*$/.test(String(text)
            .replace(/\\(?:["\\\/bfnrt]|u[0-9a-fA-F]{4})/g, "@")
            .replace(/"[^"\\\n\r]*"|true|false|null|-?\d+(?:\.\d*)?(?:[eE][+\-]?\d+)?/g, "]")
            .replace(/(?:^|:|,)(?:\s*\[)+/g, ""))) {
            return eval("(" + text + ")");
        }
        throw new Error("JSON.parse: invalid JSON");
    };
}

// --- shared helpers --------------------------------------------------------

function activeDoc() {
    if (app.documents.length === 0) {
        throw new Error("no document is open — create one with newDocument first");
    }
    return app.activeDocument;
}

// Top-left of the active artboard in Illustrator's y-up document space.
function abOrigin(doc) {
    var ab = doc.artboards[doc.artboards.getActiveArtboardIndex()];
    var r = ab.artboardRect; // [left, top, right, bottom], top > bottom
    return { left: r[0], top: r[1] };
}

function aiX(doc, x) { return abOrigin(doc).left + Number(x || 0); }
function aiY(doc, y) { return abOrigin(doc).top - Number(y || 0); }
// Back-conversion for reporting item positions in screen convention.
function screenX(doc, left) { return left - abOrigin(doc).left; }
function screenY(doc, top) { return abOrigin(doc).top - top; }

function hexToColor(hex) {
    var h = String(hex).replace(/^\s*#?/, "").replace(/\s*$/, "");
    if (h.length === 3) {
        h = h.charAt(0) + h.charAt(0) + h.charAt(1) + h.charAt(1) + h.charAt(2) + h.charAt(2);
    }
    if (!/^[0-9a-fA-F]{6}$/.test(h)) { throw new Error("not a hex color: " + hex); }
    var c = new RGBColor();
    c.red = parseInt(h.substring(0, 2), 16);
    c.green = parseInt(h.substring(2, 4), 16);
    c.blue = parseInt(h.substring(4, 6), 16);
    return c;
}

function colorToHex(c) {
    try {
        if (c && c.typename === "RGBColor") {
            function b(v) { var s = Math.round(v).toString(16); return s.length === 1 ? "0" + s : s; }
            return "#" + b(c.red) + b(c.green) + b(c.blue);
        }
        if (c && c.typename === "GrayColor") {
            var v = Math.round(255 * (1 - c.gray / 100));
            function g(x) { var s = x.toString(16); return s.length === 1 ? "0" + s : s; }
            return "#" + g(v) + g(v) + g(v);
        }
        if (c && c.typename === "CMYKColor") {
            function ch(x) { var s = Math.round(x).toString(16); return s.length === 1 ? "0" + s : s; }
            var r = Math.round(255 * (1 - c.cyan / 100) * (1 - c.black / 100));
            var gg = Math.round(255 * (1 - c.magenta / 100) * (1 - c.black / 100));
            var bb = Math.round(255 * (1 - c.yellow / 100) * (1 - c.black / 100));
            return "#" + ch(r) + ch(gg) + ch(bb);
        }
    } catch (e) {}
    return c ? c.typename : null;
}

function findLayer(doc, name) {
    for (var i = 0; i < doc.layers.length; i++) {
        if (doc.layers[i].name === name) { return doc.layers[i]; }
    }
    throw new Error("no layer named '" + name + "'");
}

// A layer by name, created if it does not exist yet.
function ensureLayer(doc, name) {
    for (var i = 0; i < doc.layers.length; i++) {
        if (doc.layers[i].name === name) { return doc.layers[i]; }
    }
    var l = doc.layers.add();
    l.name = name;
    return l;
}

// Move a freshly created item onto args.layer if given. The layer is created
// when absent (ensure semantics), so a tool never fails just because the
// caller named a layer that has not been added yet. Call this AFTER the item
// is fully sized and positioned, so a failure can never leave a half-built
// item behind.
function placeOnLayer(doc, item, args) {
    if (args && args.layer) {
        item.move(ensureLayer(doc, args.layer), ElementPlacement.PLACEATBEGINNING);
    }
}

// Apply fill/stroke/opacity/name to a new path item. `defFill` is the fill
// used when args.fill is absent (hex string or "none").
function styleItem(item, args, defFill) {
    var fill = (args.fill === undefined || args.fill === null) ? defFill : args.fill;
    if (String(fill).toLowerCase() === "none") {
        item.filled = false;
    } else {
        item.filled = true;
        item.fillColor = hexToColor(fill);
    }
    var hasStroke = (args.stroke !== undefined && args.stroke !== null) ||
                    (args.strokeWidth !== undefined && args.strokeWidth !== null);
    if (hasStroke && String(args.stroke).toLowerCase() !== "none") {
        item.stroked = true;
        item.strokeColor = hexToColor(args.stroke === undefined || args.stroke === null ? "#000000" : args.stroke);
        item.strokeWidth = (args.strokeWidth === undefined || args.strokeWidth === null) ? 1 : Number(args.strokeWidth);
    } else {
        item.stroked = false;
    }
    if (args.opacity !== undefined && args.opacity !== null) {
        item.opacity = Number(args.opacity);
    }
    if (args.name) { item.name = String(args.name); }
}

function itemSummary(doc, item) {
    var b = item.geometricBounds; // [left, top, right, bottom]
    return {
        type: item.typename,
        name: item.name || "",
        x: Math.round(screenX(doc, b[0]) * 100) / 100,
        y: Math.round(screenY(doc, b[1]) * 100) / 100,
        width: Math.round((b[2] - b[0]) * 100) / 100,
        height: Math.round((b[1] - b[3]) * 100) / 100
    };
}

function ok(extra) {
    var out = { status: "success" };
    if (extra) { for (var k in extra) { if (extra.hasOwnProperty(k)) { out[k] = extra[k]; } } }
    return JSON.stringify(out);
}

function fail(message) {
    return JSON.stringify({ status: "error", message: String(message) });
}

// --- reading ---------------------------------------------------------------

function getDocumentInfo() {
    var doc = activeDoc();
    var layers = [];
    for (var i = 0; i < doc.layers.length; i++) {
        var l = doc.layers[i];
        layers.push({ name: l.name, visible: l.visible, locked: l.locked, items: l.pageItems.length });
    }
    var boards = [];
    var activeIdx = doc.artboards.getActiveArtboardIndex();
    for (var j = 0; j < doc.artboards.length; j++) {
        var r = doc.artboards[j].artboardRect;
        boards.push({
            index: j, name: doc.artboards[j].name, active: j === activeIdx,
            width: Math.round(r[2] - r[0]), height: Math.round(r[1] - r[3])
        });
    }
    var mode = "unknown";
    try { mode = doc.documentColorSpace === DocumentColorSpace.RGB ? "rgb" : "cmyk"; } catch (e) {}
    return ok({
        name: doc.name,
        path: doc.saved && doc.fullName ? doc.fullName.fsName : "(unsaved)",
        colorMode: mode,
        width: Math.round(doc.width),
        height: Math.round(doc.height),
        artboards: boards,
        layers: layers,
        counts: {
            pageItems: doc.pageItems.length,
            pathItems: doc.pathItems.length,
            textFrames: doc.textFrames.length,
            placedItems: doc.placedItems.length,
            rasterItems: doc.rasterItems.length,
            groupItems: doc.groupItems.length
        },
        modified: (function () { try { return doc.modified; } catch (e) { return null; } })()
    });
}

function listDocuments() {
    var docs = [];
    for (var i = 0; i < app.documents.length; i++) {
        var d = app.documents[i];
        docs.push({
            name: d.name,
            active: app.documents.length > 0 && d === app.activeDocument,
            width: Math.round(d.width),
            height: Math.round(d.height)
        });
    }
    return ok({ count: docs.length, documents: docs });
}

function listArtboards() {
    var doc = activeDoc();
    var o = abOrigin(doc);
    var activeIdx = doc.artboards.getActiveArtboardIndex();
    var result = [];
    for (var i = 0; i < doc.artboards.length; i++) {
        var r = doc.artboards[i].artboardRect;
        result.push({
            index: i,
            name: doc.artboards[i].name,
            active: i === activeIdx,
            x: Math.round(r[0] - o.left),
            y: Math.round(o.top - r[1]),
            width: Math.round(r[2] - r[0]),
            height: Math.round(r[1] - r[3])
        });
    }
    return ok({ count: result.length, artboards: result });
}

function listLayers() {
    var doc = activeDoc();
    var result = [];
    for (var i = 0; i < doc.layers.length; i++) {
        var l = doc.layers[i];
        result.push({
            name: l.name, visible: l.visible, locked: l.locked,
            items: l.pageItems.length, active: l === doc.activeLayer
        });
    }
    return ok({ count: result.length, layers: result });
}

function listPageItems(args) {
    var doc = activeDoc();
    var detail = args.detail === true;
    var limit = args.limit ? Math.max(1, Number(args.limit)) : 200;
    var items = args.layer ? findLayer(doc, args.layer).pageItems : doc.pageItems;
    var result = [];
    var total = items.length;
    for (var i = 0; i < items.length && result.length < limit; i++) {
        var it = items[i];
        var entry = itemSummary(doc, it);
        entry.index = i;
        try { entry.layer = it.layer.name; } catch (e) {}
        entry.hidden = it.hidden;
        entry.locked = it.locked;
        if (detail) {
            entry.opacity = it.opacity;
            if (it.typename === "PathItem") {
                entry.filled = it.filled;
                if (it.filled) { entry.fill = colorToHex(it.fillColor); }
                entry.stroked = it.stroked;
                if (it.stroked) {
                    entry.stroke = colorToHex(it.strokeColor);
                    entry.strokeWidth = it.strokeWidth;
                }
                entry.closed = it.closed;
                entry.pathPoints = it.pathPoints.length;
            } else if (it.typename === "TextFrame") {
                entry.contents = String(it.contents).substring(0, 120);
                try {
                    var attr = it.textRange.characterAttributes;
                    entry.fontSize = attr.size;
                    entry.font = attr.textFont.name;
                    entry.color = colorToHex(attr.fillColor);
                } catch (e2) {}
            } else if (it.typename === "PlacedItem") {
                try { entry.file = it.file ? it.file.fsName : null; } catch (e3) {}
            }
        }
        result.push(entry);
    }
    return ok({ total: total, shown: result.length, items: result });
}

function listTextFrames() {
    var doc = activeDoc();
    var result = [];
    for (var i = 0; i < doc.textFrames.length; i++) {
        var tf = doc.textFrames[i];
        var entry = itemSummary(doc, tf);
        entry.index = i;
        entry.contents = String(tf.contents).substring(0, 120);
        try {
            var attr = tf.textRange.characterAttributes;
            entry.fontSize = attr.size;
            entry.font = attr.textFont.name;
            entry.color = colorToHex(attr.fillColor);
        } catch (e) {}
        result.push(entry);
    }
    return ok({ count: result.length, textFrames: result });
}

function listSwatches() {
    var doc = activeDoc();
    var result = [];
    for (var i = 0; i < doc.swatches.length; i++) {
        var sw = doc.swatches[i];
        var entry = { name: sw.name };
        try {
            entry.type = sw.color.typename;
            entry.hex = colorToHex(sw.color);
        } catch (e) { entry.type = "unknown"; }
        result.push(entry);
    }
    return ok({ count: result.length, swatches: result });
}

function listFonts(args) {
    var contains = args.contains ? String(args.contains).toLowerCase() : null;
    var limit = args.limit ? Math.max(1, Number(args.limit)) : 50;
    var result = [];
    for (var i = 0; i < app.textFonts.length && result.length < limit; i++) {
        var f = app.textFonts[i];
        if (contains && String(f.name).toLowerCase().indexOf(contains) < 0 &&
            String(f.family).toLowerCase().indexOf(contains) < 0) { continue; }
        result.push({ name: f.name, family: f.family, style: f.style });
    }
    return ok({ total: app.textFonts.length, shown: result.length, fonts: result });
}

function getSelection() {
    var doc = activeDoc();
    var sel = doc.selection;
    if (!sel || sel.length === undefined) { sel = sel ? [sel] : []; }
    var types = {};
    var names = [];
    for (var i = 0; i < sel.length; i++) {
        var t = sel[i].typename;
        types[t] = (types[t] || 0) + 1;
        if (i < 10) { names.push(sel[i].name || "(unnamed " + t + ")"); }
    }
    return ok({ count: sel.length, types: types, names: names });
}

// --- documents -------------------------------------------------------------

function newDocument(args) {
    var w = args.width ? Number(args.width) : 1920;
    var h = args.height ? Number(args.height) : 1080;
    var space = (args.colorMode === "cmyk") ? DocumentColorSpace.CMYK : DocumentColorSpace.RGB;
    var doc = app.documents.add(space, w, h);
    return ok({
        name: doc.name,
        width: Math.round(doc.width),
        height: Math.round(doc.height),
        colorMode: args.colorMode === "cmyk" ? "cmyk" : "rgb"
    });
}

function openDocument(args) {
    if (!args.path) { throw new Error("'path' is required"); }
    var f = new File(args.path);
    if (!f.exists) { throw new Error("file not found: " + args.path); }
    var doc = app.open(f);
    return ok({ name: doc.name, path: doc.fullName.fsName });
}

function saveDocument(args) {
    var doc = activeDoc();
    if (args && args.path) {
        var f = new File(args.path);
        if (f.exists && args.overwrite !== true) {
            throw new Error("refusing to overwrite existing file (pass overwrite: true): " + f.fsName);
        }
        doc.saveAs(f, new IllustratorSaveOptions());
        return ok({ path: doc.fullName.fsName });
    }
    try {
        doc.save();
    } catch (e) {
        throw new Error("save failed (never-saved documents need a 'path'): " + e);
    }
    return ok({ path: doc.fullName ? doc.fullName.fsName : doc.name });
}

function closeDocument(args) {
    var doc = activeDoc();
    var name = doc.name;
    doc.close(args && args.save === true ? SaveOptions.SAVECHANGES : SaveOptions.DONOTSAVECHANGES);
    return ok({ closed: name, saved: !!(args && args.save === true) });
}

function exportDocument(args) {
    var doc = activeDoc();
    if (!args.path) { throw new Error("'path' is required"); }
    var f = new File(args.path);
    if (f.exists && args.overwrite !== true) {
        throw new Error("refusing to overwrite existing file (pass overwrite: true): " + f.fsName);
    }
    var lower = String(args.path).toLowerCase();
    var scale = args.scale ? Number(args.scale) : 100;
    if (lower.match(/\.png$/)) {
        var po = new ExportOptionsPNG24();
        po.antiAliasing = true;
        po.transparency = true;
        po.artBoardClipping = args.artboardClipping !== false;
        po.horizontalScale = scale;
        po.verticalScale = scale;
        doc.exportFile(f, ExportType.PNG24, po);
    } else if (lower.match(/\.jpe?g$/)) {
        var jo = new ExportOptionsJPEG();
        jo.antiAliasing = true;
        jo.artBoardClipping = args.artboardClipping !== false;
        jo.qualitySetting = args.jpegQuality ? Number(args.jpegQuality) : 85;
        jo.horizontalScale = scale;
        jo.verticalScale = scale;
        doc.exportFile(f, ExportType.JPEG, jo);
    } else if (lower.match(/\.svg$/)) {
        var so = new ExportOptionsSVG();
        so.embedRasterImages = true;
        so.cssProperties = SVGCSSPropertyLocation.PRESENTATIONATTRIBUTES;
        doc.exportFile(f, ExportType.SVG, so);
    } else if (lower.match(/\.pdf$/)) {
        var opts = new PDFSaveOptions();
        try { opts.pDFPreset = "[Smallest File Size]"; } catch (e) {}
        doc.saveAs(f, opts);
    } else {
        throw new Error("unsupported export format; use .svg .png .jpg or .pdf");
    }
    return ok({ path: f.fsName });
}

function placeImage(args) {
    var doc = activeDoc();
    if (!args.path) { throw new Error("'path' is required"); }
    var f = new File(args.path);
    if (!f.exists) { throw new Error("file not found: " + args.path); }
    var pi = doc.placedItems.add();
    pi.file = f;
    if (args.name) { pi.name = String(args.name); }
    // Size first (scaling moves the corner), then position.
    var w = args.width !== undefined && args.width !== null ? Number(args.width) : null;
    var h = args.height !== undefined && args.height !== null ? Number(args.height) : null;
    if (w !== null && h === null) { h = pi.height * (w / pi.width); }
    if (h !== null && w === null) { w = pi.width * (h / pi.height); }
    if (w !== null && h !== null && pi.width > 0 && pi.height > 0) {
        pi.width = w;
        pi.height = h;
    }
    pi.left = aiX(doc, args.x || 0);
    pi.top = aiY(doc, args.y || 0);
    // Move onto the requested layer only once the item is fully built, so a
    // layer problem can never leave an unsized item behind.
    placeOnLayer(doc, pi, args);
    var summary = itemSummary(doc, pi);
    if (args.embed === true) {
        pi.embed(); // pi is invalid afterwards
        summary.embedded = true;
    }
    return ok({ placed: summary });
}

// --- artboards -------------------------------------------------------------

function addArtboard(args) {
    var doc = activeDoc();
    var o = abOrigin(doc);
    var left = o.left + Number(args.x || 0);
    var top = o.top - Number(args.y || 0);
    var rect = [left, top, left + Number(args.width), top - Number(args.height)];
    var ab = doc.artboards.add(rect);
    if (args.name) { ab.name = String(args.name); }
    return ok({
        index: doc.artboards.length - 1,
        name: ab.name,
        width: Number(args.width),
        height: Number(args.height)
    });
}

function setActiveArtboard(args) {
    var doc = activeDoc();
    var idx = Number(args.index);
    if (!(idx >= 0 && idx < doc.artboards.length)) {
        throw new Error("artboard index out of range: " + args.index);
    }
    doc.artboards.setActiveArtboardIndex(idx);
    return ok({ activeIndex: idx, name: doc.artboards[idx].name });
}

// --- layers ----------------------------------------------------------------

function addLayer(args) {
    var doc = activeDoc();
    var layer = doc.layers.add();
    layer.name = String(args.name || "Layer");
    doc.activeLayer = layer;
    return ok({ layer: layer.name, active: true });
}

function setLayer(args) {
    var doc = activeDoc();
    var layer = findLayer(doc, args.name);
    if (args.visible !== undefined && args.visible !== null) { layer.visible = args.visible === true; }
    if (args.locked !== undefined && args.locked !== null) { layer.locked = args.locked === true; }
    if (args.newName) { layer.name = String(args.newName); }
    if (args.active === true) { doc.activeLayer = layer; }
    return ok({
        layer: layer.name, visible: layer.visible,
        locked: layer.locked, active: layer === doc.activeLayer
    });
}

function deleteLayer(args) {
    var doc = activeDoc();
    var layer = findLayer(doc, args.name);
    var name = layer.name;
    layer.locked = false;
    layer.remove();
    return ok({ deleted: name });
}

// --- drawing ---------------------------------------------------------------

function drawRectangle(args) {
    var doc = activeDoc();
    var top = aiY(doc, args.y);
    var left = aiX(doc, args.x);
    var item;
    if (args.cornerRadius && Number(args.cornerRadius) > 0) {
        item = doc.pathItems.roundedRectangle(
            top, left, Number(args.width), Number(args.height),
            Number(args.cornerRadius), Number(args.cornerRadius));
    } else {
        item = doc.pathItems.rectangle(top, left, Number(args.width), Number(args.height));
    }
    placeOnLayer(doc, item, args);
    styleItem(item, args, "#000000");
    return ok({ item: itemSummary(doc, item) });
}

function drawEllipse(args) {
    var doc = activeDoc();
    var item = doc.pathItems.ellipse(
        aiY(doc, args.y), aiX(doc, args.x), Number(args.width), Number(args.height));
    placeOnLayer(doc, item, args);
    styleItem(item, args, "#000000");
    return ok({ item: itemSummary(doc, item) });
}

function drawLine(args) {
    var doc = activeDoc();
    var item = doc.pathItems.add();
    item.setEntirePath([
        [aiX(doc, args.x1), aiY(doc, args.y1)],
        [aiX(doc, args.x2), aiY(doc, args.y2)]
    ]);
    placeOnLayer(doc, item, args);
    item.filled = false;
    item.stroked = true;
    item.strokeColor = hexToColor(args.stroke === undefined || args.stroke === null ? "#000000" : args.stroke);
    item.strokeWidth = (args.strokeWidth === undefined || args.strokeWidth === null) ? 1 : Number(args.strokeWidth);
    if (args.name) { item.name = String(args.name); }
    return ok({ item: itemSummary(doc, item) });
}

function drawPolygon(args) {
    var doc = activeDoc();
    var pts = args.points || [];
    if (pts.length < 2) { throw new Error("need at least 2 points"); }
    var path = [];
    for (var i = 0; i < pts.length; i++) {
        path.push([aiX(doc, pts[i][0]), aiY(doc, pts[i][1])]);
    }
    var item = doc.pathItems.add();
    item.setEntirePath(path);
    item.closed = args.closed !== false;
    placeOnLayer(doc, item, args);
    styleItem(item, args, item.closed ? "#000000" : "none");
    return ok({ item: itemSummary(doc, item), points: pts.length, closed: item.closed });
}

function drawStar(args) {
    var doc = activeDoc();
    var cx = aiX(doc, args.centerX);
    var cy = aiY(doc, args.centerY);
    var radius = Number(args.radius);
    var n = args.points ? Math.max(3, Number(args.points)) : 5;
    var item;
    if (args.polygon === true) {
        item = doc.pathItems.polygon(cx, cy, radius, n);
    } else {
        var inner = args.innerRadius ? Number(args.innerRadius) : radius / 2;
        item = doc.pathItems.star(cx, cy, radius, inner, n);
    }
    placeOnLayer(doc, item, args);
    styleItem(item, args, "#000000");
    return ok({ item: itemSummary(doc, item) });
}

function addText(args) {
    var doc = activeDoc();
    var frame;
    if (args.width && args.height) {
        var rect = doc.pathItems.rectangle(
            aiY(doc, args.y), aiX(doc, args.x), Number(args.width), Number(args.height));
        frame = doc.textFrames.areaText(rect);
    } else {
        frame = doc.textFrames.pointText([aiX(doc, args.x), aiY(doc, args.y)]);
    }
    placeOnLayer(doc, frame, args);
    frame.contents = String(args.content || "");
    var attr = frame.textRange.characterAttributes;
    attr.size = args.fontSize ? Number(args.fontSize) : 24;
    var fontFallback = null;
    if (args.font) {
        try { attr.textFont = app.textFonts.getByName(String(args.font)); }
        catch (e) { fontFallback = "font '" + args.font + "' not found; kept " + attr.textFont.name; }
    }
    attr.fillColor = hexToColor(args.color === undefined || args.color === null ? "#000000" : args.color);
    if (args.justification) {
        var j = String(args.justification).toLowerCase();
        frame.textRange.paragraphAttributes.justification =
            j === "center" ? Justification.CENTER : j === "right" ? Justification.RIGHT : Justification.LEFT;
    }
    if (args.opacity !== undefined && args.opacity !== null) { frame.opacity = Number(args.opacity); }
    if (args.name) { frame.name = String(args.name); }
    // Point text anchors at the baseline; re-pin the frame's visual top-left
    // to the requested position so both kinds land where asked.
    frame.top = aiY(doc, args.y);
    frame.left = aiX(doc, args.x);
    var out = { index: doc.textFrames.length - 1, item: itemSummary(doc, frame) };
    if (fontFallback) { out.warning = fontFallback; }
    return ok(out);
}

function setTextFrame(args) {
    var doc = activeDoc();
    var frame = null;
    if (args.index !== undefined && args.index !== null) {
        var idx = Number(args.index);
        if (!(idx >= 0 && idx < doc.textFrames.length)) {
            throw new Error("text frame index out of range: " + args.index);
        }
        frame = doc.textFrames[idx];
    } else if (args.name) {
        for (var i = 0; i < doc.textFrames.length; i++) {
            if (doc.textFrames[i].name === args.name) { frame = doc.textFrames[i]; break; }
        }
        if (!frame) { throw new Error("no text frame named '" + args.name + "'"); }
    } else {
        throw new Error("give 'index' or 'name'");
    }
    if (args.content !== undefined && args.content !== null) { frame.contents = String(args.content); }
    var attr = frame.textRange.characterAttributes;
    if (args.fontSize) { attr.size = Number(args.fontSize); }
    var fontFallback = null;
    if (args.font) {
        try { attr.textFont = app.textFonts.getByName(String(args.font)); }
        catch (e) { fontFallback = "font '" + args.font + "' not found; kept " + attr.textFont.name; }
    }
    if (args.color) { attr.fillColor = hexToColor(args.color); }
    if (args.x !== undefined && args.x !== null) { frame.left = aiX(doc, args.x); }
    if (args.y !== undefined && args.y !== null) { frame.top = aiY(doc, args.y); }
    var out = { item: itemSummary(doc, frame) };
    if (fontFallback) { out.warning = fontFallback; }
    return ok(out);
}

// --- selection and transforms ----------------------------------------------

function selectedItems(doc) {
    var sel = doc.selection;
    if (!sel) { return []; }
    if (sel.length === undefined) { return [sel]; }
    return sel;
}

function selectAll() {
    activeDoc();
    app.executeMenuCommand("selectall");
    return ok({ selected: selectedItems(activeDoc()).length });
}

function deselectAll() {
    var doc = activeDoc();
    doc.selection = null;
    return ok({});
}

function selectByName(args) {
    var doc = activeDoc();
    if (!args.name) { throw new Error("'name' is required"); }
    if (args.add !== true) { doc.selection = null; }
    var matched = 0;
    for (var i = 0; i < doc.pageItems.length; i++) {
        if (doc.pageItems[i].name === args.name) {
            doc.pageItems[i].selected = true;
            matched++;
        }
    }
    return ok({ matched: matched, name: args.name });
}

function moveSelection(args) {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    for (var i = 0; i < sel.length; i++) {
        sel[i].translate(Number(args.dx || 0), -Number(args.dy || 0));
    }
    return ok({ moved: sel.length });
}

function scaleSelection(args) {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    for (var i = 0; i < sel.length; i++) {
        sel[i].resize(Number(args.scaleX), Number(args.scaleY));
    }
    return ok({ scaled: sel.length });
}

function rotateSelection(args) {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    for (var i = 0; i < sel.length; i++) {
        sel[i].rotate(Number(args.angle));
    }
    return ok({ rotated: sel.length, angle: Number(args.angle) });
}

function duplicateSelection(args) {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    var dx = args.dx !== undefined && args.dx !== null ? Number(args.dx) : 20;
    var dy = args.dy !== undefined && args.dy !== null ? Number(args.dy) : 20;
    var copies = [];
    for (var i = 0; i < sel.length; i++) { copies.push(sel[i].duplicate()); }
    doc.selection = null;
    for (var j = 0; j < copies.length; j++) {
        copies[j].translate(dx, -dy);
        copies[j].selected = true;
    }
    return ok({ duplicated: copies.length, dx: dx, dy: dy });
}

function deleteSelection() {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    var count = sel.length;
    for (var i = sel.length - 1; i >= 0; i--) { sel[i].remove(); }
    return ok({ deleted: count });
}

function setFill(args) {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    var none = String(args.color).toLowerCase() === "none";
    var color = none ? null : hexToColor(args.color);
    for (var i = 0; i < sel.length; i++) {
        var it = sel[i];
        if (it.typename === "TextFrame") {
            if (!none) { it.textRange.characterAttributes.fillColor = color; }
            continue;
        }
        if (none) { it.filled = false; }
        else { it.filled = true; it.fillColor = color; }
    }
    return ok({ updated: sel.length, fill: args.color });
}

function setStroke(args) {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    var none = args.color !== undefined && args.color !== null &&
               String(args.color).toLowerCase() === "none";
    for (var i = 0; i < sel.length; i++) {
        var it = sel[i];
        if (it.typename === "TextFrame") { continue; }
        if (none) { it.stroked = false; continue; }
        if (args.color) { it.stroked = true; it.strokeColor = hexToColor(args.color); }
        if (args.width !== undefined && args.width !== null) {
            it.stroked = true;
            it.strokeWidth = Number(args.width);
        }
        if (args.dash) { it.strokeDashes = args.dash; }
    }
    return ok({ updated: sel.length });
}

function setOpacity(args) {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    for (var i = 0; i < sel.length; i++) { sel[i].opacity = Number(args.opacity); }
    return ok({ updated: sel.length, opacity: Number(args.opacity) });
}

function groupSelection() {
    activeDoc();
    app.executeMenuCommand("group");
    return ok({});
}

function ungroupSelection() {
    activeDoc();
    app.executeMenuCommand("ungroup");
    return ok({});
}

function bringToFront() {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    for (var i = 0; i < sel.length; i++) { sel[i].zOrder(ZOrderMethod.BRINGTOFRONT); }
    return ok({ updated: sel.length });
}

function sendToBack() {
    var doc = activeDoc();
    var sel = selectedItems(doc);
    for (var i = 0; i < sel.length; i++) { sel[i].zOrder(ZOrderMethod.SENDTOBACK); }
    return ok({ updated: sel.length });
}

// --- history ---------------------------------------------------------------

function undoCommand() {
    activeDoc();
    app.undo();
    return ok({});
}

function redoCommand() {
    activeDoc();
    app.redo();
    return ok({});
}

// --- escape hatch and batching ---------------------------------------------

function runRawJsx(args) {
    if (!args || !args.script) { throw new Error("'script' is required"); }
    var value = eval(String(args.script));
    var rendered;
    if (value === undefined) { rendered = "undefined"; }
    else if (value === null) { rendered = "null"; }
    else if (typeof value === "object") {
        try { rendered = JSON.stringify(value); } catch (e) { rendered = String(value); }
    } else { rendered = String(value); }
    return ok({ result: rendered });
}

// Run many commands in a single round trip.
//
// The bridge costs one ~2s poll cycle per command, so building anything of
// size one command at a time is dominated by round-trip latency. Batching
// collapses that to a single cycle. Results are summarised (status only, plus
// the message on failure) to keep the response small.
function runBatch(args) {
    var cmds = args.commands || [];
    var stopOnError = args.continueOnError !== true;
    var results = [];
    var failed = 0;
    for (var i = 0; i < cmds.length; i++) {
        var entry = cmds[i] || {};
        var raw = executeCommand(entry.command, entry.args || {});
        var parsed;
        try { parsed = JSON.parse(raw); } catch (e) { parsed = { status: "error", message: String(raw) }; }
        var okEntry = parsed.status !== "error" && parsed.success !== false;
        var summary = { i: i, command: entry.command, status: okEntry ? "ok" : "error" };
        if (!okEntry) {
            failed++;
            summary.message = parsed.message || "unknown error";
        }
        results.push(summary);
        if (!okEntry && stopOnError) { break; }
    }
    try { app.redraw(); } catch (e2) {}
    return JSON.stringify({
        status: failed > 0 ? "error" : "success",
        requested: cmds.length,
        completed: results.length,
        failed: failed,
        results: results
    });
}

// --- dispatch --------------------------------------------------------------

// Depth > 0 means we're inside runBatch: used by UIs to skip per-command
// refreshes.
var BATCH_DEPTH = 0;

function executeCommand(command, args) {
    var result = "";
    if (command === "runBatch") { BATCH_DEPTH++; }
    try {
        switch (command) {
            case "getDocumentInfo": result = getDocumentInfo(); break;
            case "listDocuments": result = listDocuments(); break;
            case "newDocument": result = newDocument(args); break;
            case "openDocument": result = openDocument(args); break;
            case "saveDocument": result = saveDocument(args); break;
            case "closeDocument": result = closeDocument(args); break;
            case "exportDocument": result = exportDocument(args); break;
            case "placeImage": result = placeImage(args); break;
            case "listArtboards": result = listArtboards(); break;
            case "addArtboard": result = addArtboard(args); break;
            case "setActiveArtboard": result = setActiveArtboard(args); break;
            case "addLayer": result = addLayer(args); break;
            case "listLayers": result = listLayers(); break;
            case "setLayer": result = setLayer(args); break;
            case "deleteLayer": result = deleteLayer(args); break;
            case "drawRectangle": result = drawRectangle(args); break;
            case "drawEllipse": result = drawEllipse(args); break;
            case "drawLine": result = drawLine(args); break;
            case "drawPolygon": result = drawPolygon(args); break;
            case "drawStar": result = drawStar(args); break;
            case "addText": result = addText(args); break;
            case "setTextFrame": result = setTextFrame(args); break;
            case "listTextFrames": result = listTextFrames(); break;
            case "listPageItems": result = listPageItems(args); break;
            case "selectAll": result = selectAll(); break;
            case "deselectAll": result = deselectAll(); break;
            case "selectByName": result = selectByName(args); break;
            case "getSelection": result = getSelection(); break;
            case "moveSelection": result = moveSelection(args); break;
            case "scaleSelection": result = scaleSelection(args); break;
            case "rotateSelection": result = rotateSelection(args); break;
            case "duplicateSelection": result = duplicateSelection(args); break;
            case "deleteSelection": result = deleteSelection(); break;
            case "setFill": result = setFill(args); break;
            case "setStroke": result = setStroke(args); break;
            case "setOpacity": result = setOpacity(args); break;
            case "groupSelection": result = groupSelection(); break;
            case "ungroupSelection": result = ungroupSelection(); break;
            case "bringToFront": result = bringToFront(); break;
            case "sendToBack": result = sendToBack(); break;
            case "listSwatches": result = listSwatches(); break;
            case "listFonts": result = listFonts(args); break;
            case "undo": result = undoCommand(); break;
            case "redo": result = redoCommand(); break;
            case "runBatch": result = runBatch(args); break;
            case "runJsx": result = runRawJsx(args); break;
            default:
                result = JSON.stringify({ status: "error", message: "Unknown command: " + command });
        }
        var resultString = (typeof result === "string") ? result : JSON.stringify(result);
        try {
            var resultObj = JSON.parse(resultString);
            resultObj._responseTimestamp = new Date().toString();
            resultObj._commandExecuted = command;
            resultString = JSON.stringify(resultObj);
        } catch (parseError) {
            // Not JSON; send as-is.
        }
        if (command === "runBatch") { BATCH_DEPTH--; }
        return resultString;
    } catch (error) {
        if (command === "runBatch") { BATCH_DEPTH--; }
        return JSON.stringify({
            status: "error",
            command: command,
            message: error.toString(),
            line: error.line
        });
    }
}

// Wire adapter for the CEP panel: evalScript can only pass strings, so the
// whole command object arrives URI-encoded JSON and leaves as the result
// string.
function ilstMcpExecuteWire(encoded) {
    try {
        var data = JSON.parse(decodeURIComponent(encoded));
        return executeCommand(data.command, data.args || {});
    } catch (e) {
        return JSON.stringify({ status: "error", message: "wire decode failed: " + e });
    }
}
