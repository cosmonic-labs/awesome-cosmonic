// mcp-bridge-auto.jsx
// Auto-running MCP Bridge panel for After Effects

// Remove #include directives as we define functions below
/*
#include "createComposition.jsx"
#include "createTextLayer.jsx"
#include "createShapeLayer.jsx"
#include "createSolidLayer.jsx"
#include "setLayerProperties.jsx"
*/

// --- Function Definitions ---

// --- createComposition (from createComposition.jsx) --- 
function createComposition(args) {
    try {
        var name = args.name || "New Composition";
        var width = parseInt(args.width) || 1920;
        var height = parseInt(args.height) || 1080;
        var pixelAspect = parseFloat(args.pixelAspect) || 1.0;
        var duration = parseFloat(args.duration) || 10.0;
        var frameRate = parseFloat(args.frameRate) || 30.0;
        var bgColor = args.backgroundColor ? [args.backgroundColor.r/255, args.backgroundColor.g/255, args.backgroundColor.b/255] : [0, 0, 0];
        var newComp = app.project.items.addComp(name, width, height, pixelAspect, duration, frameRate);
        if (args.backgroundColor) {
            newComp.bgColor = bgColor;
        }
        return JSON.stringify({
            status: "success", message: "Composition created successfully",
            composition: { name: newComp.name, id: newComp.id, width: newComp.width, height: newComp.height, pixelAspect: newComp.pixelAspect, duration: newComp.duration, frameRate: newComp.frameRate, bgColor: newComp.bgColor }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- createTextLayer (from createTextLayer.jsx) ---
function createTextLayer(args) {
    try {
        var compName = args.compName || "";
        var text = args.text || "Text Layer";
        var position = args.position || [960, 540]; 
        var fontSize = args.fontSize || 72;
        var color = args.color || [1, 1, 1]; 
        var startTime = args.startTime || 0;
        var duration = args.duration || 5; 
        var fontFamily = args.fontFamily || "Arial";
        var alignment = args.alignment || "center"; 
        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; } 
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }
        var textLayer = comp.layers.addText(text);
        var textProp = textLayer.property("ADBE Text Properties").property("ADBE Text Document");
        var textDocument = textProp.value;
        textDocument.fontSize = fontSize;
        textDocument.fillColor = color;
        textDocument.font = fontFamily;
        if (alignment === "left") { textDocument.justification = ParagraphJustification.LEFT_JUSTIFY; } 
        else if (alignment === "center") { textDocument.justification = ParagraphJustification.CENTER_JUSTIFY; } 
        else if (alignment === "right") { textDocument.justification = ParagraphJustification.RIGHT_JUSTIFY; }
        textProp.setValue(textDocument);
        textLayer.property("Position").setValue(position);
        textLayer.startTime = startTime;
        if (duration > 0) { textLayer.outPoint = startTime + duration; }
        return JSON.stringify({
            status: "success", message: "Text layer created successfully",
            layer: { name: textLayer.name, index: textLayer.index, type: "text", inPoint: textLayer.inPoint, outPoint: textLayer.outPoint, position: textLayer.property("Position").value }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- createShapeLayer (from createShapeLayer.jsx) --- 
function createShapeLayer(args) {
    try {
        var compName = args.compName || "";
        var shapeType = args.shapeType || "rectangle"; 
        var position = args.position || [960, 540]; 
        var size = args.size || [200, 200]; 
        var fillColor = args.fillColor || [1, 0, 0]; 
        var strokeColor = args.strokeColor || [0, 0, 0]; 
        var strokeWidth = args.strokeWidth || 0; 
        var startTime = args.startTime || 0;
        var duration = args.duration || 5; 
        var name = args.name || "Shape Layer";
        var points = args.points || 5; 
        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; } 
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }
        var shapeLayer = comp.layers.addShape();
        shapeLayer.name = name;
        var contents = shapeLayer.property("Contents"); 
        var shapeGroup = contents.addProperty("ADBE Vector Group");
        var groupContents = shapeGroup.property("Contents"); 
        var shapePathProperty;
        if (shapeType === "rectangle") {
            shapePathProperty = groupContents.addProperty("ADBE Vector Shape - Rect");
            shapePathProperty.property("Size").setValue(size);
        } else if (shapeType === "ellipse") {
            shapePathProperty = groupContents.addProperty("ADBE Vector Shape - Ellipse");
            shapePathProperty.property("Size").setValue(size);
        } else if (shapeType === "polygon" || shapeType === "star") { 
            shapePathProperty = groupContents.addProperty("ADBE Vector Shape - Star");
            // AE's star Type enum is 1 = Star, 2 = Polygon. Getting this
            // backwards leaves Inner Radius hidden, and setting a hidden
            // property throws.
            shapePathProperty.property("Type").setValue(shapeType === "polygon" ? 2 : 1);
            shapePathProperty.property("Points").setValue(points);
            shapePathProperty.property("Outer Radius").setValue(size[0] / 2);
            if (shapeType === "star") { shapePathProperty.property("Inner Radius").setValue(size[0] / 3); }
        }
        // Corner radius for rectangles (design-system radii).
        if (shapeType === "rectangle" && args.roundness) {
            shapePathProperty.property("Roundness").setValue(parseFloat(args.roundness));
        }
        if (args.fillNone !== true) {
            var fill = groupContents.addProperty("ADBE Vector Graphic - Fill");
            fill.property("Color").setValue(fillColor);
            fill.property("Opacity").setValue(
                args.fillOpacity === undefined || args.fillOpacity === null ? 100 : parseFloat(args.fillOpacity)
            );
        }
        if (strokeWidth > 0) {
            var stroke = groupContents.addProperty("ADBE Vector Graphic - Stroke");
            stroke.property("Color").setValue(strokeColor);
            stroke.property("Stroke Width").setValue(strokeWidth);
            stroke.property("Opacity").setValue(
                args.strokeOpacity === undefined || args.strokeOpacity === null ? 100 : parseFloat(args.strokeOpacity)
            );
            // args.dash = [dashLength, gapLength] -> dotted/dashed outline
            if (args.dash && args.dash.length >= 2) {
                var dashes = stroke.property("ADBE Vector Stroke Dashes");
                dashes.addProperty("ADBE Vector Stroke Dash 1").setValue(parseFloat(args.dash[0]));
                dashes.addProperty("ADBE Vector Stroke Gap 1").setValue(parseFloat(args.dash[1]));
            }
        }
        shapeLayer.property("Position").setValue(position);
        shapeLayer.startTime = startTime;
        if (duration > 0) { shapeLayer.outPoint = startTime + duration; }
        return JSON.stringify({
            status: "success", message: "Shape layer created successfully",
            layer: { name: shapeLayer.name, index: shapeLayer.index, type: "shape", shapeType: shapeType, inPoint: shapeLayer.inPoint, outPoint: shapeLayer.outPoint, position: shapeLayer.property("Position").value }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- createCamera ---
function createCamera(args) {
    try {
        var compName = args.compName || "";
        var name = args.name || "Camera";
        var zoom = args.zoom || 1777.78; // Default ~50mm equivalent
        var position = args.position; // Optional [x, y, z]
        var pointOfInterest = args.pointOfInterest; // Optional [x, y, z]
        var oneNode = args.oneNode || false; // If true, create a one-node camera (no point of interest)

        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; }
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }

        var centerPoint = [comp.width / 2, comp.height / 2];
        var cameraLayer = comp.layers.addCamera(name, centerPoint);
        cameraLayer.property("Camera Options").property("Zoom").setValue(zoom);

        if (oneNode) {
            cameraLayer.autoOrient = AutoOrientType.NO_AUTO_ORIENT;
        }

        if (position !== undefined && position !== null) {
            cameraLayer.property("Position").setValue(position);
        }

        if (pointOfInterest !== undefined && pointOfInterest !== null && !oneNode) {
            cameraLayer.property("Point of Interest").setValue(pointOfInterest);
        }

        var result = {
            name: cameraLayer.name,
            index: cameraLayer.index,
            zoom: cameraLayer.property("Camera Options").property("Zoom").value,
            position: cameraLayer.property("Position").value,
            oneNode: oneNode
        };
        if (!oneNode) {
            result.pointOfInterest = cameraLayer.property("Point of Interest").value;
        }

        return JSON.stringify({
            status: "success",
            message: "Camera created successfully",
            layer: result
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- duplicateLayer ---
function duplicateLayer(args) {
    try {
        var compName = args.compName || "";
        var layerIndex = args.layerIndex;
        var layerName = args.layerName || "";
        var newName = args.newName; // optional rename

        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; }
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }

        var layer = null;
        if (layerIndex !== undefined && layerIndex !== null) {
            if (layerIndex > 0 && layerIndex <= comp.numLayers) { layer = comp.layer(layerIndex); }
            else { throw new Error("Layer index out of bounds: " + layerIndex); }
        } else if (layerName) {
            for (var j = 1; j <= comp.numLayers; j++) {
                if (comp.layer(j).name === layerName) { layer = comp.layer(j); break; }
            }
        }
        if (!layer) { throw new Error("Layer not found: " + (layerName || "index " + layerIndex)); }

        var newLayer = layer.duplicate();
        if (newName) { newLayer.name = newName; }

        return JSON.stringify({
            status: "success",
            message: "Layer duplicated successfully",
            original: { name: layer.name, index: layer.index },
            duplicate: { name: newLayer.name, index: newLayer.index }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- deleteLayer ---
function deleteLayer(args) {
    try {
        var compName = args.compName || "";
        var layerIndex = args.layerIndex;
        var layerName = args.layerName || "";

        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; }
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }

        var layer = null;
        if (layerIndex !== undefined && layerIndex !== null) {
            if (layerIndex > 0 && layerIndex <= comp.numLayers) { layer = comp.layer(layerIndex); }
            else { throw new Error("Layer index out of bounds: " + layerIndex); }
        } else if (layerName) {
            for (var j = 1; j <= comp.numLayers; j++) {
                if (comp.layer(j).name === layerName) { layer = comp.layer(j); break; }
            }
        }
        if (!layer) { throw new Error("Layer not found: " + (layerName || "index " + layerIndex)); }

        var deletedName = layer.name;
        var deletedIndex = layer.index;
        layer.remove();

        return JSON.stringify({
            status: "success",
            message: "Layer deleted successfully",
            deleted: { name: deletedName, index: deletedIndex }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- setLayerMask: create or modify a mask on a layer ---
function setLayerMask(args) {
    try {
        var compName = args.compName || "";
        var layerIndex = args.layerIndex;
        var layerName = args.layerName || "";
        var maskIndex = args.maskIndex; // optional — if provided, modify existing mask
        var maskPath = args.maskPath; // array of [x, y] points defining the mask shape
        var maskRect = args.maskRect; // shorthand: {top, left, width, height} for rectangular masks
        var maskMode = args.maskMode || "add"; // "add", "subtract", "intersect", "none"
        var maskFeather = args.maskFeather; // optional [x, y] feather
        var maskOpacity = args.maskOpacity; // optional 0-100
        var maskExpansion = args.maskExpansion; // optional pixels
        var maskName = args.maskName; // optional rename

        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; }
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }

        var layer = null;
        if (layerIndex !== undefined && layerIndex !== null) {
            if (layerIndex > 0 && layerIndex <= comp.numLayers) { layer = comp.layer(layerIndex); }
            else { throw new Error("Layer index out of bounds: " + layerIndex); }
        } else if (layerName) {
            for (var j = 1; j <= comp.numLayers; j++) {
                if (comp.layer(j).name === layerName) { layer = comp.layer(j); break; }
            }
        }
        if (!layer) { throw new Error("Layer not found: " + (layerName || "index " + layerIndex)); }

        // Build the mask shape
        var shapePoints = [];
        if (maskRect) {
            // Rectangle shorthand
            var t = maskRect.top || 0;
            var l = maskRect.left || 0;
            var w = maskRect.width || comp.width;
            var h = maskRect.height || comp.height;
            shapePoints = [[l, t], [l + w, t], [l + w, t + h], [l, t + h]];
        } else if (maskPath && maskPath.length >= 3) {
            shapePoints = maskPath;
        } else {
            throw new Error("Must provide either maskRect or maskPath with at least 3 points");
        }

        // Create the shape object
        var myShape = new Shape();
        var vertices = [];
        for (var p = 0; p < shapePoints.length; p++) {
            vertices.push(shapePoints[p]);
        }
        myShape.vertices = vertices;
        myShape.closed = true;

        var changed = [];
        var mask;

        if (maskIndex !== undefined && maskIndex !== null) {
            // Modify existing mask
            if (maskIndex > 0 && maskIndex <= layer.property("Masks").numProperties) {
                mask = layer.property("Masks").property(maskIndex);
            } else {
                throw new Error("Mask index out of bounds: " + maskIndex);
            }
            mask.property("Mask Path").setValue(myShape);
            changed.push("maskPath");
        } else {
            // Create new mask
            mask = layer.property("Masks").addProperty("Mask");
            mask.property("Mask Path").setValue(myShape);
            changed.push("newMask");
        }

        // Set mask mode
        var modes = {
            "none": MaskMode.NONE,
            "add": MaskMode.ADD,
            "subtract": MaskMode.SUBTRACT,
            "intersect": MaskMode.INTERSECT,
            "lighten": MaskMode.LIGHTEN,
            "darken": MaskMode.DARKEN,
            "difference": MaskMode.DIFFERENCE
        };
        if (modes[maskMode] !== undefined) {
            mask.maskMode = modes[maskMode];
            changed.push("maskMode");
        }

        if (maskFeather !== undefined && maskFeather !== null) {
            mask.property("Mask Feather").setValue(maskFeather);
            changed.push("maskFeather");
        }
        if (maskOpacity !== undefined && maskOpacity !== null) {
            mask.property("Mask Opacity").setValue(maskOpacity);
            changed.push("maskOpacity");
        }
        if (maskExpansion !== undefined && maskExpansion !== null) {
            mask.property("Mask Expansion").setValue(maskExpansion);
            changed.push("maskExpansion");
        }
        if (maskName) {
            mask.name = maskName;
            changed.push("maskName");
        }

        return JSON.stringify({
            status: "success",
            message: "Mask set successfully",
            layer: { name: layer.name, index: layer.index },
            mask: {
                name: mask.name,
                index: mask.propertyIndex,
                mode: maskMode,
                changedProperties: changed
            }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- createSolidLayer (from createSolidLayer.jsx) ---
function createSolidLayer(args) {
    try {
        var compName = args.compName || "";
        var color = args.color || [1, 1, 1]; 
        var name = args.name || "Solid Layer";
        var position = args.position || [960, 540]; 
        var size = args.size; 
        var startTime = args.startTime || 0;
        var duration = args.duration || 5; 
        var isAdjustment = args.isAdjustment || false; 
        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; } 
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }
        if (!size) { size = [comp.width, comp.height]; }
        var solidLayer;
        if (isAdjustment) {
            solidLayer = comp.layers.addSolid([0, 0, 0], name, size[0], size[1], 1);
            solidLayer.adjustmentLayer = true;
        } else {
            solidLayer = comp.layers.addSolid(color, name, size[0], size[1], 1);
        }
        solidLayer.property("Position").setValue(position);
        solidLayer.startTime = startTime;
        if (duration > 0) { solidLayer.outPoint = startTime + duration; }
        return JSON.stringify({
            status: "success", message: isAdjustment ? "Adjustment layer created successfully" : "Solid layer created successfully",
            layer: { name: solidLayer.name, index: solidLayer.index, type: isAdjustment ? "adjustment" : "solid", inPoint: solidLayer.inPoint, outPoint: solidLayer.outPoint, position: solidLayer.property("Position").value, isAdjustment: solidLayer.adjustmentLayer }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- setLayerProperties (modified to handle text properties) ---
// Set a transform property to a static value.
//
// After Effects rejects setValue() on an animated property, which used to pass
// silently here: the caller was told "scale" changed while the layer never
// moved. Report it instead — the caller needs to know its edit didn't land, and
// silently deleting an element's entrance animation is worse than refusing.
function setStaticTransform(layer, propName, value, label, changed, skipped) {
    var prop;
    try {
        prop = layer.property(propName);
    } catch (e) {
        skipped.push(label + ": layer has no " + propName + " property");
        return false;
    }
    if (!prop) {
        skipped.push(label + ": layer has no " + propName + " property");
        return false;
    }
    if (prop.numKeys > 0) {
        skipped.push(label + ": property is animated (" + prop.numKeys +
                     " keyframes); use setLayerKeyframe instead of a static value");
        return false;
    }
    if (prop.expressionEnabled && prop.expression) {
        // The expression wins at render time, so a static write is a no-op.
        skipped.push(label + ": property is driven by an expression; the static value would be ignored");
        return false;
    }
    prop.setValue(value);
    changed.push(label);
    return true;
}

function setLayerProperties(args) {
    try {
        var compName = args.compName || "";
        var layerName = args.layerName || "";
        var layerIndex = args.layerIndex; 
        
        // General Properties
        var position = args.position; 
        var scale = args.scale; 
        var rotation = args.rotation; 
        var opacity = args.opacity; 
        var startTime = args.startTime; 
        var duration = args.duration; 

        // Text Specific Properties
        var textContent = args.text; // New: text content
        var fontFamily = args.fontFamily; // New: font family
        var fontSize = args.fontSize; // New: font size
        var fillColor = args.fillColor; // New: font color
        
        // Find the composition (same logic as before)
        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; } 
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }
        
        // Find the layer (same logic as before)
        var layer = null;
        if (layerIndex !== undefined && layerIndex !== null) {
            if (layerIndex > 0 && layerIndex <= comp.numLayers) { layer = comp.layer(layerIndex); } 
            else { throw new Error("Layer index out of bounds: " + layerIndex); }
        } else if (layerName) {
            for (var j = 1; j <= comp.numLayers; j++) {
                if (comp.layer(j).name === layerName) { layer = comp.layer(j); break; }
            }
        }
        if (!layer) { throw new Error("Layer not found: " + (layerName || "index " + layerIndex)); }
        
        var changedProperties = [];
        // Requested edits that did NOT take effect, with the reason.
        var skippedProperties = [];
        var textDocumentChanged = false;
        var textProp = null;
        var textDocument = null;

        // --- Text Property Handling ---
        if (layer instanceof TextLayer && (textContent !== undefined || fontFamily !== undefined || fontSize !== undefined || fillColor !== undefined)) {
            var sourceTextProp = layer.property("Source Text");
            if (sourceTextProp && sourceTextProp.value) {
                var currentTextDocument = sourceTextProp.value; // Get the current value
                var updated = false;

                if (textContent !== undefined && textContent !== null && currentTextDocument.text !== textContent) {
                    currentTextDocument.text = textContent;
                    changedProperties.push("text");
                    updated = true;
                }
                if (fontFamily !== undefined && fontFamily !== null && currentTextDocument.font !== fontFamily) {
                    // Add basic validation/logging for font existence if needed
                    // try { app.fonts.findFont(fontFamily); } catch (e) { logToPanel("Warning: Font '"+fontFamily+"' might not be installed."); }
                    currentTextDocument.font = fontFamily;
                    changedProperties.push("fontFamily");
                    updated = true;
                }
                if (fontSize !== undefined && fontSize !== null && currentTextDocument.fontSize !== fontSize) {
                    currentTextDocument.fontSize = fontSize;
                    changedProperties.push("fontSize");
                    updated = true;
                }
                // Comparing colors needs care due to potential floating point inaccuracies if set via UI
                // Simple comparison for now
                if (fillColor !== undefined && fillColor !== null && 
                    (currentTextDocument.fillColor[0] !== fillColor[0] || 
                     currentTextDocument.fillColor[1] !== fillColor[1] || 
                     currentTextDocument.fillColor[2] !== fillColor[2])) {
                    currentTextDocument.fillColor = fillColor;
                    changedProperties.push("fillColor");
                    updated = true;
                }

                // Only set the value if something actually changed
                if (updated) {
                    try {
                        sourceTextProp.setValue(currentTextDocument);
                        logToPanel("Applied changes to Text Document for layer: " + layer.name);
                    } catch (e) {
                        logToPanel("ERROR applying Text Document changes: " + e.toString());
                        // Decide if we should throw or just log the error for text properties
                        // For now, just log, other properties might still succeed
                    }
                }
                 // Store the potentially updated document for the return value
                 textDocument = currentTextDocument; 

            } else {
                logToPanel("Warning: Could not access Source Text property for layer: " + layer.name);
            }
        }

        // --- Enabled/Visible ---
        var enabled = args.enabled;
        if (enabled !== undefined && enabled !== null) { layer.enabled = !!enabled; changedProperties.push("enabled"); }

        // --- Blend Mode ---
        var blendMode = args.blendMode;
        if (blendMode !== undefined && blendMode !== null) {
            var modes = {
                "normal": BlendingMode.NORMAL,
                "add": BlendingMode.ADD,
                "multiply": BlendingMode.MULTIPLY,
                "screen": BlendingMode.SCREEN,
                "overlay": BlendingMode.OVERLAY,
                "softLight": BlendingMode.SOFT_LIGHT,
                "hardLight": BlendingMode.HARD_LIGHT,
                "colorDodge": BlendingMode.COLOR_DODGE,
                "colorBurn": BlendingMode.COLOR_BURN,
                "darken": BlendingMode.DARKEN,
                "lighten": BlendingMode.LIGHTEN,
                "difference": BlendingMode.DIFFERENCE,
                "exclusion": BlendingMode.EXCLUSION,
                "hue": BlendingMode.HUE,
                "saturation": BlendingMode.SATURATION,
                "color": BlendingMode.COLOR,
                "luminosity": BlendingMode.LUMINOSITY
            };
            if (modes[blendMode] !== undefined) {
                layer.blendingMode = modes[blendMode];
                changedProperties.push("blendMode");
            }
        }

        // --- Track Matte ---
        var trackMatteType = args.trackMatteType;
        if (trackMatteType !== undefined && trackMatteType !== null) {
            // Values: "none", "alpha", "alphaInverted", "luma", "lumaInverted"
            var matteTypes = {
                "none": TrackMatteType.NO_TRACK_MATTE,
                "alpha": TrackMatteType.ALPHA,
                "alphaInverted": TrackMatteType.ALPHA_INVERTED,
                "luma": TrackMatteType.LUMA,
                "lumaInverted": TrackMatteType.LUMA_INVERTED
            };
            if (matteTypes[trackMatteType] !== undefined) {
                layer.trackMatteType = matteTypes[trackMatteType];
                changedProperties.push("trackMatteType");
            }
        }

        // --- General Property Handling ---
        var threeDLayer = args.threeDLayer;
        if (threeDLayer !== undefined && threeDLayer !== null) { layer.threeDLayer = !!threeDLayer; changedProperties.push("threeDLayer"); }
        if (position !== undefined && position !== null) {
            var posProp = layer.property("Position");
            // Position deliberately overrides an existing animation.
            if (posProp.numKeys > 0) {
                while (posProp.numKeys > 0) { posProp.removeKey(1); }
                skippedProperties.push("position: removed existing keyframes to set a static value");
            }
            posProp.setValue(position);
            changedProperties.push("position");
        }
        if (scale !== undefined && scale !== null) {
            setStaticTransform(layer, "Scale", scale, "scale", changedProperties, skippedProperties);
        }
        if (rotation !== undefined && rotation !== null) {
            // For 3D layers, Z rotation is often what's intended by a single value
            var rotProp = layer.threeDLayer ? "Z Rotation" : "Rotation";
            setStaticTransform(layer, rotProp, rotation, "rotation", changedProperties, skippedProperties);
        }
        if (opacity !== undefined && opacity !== null) {
            setStaticTransform(layer, "Opacity", opacity, "opacity", changedProperties, skippedProperties);
        }
        if (startTime !== undefined && startTime !== null) { layer.startTime = startTime; changedProperties.push("startTime"); }
        if (duration !== undefined && duration !== null && duration > 0) {
            var actualStartTime = (startTime !== undefined && startTime !== null) ? startTime : layer.startTime;
            layer.outPoint = actualStartTime + duration;
            changedProperties.push("duration");
        }

        // Return success with updated layer details (including text if changed)
        var returnLayerInfo = {
            name: layer.name,
            index: layer.index,
            threeDLayer: layer.threeDLayer,
            position: layer.property("Position").value,
            scale: layer.property("Scale").value,
            rotation: layer.threeDLayer ? layer.property("Z Rotation").value : layer.property("Rotation").value, // Return appropriate rotation
            opacity: layer.property("Opacity").value,
            inPoint: layer.inPoint,
            outPoint: layer.outPoint,
            changedProperties: changedProperties,
            skippedProperties: skippedProperties
        };
        // Add text properties to the return object if it was a text layer
        if (layer instanceof TextLayer && textDocument) {
            returnLayerInfo.text = textDocument.text;
            returnLayerInfo.fontFamily = textDocument.font;
            returnLayerInfo.fontSize = textDocument.fontSize;
            returnLayerInfo.fillColor = textDocument.fillColor;
        }

        // *** ADDED LOGGING HERE ***
        logToPanel("Final check before return:");
        logToPanel("  Changed Properties: " + changedProperties.join(", "));
        logToPanel("  Return Layer Info Font: " + (returnLayerInfo.fontFamily || "N/A")); 
        logToPanel("  TextDocument Font: " + (textDocument ? textDocument.font : "N/A"));

        return JSON.stringify({
            status: "success", message: "Layer properties updated successfully",
            layer: returnLayerInfo
        }, null, 2);
    } catch (error) {
        // Error handling remains similar, but add more specific checks if needed
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// --- batchSetLayerProperties: apply properties to multiple layers in one call ---
function batchSetLayerProperties(args) {
    try {
        var compName = args.compName || "";
        var operations = args.operations; // Array of {layerIndex, threeDLayer, position, scale, rotation, opacity, ...}

        if (!operations || !operations.length) {
            throw new Error("No operations provided. Pass an array of {layerIndex, ...properties}");
        }

        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; }
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }

        var results = [];
        for (var o = 0; o < operations.length; o++) {
            var op = operations[o];
            var layer = null;
            if (op.layerIndex !== undefined && op.layerIndex !== null) {
                if (op.layerIndex > 0 && op.layerIndex <= comp.numLayers) { layer = comp.layer(op.layerIndex); }
                else { results.push({ layerIndex: op.layerIndex, status: "error", message: "Layer index out of bounds" }); continue; }
            } else if (op.layerName) {
                for (var j = 1; j <= comp.numLayers; j++) {
                    if (comp.layer(j).name === op.layerName) { layer = comp.layer(j); break; }
                }
            }
            if (!layer) { results.push({ layerIndex: op.layerIndex, layerName: op.layerName, status: "error", message: "Layer not found" }); continue; }

            var changed = [];
            if (op.threeDLayer !== undefined && op.threeDLayer !== null) { layer.threeDLayer = !!op.threeDLayer; changed.push("threeDLayer"); }
            if (op.position !== undefined && op.position !== null) {
                var posProp = layer.property("Position");
                if (posProp.numKeys > 0) {
                    while (posProp.numKeys > 0) { posProp.removeKey(1); }
                }
                posProp.setValue(op.position);
                changed.push("position");
            }
            if (op.scale !== undefined && op.scale !== null) { layer.property("Scale").setValue(op.scale); changed.push("scale"); }
            if (op.rotation !== undefined && op.rotation !== null) {
                if (layer.threeDLayer) { layer.property("Z Rotation").setValue(op.rotation); }
                else { layer.property("Rotation").setValue(op.rotation); }
                changed.push("rotation");
            }
            if (op.opacity !== undefined && op.opacity !== null) { layer.property("Opacity").setValue(op.opacity); changed.push("opacity"); }
            if (op.blendMode !== undefined && op.blendMode !== null) {
                var bModes = {"normal":BlendingMode.NORMAL,"add":BlendingMode.ADD,"multiply":BlendingMode.MULTIPLY,"screen":BlendingMode.SCREEN,"overlay":BlendingMode.OVERLAY,"softLight":BlendingMode.SOFT_LIGHT,"hardLight":BlendingMode.HARD_LIGHT,"darken":BlendingMode.DARKEN,"lighten":BlendingMode.LIGHTEN,"difference":BlendingMode.DIFFERENCE};
                if (bModes[op.blendMode] !== undefined) { layer.blendingMode = bModes[op.blendMode]; changed.push("blendMode"); }
            }
            if (op.startTime !== undefined && op.startTime !== null) { layer.startTime = op.startTime; changed.push("startTime"); }
            if (op.outPoint !== undefined && op.outPoint !== null) { layer.outPoint = op.outPoint; changed.push("outPoint"); }

            results.push({
                layerIndex: layer.index,
                name: layer.name,
                status: "success",
                threeDLayer: layer.threeDLayer,
                position: layer.property("Position").value,
                changedProperties: changed
            });
        }

        return JSON.stringify({ status: "success", results: results }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

/**
 * Sets a keyframe for a specific property on a layer.
 * Indices are 1-based for After Effects collections.
 * @param {number} compIndex - The index of the composition (1-based).
 * @param {number} layerIndex - The index of the layer within the composition (1-based).
 * @param {string} propertyName - The name of the property (e.g., "Position", "Scale", "Rotation", "Opacity").
 * @param {number} timeInSeconds - The time (in seconds) for the keyframe.
 * @param {any} value - The value for the keyframe (e.g., [x, y] for Position, [w, h] for Scale, angle for Rotation, percentage for Opacity).
 * @returns {string} JSON string indicating success or error.
 */
function setLayerKeyframe(compIndex, layerIndex, propertyName, timeInSeconds, value) {
    try {
        // Use 1-based indices as per After Effects API
        var comp = app.project.items[compIndex];
        if (!comp || !(comp instanceof CompItem)) {
            return JSON.stringify({ success: false, message: "Composition not found at index " + compIndex });
        }
        var layer = comp.layers[layerIndex];
        if (!layer) {
            return JSON.stringify({ success: false, message: "Layer not found at index " + layerIndex + " in composition '" + comp.name + "'"});
        }

        var transformGroup = layer.property("Transform");
        if (!transformGroup) {
             return JSON.stringify({ success: false, message: "Transform properties not found for layer '" + layer.name + "' (type: " + layer.matchName + ")." });
        }

        var property = transformGroup.property(propertyName);
        if (!property) {
            // Check other common property groups if not in Transform
             if (layer.property("Effects") && layer.property("Effects").property(propertyName)) {
                 property = layer.property("Effects").property(propertyName);
             } else if (layer.property("Text") && layer.property("Text").property(propertyName)) {
                 property = layer.property("Text").property(propertyName);
            } // Add more groups if needed (e.g., Masks, Shapes)

            if (!property) {
                 return JSON.stringify({ success: false, message: "Property '" + propertyName + "' not found on layer '" + layer.name + "'." });
            }
        }


        // Ensure the property can be keyframed
        if (!property.canVaryOverTime) {
             return JSON.stringify({ success: false, message: "Property '" + propertyName + "' cannot be keyframed." });
        }

        // Make sure the property is enabled for keyframing
        if (property.numKeys === 0 && !property.isTimeVarying) {
             property.setValueAtTime(comp.time, property.value); // Set initial keyframe if none exist
        }


        property.setValueAtTime(timeInSeconds, value);

        return JSON.stringify({ success: true, message: "Keyframe set for '" + propertyName + "' on layer '" + layer.name + "' at " + timeInSeconds + "s." });
    } catch (e) {
        return JSON.stringify({ success: false, message: "Error setting keyframe: " + e.toString() + " (Line: " + e.line + ")" });
    }
}


/**
 * Sets an expression for a specific property on a layer.
 * @param {number} compIndex - The index of the composition (1-based).
 * @param {number} layerIndex - The index of the layer within the composition (1-based).
 * @param {string} propertyName - The name of the property (e.g., "Position", "Scale", "Rotation", "Opacity").
 * @param {string} expressionString - The JavaScript expression string. Use "" to remove expression.
 * @returns {string} JSON string indicating success or error.
 */
// Accepts either a compName (preferred — project item indices shift whenever
// footage is imported) or a positional compIndex.
function setLayerExpression(compIndex, layerIndex, propertyName, expressionString, compName) {
    try {
        var comp = null;
        if (compName) {
            comp = findCompByName(compName);
        } else {
            comp = app.project.items[compIndex];
        }
         if (!comp || !(comp instanceof CompItem)) {
            return JSON.stringify({ success: false, message: "Composition not found (" + (compName || ("index " + compIndex)) + ")" });
        }
        var layer = comp.layers[layerIndex];
         if (!layer) {
            return JSON.stringify({ success: false, message: "Layer not found at index " + layerIndex + " in composition '" + comp.name + "'"});
        }

        var transformGroup = layer.property("Transform");
         if (!transformGroup) {
             // Allow expressions on non-transformable layers if property exists elsewhere
             // return JSON.stringify({ success: false, message: "Transform properties not found for layer '" + layer.name + "' (type: " + layer.matchName + ")." });
        }

        var property = transformGroup ? transformGroup.property(propertyName) : null;
         if (!property) {
            // Check other common property groups if not in Transform
             if (layer.property("Effects") && layer.property("Effects").property(propertyName)) {
                 property = layer.property("Effects").property(propertyName);
             } else if (layer.property("Text") && layer.property("Text").property(propertyName)) {
                 property = layer.property("Text").property(propertyName);
             }

            // Search inside individual effects for sub-properties
            if (!property && layer.property("Effects")) {
                var effects = layer.property("Effects");
                for (var ei = 1; ei <= effects.numProperties; ei++) {
                    var eff = effects.property(ei);
                    try {
                        var subProp = eff.property(propertyName);
                        if (subProp) { property = subProp; break; }
                    } catch (e2) {}
                }
            }

            if (!property) {
                 return JSON.stringify({ success: false, message: "Property '" + propertyName + "' not found on layer '" + layer.name + "'." });
            }
        }

        if (!property.canSetExpression) {
            return JSON.stringify({ success: false, message: "Property '" + propertyName + "' does not support expressions." });
        }

        property.expression = expressionString;

        var action = expressionString === "" ? "removed" : "set";
        return JSON.stringify({ success: true, message: "Expression " + action + " for '" + propertyName + "' on layer '" + layer.name + "'." });
    } catch (e) {
        return JSON.stringify({ success: false, message: "Error setting expression: " + e.toString() + " (Line: " + e.line + ")" });
    }
}

// --- applyEffect (from applyEffect.jsx) ---
function applyEffect(args) {
    try {
        // Extract parameters
        var compIndex = args.compIndex || 1; // Default to first comp
        var layerIndex = args.layerIndex || 1; // Default to first layer
        var effectName = args.effectName; // Name of the effect to apply
        var effectMatchName = args.effectMatchName; // After Effects internal name (more reliable)
        var effectCategory = args.effectCategory || ""; // Optional category for filtering
        var presetPath = args.presetPath; // Optional path to an effect preset
        var effectSettings = args.effectSettings || {}; // Optional effect parameters
        
        if (!effectName && !effectMatchName && !presetPath) {
            throw new Error("You must specify either effectName, effectMatchName, or presetPath");
        }
        
        // Prefer compName: project item indices shift when footage is imported.
        var comp = args.compName ? findCompByName(args.compName) : app.project.item(compIndex);
        if (!comp || !(comp instanceof CompItem)) {
            throw new Error("Composition not found (" + (args.compName || ("index " + compIndex)) + ")");
        }
        
        // Find the layer by index
        var layer = comp.layer(layerIndex);
        if (!layer) {
            throw new Error("Layer not found at index " + layerIndex + " in composition '" + comp.name + "'");
        }
        
        var effectResult;
        
        // Apply preset if a path is provided
        if (presetPath) {
            var presetFile = new File(presetPath);
            if (!presetFile.exists) {
                throw new Error("Effect preset file not found: " + presetPath);
            }
            
            // Apply the preset to the layer
            layer.applyPreset(presetFile);
            effectResult = {
                type: "preset",
                name: presetPath.split('/').pop().split('\\').pop(),
                applied: true
            };
        }
        // Apply effect by match name (more reliable method)
        else if (effectMatchName) {
            var effect = layer.Effects.addProperty(effectMatchName);
            effectResult = {
                type: "effect",
                name: effect.name,
                matchName: effect.matchName,
                index: effect.propertyIndex
            };
            
            // Apply settings if provided
            applyEffectSettings(effect, effectSettings);
        }
        // Apply effect by display name
        else {
            // Get the effect from the Effect menu
            var effect = layer.Effects.addProperty(effectName);
            effectResult = {
                type: "effect",
                name: effect.name,
                matchName: effect.matchName,
                index: effect.propertyIndex
            };
            
            // Apply settings if provided
            applyEffectSettings(effect, effectSettings);
        }
        
        return JSON.stringify({
            status: "success",
            message: "Effect applied successfully",
            effect: effectResult,
            layer: {
                name: layer.name,
                index: layerIndex
            },
            composition: {
                name: comp.name,
                index: compIndex
            }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({
            status: "error",
            message: error.toString()
        }, null, 2);
    }
}

// Helper function to apply effect settings
function applyEffectSettings(effect, settings) {
    // Skip if no settings are provided
    if (!settings) return;
    var hasKeys = false;
    for (var k in settings) { if (settings.hasOwnProperty(k)) { hasKeys = true; break; } }
    if (!hasKeys) return;
    
    // Iterate through all provided settings
    for (var propName in settings) {
        if (settings.hasOwnProperty(propName)) {
            try {
                // Find the property in the effect
                var property = null;
                
                // Try direct property access first
                try {
                    property = effect.property(propName);
                } catch (e) {
                    // If direct access fails, search through all properties
                    for (var i = 1; i <= effect.numProperties; i++) {
                        var prop = effect.property(i);
                        if (prop.name === propName) {
                            property = prop;
                            break;
                        }
                    }
                }
                
                // Set the property value if found
                if (property && property.setValue) {
                    property.setValue(settings[propName]);
                }
            } catch (e) {
                // Log error but continue with other properties
                $.writeln("Error setting effect property '" + propName + "': " + e.toString());
            }
        }
    }
}

// --- applyEffectTemplate (from applyEffectTemplate.jsx) ---
function applyEffectTemplate(args) {
    try {
        // Extract parameters
        var compIndex = args.compIndex || 1; // Default to first comp
        var layerIndex = args.layerIndex || 1; // Default to first layer
        var templateName = args.templateName; // Name of the template to apply
        var customSettings = args.customSettings || {}; // Optional customizations
        
        if (!templateName) {
            throw new Error("You must specify a templateName");
        }
        
        // Prefer compName: project item indices shift when footage is imported.
        var comp = args.compName ? findCompByName(args.compName) : app.project.item(compIndex);
        if (!comp || !(comp instanceof CompItem)) {
            throw new Error("Composition not found (" + (args.compName || ("index " + compIndex)) + ")");
        }
        
        // Find the layer by index
        var layer = comp.layer(layerIndex);
        if (!layer) {
            throw new Error("Layer not found at index " + layerIndex + " in composition '" + comp.name + "'");
        }
        
        // Template definitions
        var templates = {
            // Blur effects
            "gaussian-blur": {
                effectMatchName: "ADBE Gaussian Blur 2",
                settings: {
                    "Blurriness": customSettings.blurriness || 20
                }
            },
            "directional-blur": {
                effectMatchName: "ADBE Directional Blur",
                settings: {
                    "Direction": customSettings.direction || 0,
                    "Blur Length": customSettings.length || 10
                }
            },
            
            // Color correction effects
            "color-balance": {
                effectMatchName: "ADBE Color Balance (HLS)",
                settings: {
                    "Hue": customSettings.hue || 0,
                    "Lightness": customSettings.lightness || 0,
                    "Saturation": customSettings.saturation || 0
                }
            },
            "brightness-contrast": {
                effectMatchName: "ADBE Brightness & Contrast 2",
                settings: {
                    "Brightness": customSettings.brightness || 0,
                    "Contrast": customSettings.contrast || 0,
                    "Use Legacy": false
                }
            },
            "curves": {
                effectMatchName: "ADBE CurvesCustom",
                // Curves are complex and would need special handling
            },
            
            // Stylistic effects
            "glow": {
                effectMatchName: "ADBE Glow",
                settings: {
                    "Glow Threshold": customSettings.threshold || 50,
                    "Glow Radius": customSettings.radius || 15,
                    "Glow Intensity": customSettings.intensity || 1
                }
            },
            "drop-shadow": {
                effectMatchName: "ADBE Drop Shadow",
                settings: {
                    "Shadow Color": customSettings.color || [0, 0, 0, 1],
                    "Opacity": customSettings.opacity || 50,
                    "Direction": customSettings.direction || 135,
                    "Distance": customSettings.distance || 10,
                    "Softness": customSettings.softness || 10
                }
            },
            
            // Common effect chains
            "cinematic-look": {
                effects: [
                    {
                        effectMatchName: "ADBE CurvesCustom",
                        settings: {}
                    },
                    {
                        effectMatchName: "ADBE Vibrance",
                        settings: {
                            "Vibrance": 15,
                            "Saturation": -5
                        }
                    }
                ]
            },
            "text-pop": {
                effects: [
                    {
                        effectMatchName: "ADBE Drop Shadow",
                        settings: {
                            "Shadow Color": [0, 0, 0, 1],
                            "Opacity": 75,
                            "Distance": 5,
                            "Softness": 10
                        }
                    },
                    {
                        effectMatchName: "ADBE Glow",
                        settings: {
                            "Glow Threshold": 50,
                            "Glow Radius": 10,
                            "Glow Intensity": 1.5
                        }
                    }
                ]
            }
        };
        
        // Check if the requested template exists
        var template = templates[templateName];
        if (!template) {
            var availableTemplates = Object.keys(templates).join(", ");
            throw new Error("Template '" + templateName + "' not found. Available templates: " + availableTemplates);
        }
        
        var appliedEffects = [];
        
        // Apply single effect or multiple effects based on template structure
        if (template.effectMatchName) {
            // Single effect template
            var effect = layer.Effects.addProperty(template.effectMatchName);
            
            // Apply settings
            for (var propName in template.settings) {
                try {
                    var property = effect.property(propName);
                    if (property) {
                        property.setValue(template.settings[propName]);
                    }
                } catch (e) {
                    $.writeln("Warning: Could not set " + propName + " on effect " + effect.name + ": " + e);
                }
            }
            
            appliedEffects.push({
                name: effect.name,
                matchName: effect.matchName
            });
        } else if (template.effects) {
            // Multiple effects template
            for (var i = 0; i < template.effects.length; i++) {
                var effectData = template.effects[i];
                var effect = layer.Effects.addProperty(effectData.effectMatchName);
                
                // Apply settings
                for (var propName in effectData.settings) {
                    try {
                        var property = effect.property(propName);
                        if (property) {
                            property.setValue(effectData.settings[propName]);
                        }
                    } catch (e) {
                        $.writeln("Warning: Could not set " + propName + " on effect " + effect.name + ": " + e);
                    }
                }
                
                appliedEffects.push({
                    name: effect.name,
                    matchName: effect.matchName
                });
            }
        }
        
        return JSON.stringify({
            status: "success",
            message: "Effect template '" + templateName + "' applied successfully",
            appliedEffects: appliedEffects,
            layer: {
                name: layer.name,
                index: layerIndex
            },
            composition: {
                name: comp.name,
                index: compIndex
            }
        }, null, 2);
    } catch (error) {
        return JSON.stringify({
            status: "error",
            message: error.toString()
        }, null, 2);
    }
}

// --- End of Function Definitions ---

// --- Bridge test function to verify communication and effects application ---
function bridgeTestEffects(args) {
    try {
        var compIndex = (args && args.compIndex) ? args.compIndex : 1;
        var layerIndex = (args && args.layerIndex) ? args.layerIndex : 1;

        // Apply a light Gaussian Blur
        var blurRes = JSON.parse(applyEffect({
            compIndex: compIndex,
            layerIndex: layerIndex,
            effectMatchName: "ADBE Gaussian Blur 2",
            effectSettings: { "Blurriness": 5 }
        }));

        // Apply a simple drop shadow via template
        var shadowRes = JSON.parse(applyEffectTemplate({
            compIndex: compIndex,
            layerIndex: layerIndex,
            templateName: "drop-shadow"
        }));

        return JSON.stringify({
            status: "success",
            message: "Bridge test effects applied.",
            results: [blurRes, shadowRes]
        }, null, 2);
    } catch (e) {
        return JSON.stringify({ status: "error", message: e.toString() }, null, 2);
    }
}

// JSON polyfill for ExtendScript (when JSON is undefined)
if (typeof JSON === "undefined") {
    JSON = {};
}
if (typeof JSON.parse !== "function") {
    JSON.parse = function (text) {
        // Safe-ish fallback for trusted input (our own command file)
        return eval("(" + text + ")");
    };
}
if (typeof JSON.stringify !== "function") {
    (function () {
        function esc(str) {
            return (str + "")
                .replace(/\\/g, "\\\\")
                .replace(/"/g, '\\"')
                .replace(/\n/g, "\\n")
                .replace(/\r/g, "\\r")
                .replace(/\t/g, "\\t");
        }
        function toJSON(val) {
            if (val === null) return "null";
            var t = typeof val;
            if (t === "number" || t === "boolean") return String(val);
            if (t === "string") return '"' + esc(val) + '"';
            if (val instanceof Array) {
                var a = [];
                for (var i = 0; i < val.length; i++) a.push(toJSON(val[i]));
                return "[" + a.join(",") + "]";
            }
            if (t === "object") {
                var props = [];
                for (var k in val) {
                    if (val.hasOwnProperty(k) && typeof val[k] !== "function" && typeof val[k] !== "undefined") {
                        props.push('"' + esc(k) + '":' + toJSON(val[k]));
                    }
                }
                return "{" + props.join(",") + "}";
            }
            return "null";
        }
        JSON.stringify = function (value, _replacer, _space) {
            return toJSON(value);
        };
    })();
}

// Detect AE version (AE 2025 = version 25.x, AE 2026 = version 26.x)
var aeVersion = parseFloat(app.version);
var isAE2025OrLater = aeVersion >= 25.0;

// Always create a floating palette window for AE 2025+
var panel = new Window("palette", "MCP Bridge Auto", undefined);
panel.orientation = "column";
panel.alignChildren = ["fill", "top"];
panel.spacing = 10;
panel.margins = 16;

// Status display
var statusText = panel.add("statictext", undefined, "Waiting for commands...");
statusText.alignment = ["fill", "top"];

// Add log area
var logPanel = panel.add("panel", undefined, "Command Log");
logPanel.orientation = "column";
logPanel.alignChildren = ["fill", "fill"];
var logText = logPanel.add("edittext", undefined, "", {multiline: true, readonly: true});
logText.preferredSize.height = 200;

// AE 2025 warning
if (isAE2025OrLater) {
    var warning = panel.add("statictext", undefined, "AE 2025+: Dockable panels are not supported. Floating window only.");
    warning.graphics.foregroundColor = warning.graphics.newPen(warning.graphics.PenType.SOLID_COLOR, [1,0.3,0,1], 1);
}

// Auto-run checkbox
var autoRunCheckbox = panel.add("checkbox", undefined, "Auto-run commands");
autoRunCheckbox.value = true;

// Check interval (ms)
var checkInterval = 2000;
var isChecking = false;

// --- setCompositionProperties: set duration, frameRate, etc. on active or named comp ---
function setCompositionProperties(args) {
    try {
        var compName = args.compName || "";
        var comp = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var item = app.project.item(i);
            if (item instanceof CompItem && item.name === compName) { comp = item; break; }
        }
        if (!comp) {
            if (app.project.activeItem instanceof CompItem) { comp = app.project.activeItem; }
            else { throw new Error("No composition found with name '" + compName + "' and no active composition"); }
        }
        var changed = [];
        if (args.duration !== undefined && args.duration !== null) { comp.duration = args.duration; changed.push("duration"); }
        if (args.frameRate !== undefined && args.frameRate !== null) { comp.frameRate = args.frameRate; changed.push("frameRate"); }
        if (args.width !== undefined && args.width !== null && args.height !== undefined && args.height !== null) {
            comp.width = args.width; comp.height = args.height; changed.push("dimensions");
        }
        // The background colour is a preview backdrop only: it never renders
        // into the alpha channel. It does tint the RGB of fully transparent
        // pixels though, so black is the safe choice for a comp meant to be
        // composited over other footage.
        if (args.backgroundColor) {
            var bg = args.backgroundColor;
            comp.bgColor = [bg.r / 255, bg.g / 255, bg.b / 255];
            changed.push("backgroundColor");
        }
        return JSON.stringify({
            status: "success",
            composition: {
                name: comp.name, duration: comp.duration, frameRate: comp.frameRate,
                width: comp.width, height: comp.height, bgColor: comp.bgColor
            },
            changedProperties: changed
        }, null, 2);
    } catch (error) {
        return JSON.stringify({ status: "error", message: error.toString() }, null, 2);
    }
}

// Functions for each script type
function getProjectInfo() {
    var project = app.project;
    var result = {
        projectName: project.file ? project.file.name : "Untitled Project",
        path: project.file ? project.file.fsName : "",
        numItems: project.numItems,
        bitsPerChannel: project.bitsPerChannel,
        timeMode: project.timeDisplayType === TimeDisplayType.FRAMES ? "Frames" : "Timecode",
        items: []
    };

    // Count item types
    var countByType = {
        compositions: 0,
        footage: 0,
        folders: 0,
        solids: 0
    };

    // Get item information (limited for performance)
    for (var i = 1; i <= Math.min(project.numItems, 50); i++) {
        var item = project.item(i);
        var itemType = "";
        
        if (item instanceof CompItem) {
            itemType = "Composition";
            countByType.compositions++;
        } else if (item instanceof FolderItem) {
            itemType = "Folder";
            countByType.folders++;
        } else if (item instanceof FootageItem) {
            if (item.mainSource instanceof SolidSource) {
                itemType = "Solid";
                countByType.solids++;
            } else {
                itemType = "Footage";
                countByType.footage++;
            }
        }
        
        result.items.push({
            id: item.id,
            name: item.name,
            type: itemType
        });
    }
    
    result.itemCounts = countByType;

    // Include active composition metadata if available
    if (app.project.activeItem instanceof CompItem) {
        var ac = app.project.activeItem;
        result.activeComp = {
            id: ac.id,
            name: ac.name,
            width: ac.width,
            height: ac.height,
            duration: ac.duration,
            frameRate: ac.frameRate,
            numLayers: ac.numLayers
        };
    }

    return JSON.stringify(result, null, 2);
}

function listCompositions() {
    var project = app.project;
    var result = {
        compositions: []
    };
    
    // Loop through items in the project
    for (var i = 1; i <= project.numItems; i++) {
        var item = project.item(i);
        
        // Check if the item is a composition
        if (item instanceof CompItem) {
            result.compositions.push({
                id: item.id,
                name: item.name,
                duration: item.duration,
                frameRate: item.frameRate,
                width: item.width,
                height: item.height,
                numLayers: item.numLayers
            });
        }
    }
    
    return JSON.stringify(result, null, 2);
}

// Transform properties worth reporting, in a fixed order.
var LAYER_TRANSFORM_PROPS = [
    "Anchor Point", "Position", "Scale", "Rotation", "Opacity"
];

// Read a property's static value, tolerating layers that lack it (a camera has
// no Opacity, a 2D layer no Z Rotation).
function safePropValue(layer, name) {
    try {
        var p = layer.property(name);
        return p ? p.value : null;
    } catch (e) {
        return null;
    }
}

// Dump a property's keyframes as [{time, value, inInterp, outInterp}], or null
// when it isn't animated. This is what makes an existing animation
// reproducible: without it a caller can only guess at timing and easing.
function propKeyframes(layer, name) {
    var p;
    try {
        p = layer.property(name);
    } catch (e) {
        return null;
    }
    if (!p || !p.numKeys) { return null; }
    var keys = [];
    for (var k = 1; k <= p.numKeys; k++) {
        keys.push({
            time: p.keyTime(k),
            value: p.keyValue(k),
            inInterp: String(p.keyInInterpolationType(k)),
            outInterp: String(p.keyOutInterpolationType(k))
        });
    }
    return keys;
}

// Text layers: the styling a caller needs to match an existing caption.
function textDetail(layer) {
    var srcProp;
    try {
        srcProp = layer.property("Source Text");
    } catch (e) {
        return null;
    }
    if (!srcProp) { return null; }
    var doc = srcProp.value;
    if (!doc) { return null; }
    var detail = {
        text: doc.text,
        font: doc.font,
        fontSize: doc.fontSize,
        justification: String(doc.justification)
    };
    // These vary by AE version and throw on some documents; report what we can.
    try { detail.fillColor = doc.applyFill ? doc.fillColor : null; } catch (e) {}
    try { detail.tracking = doc.tracking; } catch (e) {}
    try { detail.leading = doc.autoLeading ? "auto" : doc.leading; } catch (e) {}
    return detail;
}

// Does `list` (an array of strings/numbers) contain `value`? ExtendScript is
// ES3, so Array.prototype.indexOf isn't available.
function listHas(list, value) {
    if (!list) { return false; }
    for (var i = 0; i < list.length; i++) {
        if (list[i] === value) { return true; }
    }
    return false;
}

// Report layers of a composition.
//
// With no arguments this returns the lean summary it always did (every layer:
// position and timing), which keeps output manageable for a 100+ layer comp.
// Pass `detail: true` for scale/anchor/source size/text styling, and
// `keyframes: true` to dump animated properties. Narrow the set with
// `indices` or `names` so detail can be requested without dumping the comp.
function getLayerInfo(args) {
    args = args || {};
    var comp = findCompByName(args.compName);
    if (!comp) {
        return JSON.stringify({ error: "No active composition" }, null, 2);
    }

    var wantDetail = args.detail === true;
    var wantKeys = args.keyframes === true;
    var indices = args.indices || null;
    var names = args.names || null;

    var result = { comp: comp.name, layers: [] };

    for (var i = 1; i <= comp.numLayers; i++) {
        var layer = comp.layer(i);
        if (indices && !listHas(indices, layer.index)) { continue; }
        if (names && !listHas(names, layer.name)) { continue; }

        var layerInfo = {
            index: layer.index,
            name: layer.name,
            enabled: layer.enabled,
            locked: layer.locked,
            threeDLayer: layer.threeDLayer,
            position: safePropValue(layer, "Position"),
            inPoint: layer.inPoint,
            outPoint: layer.outPoint
        };

        if (wantDetail) {
            layerInfo.startTime = layer.startTime;
            layerInfo.anchorPoint = safePropValue(layer, "Anchor Point");
            layerInfo.scale = safePropValue(layer, "Scale");
            layerInfo.rotation = safePropValue(layer, "Rotation");
            layerInfo.opacity = safePropValue(layer, "Opacity");

            // Rendered size at the layer's in point — the number that actually
            // matters when matching one element's on-screen size to another's.
            try {
                var rect = layer.sourceRectAtTime(layer.inPoint, false);
                layerInfo.sourceRect = {
                    top: rect.top, left: rect.left,
                    width: rect.width, height: rect.height
                };
            } catch (e) {}

            if (layer.source) {
                layerInfo.sourceName = layer.source.name;
                if (layer.source.width) {
                    layerInfo.sourceSize = [layer.source.width, layer.source.height];
                }
            }

            var text = textDetail(layer);
            if (text) { layerInfo.textStyle = text; }
        }

        if (wantKeys) {
            var animated = {};
            var found = false;
            for (var t = 0; t < LAYER_TRANSFORM_PROPS.length; t++) {
                var keys = propKeyframes(layer, LAYER_TRANSFORM_PROPS[t]);
                if (keys) { animated[LAYER_TRANSFORM_PROPS[t]] = keys; found = true; }
            }
            var srcKeys = propKeyframes(layer, "Source Text");
            if (srcKeys) { animated["Source Text"] = srcKeys; found = true; }
            if (found) { layerInfo.keyframes = animated; }
        }

        result.layers.push(layerInfo);
    }

    return JSON.stringify(result, null, 2);
}

// --- HTTP bridge transport (Cosmonic Desktop) ---------------------------------
// Instead of exchanging files in ~/Documents/ae-mcp-bridge, this panel polls
// the ae-mcp workload running on Cosmonic Desktop over HTTP and posts results
// back. Requires "Allow Scripts to Write Files and Access Network" in
// After Effects' Scripting & Expressions preferences.

var BRIDGE_SERVER = "127.0.0.1";
var BRIDGE_PORT = 8200;
var BRIDGE_HOST_HEADER = "ae-mcp.localhost.cosmonic.sh";

// Identifies this panel instance. Reloading the panel (e.g. via DoScript while
// iterating) leaves the previous instance's scheduled task polling; the server
// serves commands only to the highest client id it has seen, so the newest
// panel takes over instead of the two racing for commands.
var CLIENT_ID = new Date().getTime();

var connectionText = panel.add("statictext", undefined, "Server: not yet contacted");
connectionText.alignment = ["fill", "top"];

function utf8ByteLength(str) {
    return unescape(encodeURIComponent(str)).length;
}

// Split an HTTP response into its body.
//
// ExtendScript's Socket.read() strips carriage returns, so the response
// arrives LF-only and the canonical "\r\n\r\n" header separator is never
// present. Try every blank-line spelling, then fall back to the JSON payload
// itself, so this works regardless of how the runtime translates newlines.
function extractBody(response) {
    var separators = ["\r\n\r\n", "\n\n", "\r\r"];
    for (var i = 0; i < separators.length; i++) {
        var at = response.indexOf(separators[i]);
        if (at >= 0) {
            return response.substring(at + separators[i].length);
        }
    }
    var brace = response.indexOf("{");
    var close = response.lastIndexOf("}");
    if (brace >= 0 && close > brace) {
        return response.substring(brace, close + 1);
    }
    return null;
}

// Minimal HTTP/1.0 client over an ExtendScript Socket. Returns the response
// body as a string, or null if the request failed.
function httpRequest(method, path, body) {
    var conn = new Socket();
    conn.timeout = 5;
    if (!conn.open(BRIDGE_SERVER + ":" + BRIDGE_PORT, "UTF-8")) {
        return null;
    }
    try {
        var req = method + " " + path + " HTTP/1.0\r\n" +
                  "Host: " + BRIDGE_HOST_HEADER + "\r\n" +
                  "Connection: close\r\n";
        if (body !== null && body !== undefined) {
            req += "Content-Type: application/json\r\n" +
                   "Content-Length: " + utf8ByteLength(body) + "\r\n";
        }
        req += "\r\n";
        if (body !== null && body !== undefined) {
            req += body;
        }
        conn.write(req);

        var response = "";
        while (conn.connected && !conn.eof) {
            var chunk = conn.read(65536);
            if (chunk === null || chunk === "") { break; }
            response += chunk;
        }
        conn.close();

        return extractBody(response);
    } catch (e) {
        try { conn.close(); } catch (ignored) {}
        return null;
    }
}

// Run many commands in a single round trip, inside one undo group.
//
// The bridge costs one ~2s poll cycle per command, so building anything of size
// one command at a time is dominated by round-trip latency. Batching collapses
// that to a single cycle. Results are summarised (status only, plus the message
// on failure) to keep the response small.
function runBatch(args) {
    var cmds = args.commands || [];
    var stopOnError = args.continueOnError !== true;
    var results = [];
    var failed = 0;
    app.beginUndoGroup(args.undoGroup || "MCP batch");
    try {
        for (var i = 0; i < cmds.length; i++) {
            var entry = cmds[i] || {};
            var raw = executeCommand(entry.command, entry.args || {});
            var parsed;
            try { parsed = JSON.parse(raw); } catch (e) { parsed = { status: "error", message: String(raw) }; }
            var ok = parsed.status !== "error" && parsed.success !== false;
            var summary = { i: i, command: entry.command, status: ok ? "ok" : "error" };
            if (!ok) {
                failed++;
                summary.message = parsed.message || "unknown error";
            }
            results.push(summary);
            if (!ok && stopOnError) { break; }
        }
    } catch (fatal) {
        app.endUndoGroup();
        return JSON.stringify({
            status: "error",
            message: "batch aborted: " + fatal.toString(),
            completed: results.length,
            results: results
        });
    }
    app.endUndoGroup();
    return JSON.stringify({
        status: failed > 0 ? "error" : "success",
        requested: cmds.length,
        completed: results.length,
        failed: failed,
        results: results
    });
}

// Import an image (reusing it if already imported) and add it as a layer.
// Size with an explicit scale, or by target height/width in composition pixels.
function addImageLayer(args) {
    try {
        var comp = findCompByName(args.compName);
        if (!comp) { return JSON.stringify({ status: "error", message: "no composition found" }); }
        if (!args.path) { return JSON.stringify({ status: "error", message: "'path' is required" }); }
        var file = new File(args.path);
        if (!file.exists) {
            return JSON.stringify({ status: "error", message: "file not found: " + args.path });
        }
        var item = null;
        for (var i = 1; i <= app.project.numItems; i++) {
            var it = app.project.item(i);
            if (it instanceof FootageItem && it.file && it.file.fsName === file.fsName) { item = it; break; }
        }
        if (!item) {
            item = app.project.importFile(new ImportOptions(file));
        }
        var layer = comp.layers.add(item);
        if (args.name) { layer.name = args.name; }
        if (args.position) { layer.property("Position").setValue(args.position); }
        var s = null;
        if (args.scale) { s = args.scale; }
        else if (args.height) { var sh = (parseFloat(args.height) / item.height) * 100; s = [sh, sh]; }
        else if (args.width) { var sw = (parseFloat(args.width) / item.width) * 100; s = [sw, sw]; }
        if (s) { layer.property("Scale").setValue(s); }
        if (args.opacity !== undefined && args.opacity !== null) {
            layer.property("Opacity").setValue(parseFloat(args.opacity));
        }
        var start = parseFloat(args.startTime) || 0;
        layer.startTime = start;
        if (args.duration && parseFloat(args.duration) > 0) {
            layer.outPoint = start + parseFloat(args.duration);
        }
        return JSON.stringify({
            status: "success",
            layer: {
                name: layer.name, index: layer.index,
                sourceWidth: item.width, sourceHeight: item.height,
                scale: layer.property("Scale").value,
                position: layer.property("Position").value
            }
        });
    } catch (e) {
        return JSON.stringify({ status: "error", message: e.toString() });
    }
}

// Export a single frame as a PNG, for visual verification.
function saveFramePng(args) {
    try {
        var comp = findCompByName(args.compName);
        if (!comp) { return JSON.stringify({ status: "error", message: "no composition found" }); }
        if (!args.path) { return JSON.stringify({ status: "error", message: "'path' is required" }); }
        var file = new File(args.path);
        if (file.exists && args.overwrite !== true) {
            return JSON.stringify({
                status: "error",
                message: "refusing to overwrite existing file (pass overwrite: true): " + file.fsName
            });
        }
        comp.saveFrameToPng(parseFloat(args.time) || 0, file);
        return JSON.stringify({
            status: "success", path: file.fsName,
            time: parseFloat(args.time) || 0, composition: comp.name
        });
    } catch (e) {
        return JSON.stringify({ status: "error", message: e.toString() });
    }
}

// Save the project. With no path, saves in place (requires a saved project).
function saveProject(args) {
    try {
        if (args && args.path) {
            var file = new File(args.path);
            if (file.exists && args.overwrite !== true) {
                return JSON.stringify({
                    status: "error",
                    message: "refusing to overwrite existing project (pass overwrite: true): " + file.fsName
                });
            }
            app.project.save(file);
        } else {
            if (!app.project.file) {
                return JSON.stringify({ status: "error", message: "project has never been saved; supply 'path'" });
            }
            app.project.save();
        }
        return JSON.stringify({ status: "success", path: app.project.file.fsName });
    } catch (e) {
        return JSON.stringify({ status: "error", message: e.toString() });
    }
}

// Delete a composition by name (used to rebuild a scene from scratch).
function deleteComposition(args) {
    try {
        var removed = 0;
        for (var i = app.project.numItems; i >= 1; i--) {
            var it = app.project.item(i);
            if (it instanceof CompItem && it.name === args.compName) {
                it.remove();
                removed++;
            }
        }
        return JSON.stringify({ status: "success", removed: removed, compName: args.compName });
    } catch (e) {
        return JSON.stringify({ status: "error", message: e.toString() });
    }
}

// Find a comp by name, falling back to the active composition.
function findCompByName(compName) {
    if (compName) {
        for (var i = 1; i <= app.project.numItems; i++) {
            var it = app.project.item(i);
            if (it instanceof CompItem && it.name === compName) { return it; }
        }
    }
    if (app.project.activeItem instanceof CompItem) { return app.project.activeItem; }
    for (var j = 1; j <= app.project.numItems; j++) {
        if (app.project.item(j) instanceof CompItem) { return app.project.item(j); }
    }
    return null;
}

// Log message to panel
function logToPanel(message) {
    var timestamp = new Date().toLocaleTimeString();
    logText.text = timestamp + ": " + message + "\n" + logText.text;
}

// Execute a command and return the result JSON string
// Depth > 0 means we're inside runBatch: skip the per-command UI refresh, which
// would otherwise cost a full redraw for every command in the batch.
var BATCH_DEPTH = 0;

function executeCommand(command, args) {
    var result = "";
    if (command === "runBatch") { BATCH_DEPTH++; }
    if (BATCH_DEPTH === 0) {
        statusText.text = "Running: " + command;
        panel.update();
    }
    try {
        switch (command) {
            case "getProjectInfo": result = getProjectInfo(); break;
            case "listCompositions": result = listCompositions(); break;
            case "getLayerInfo": result = getLayerInfo(args); break;
            case "createComposition": result = createComposition(args); break;
            case "createTextLayer": result = createTextLayer(args); break;
            case "createShapeLayer": result = createShapeLayer(args); break;
            case "createSolidLayer": result = createSolidLayer(args); break;
            case "setLayerProperties": result = setLayerProperties(args); break;
            case "setLayerKeyframe":
                result = setLayerKeyframe(args.compIndex, args.layerIndex, args.propertyName, args.timeInSeconds, args.value);
                break;
            case "setLayerExpression":
                result = setLayerExpression(args.compIndex, args.layerIndex, args.propertyName, args.expressionString, args.compName);
                break;
            case "applyEffect": result = applyEffect(args); break;
            case "applyEffectTemplate": result = applyEffectTemplate(args); break;
            case "bridgeTestEffects": result = bridgeTestEffects(args); break;
            case "createCamera": result = createCamera(args); break;
            case "batchSetLayerProperties": result = batchSetLayerProperties(args); break;
            case "setCompositionProperties": result = setCompositionProperties(args); break;
            case "duplicateLayer": result = duplicateLayer(args); break;
            case "deleteLayer": result = deleteLayer(args); break;
            case "setLayerMask": result = setLayerMask(args); break;
            case "runBatch": result = runBatch(args); break;
            case "addImageLayer": result = addImageLayer(args); break;
            case "saveFramePng": result = saveFramePng(args); break;
            case "saveProject": result = saveProject(args); break;
            case "deleteComposition": result = deleteComposition(args); break;
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
        if (BATCH_DEPTH === 0) { statusText.text = "Completed: " + command; }
        return resultString;
    } catch (error) {
        if (command === "runBatch") { BATCH_DEPTH--; }
        var errorMsg = "ERROR in '" + command + "': " + error.toString() + (error.line ? " (line " + error.line + ")" : "");
        logToPanel(errorMsg);
        statusText.text = "Error: " + error.toString();
        return JSON.stringify({
            status: "error",
            command: command,
            message: error.toString(),
            line: error.line
        });
    }
}

// Poll the server for a pending command; execute it and post the result back.
function checkForCommands() {
    if (!autoRunCheckbox.value || isChecking) { return; }
    isChecking = true;
    try {
        // ?v=2 marks this panel as able to parse responses correctly; the
        // server withholds commands from older panels so they can't consume
        // and discard them.
        var responseText = httpRequest("GET", "/bridge/command?v=2&client=" + CLIENT_ID, null);
        if (responseText === null) {
            connectionText.text = "Server: unreachable (is the ae-mcp workload running on Cosmonic Desktop?)";
        } else {
            connectionText.text = "Server: connected (" + BRIDGE_HOST_HEADER + ")";
            var commandData = JSON.parse(responseText);
            if (commandData && commandData.command) {
                logToPanel("Executing command: " + commandData.command + " (id " + commandData.id + ")");
                var resultString = executeCommand(commandData.command, commandData.args || {});
                // Retry: a dropped POST silently loses the command's result and
                // the waiting tool call times out even though the work was done.
                var posted = null;
                for (var attempt = 0; attempt < 3 && posted === null; attempt++) {
                    posted = httpRequest("POST", "/bridge/result?id=" + commandData.id, resultString);
                    if (posted === null) {
                        logToPanel("post attempt " + (attempt + 1) + " failed; retrying");
                    }
                }
                if (posted === null) {
                    logToPanel("WARNING: failed to post result after 3 attempts");
                } else {
                    logToPanel("Result posted for: " + commandData.command);
                }
            }
        }
    } catch (e) {
        logToPanel("Error checking for commands: " + e.toString());
    }
    isChecking = false;
}

// Set up timer to check for commands
function startCommandChecker() {
    app.scheduleTask("checkForCommands()", checkInterval, true);
}

// Add manual check button
var checkButton = panel.add("button", undefined, "Check for Commands Now");
checkButton.onClick = function () {
    logToPanel("Manually checking for commands");
    checkForCommands();
};

// Log startup
logToPanel("MCP Bridge Auto started (HTTP mode)");
logToPanel("Server: " + BRIDGE_SERVER + ":" + BRIDGE_PORT + " (Host: " + BRIDGE_HOST_HEADER + ")");
statusText.text = "Ready - Auto-run is " + (autoRunCheckbox.value ? "ON" : "OFF");

// Start the command checker
startCommandChecker();

// Show the panel
panel.center();
panel.show();
