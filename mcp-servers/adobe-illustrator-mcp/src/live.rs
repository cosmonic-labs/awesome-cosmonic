//! Live Adobe Illustrator control.
//!
//! Illustrator has no remote API, so this drives it the only way a sandboxed
//! component can: a bridge runs *inside* Illustrator and polls this component
//! every ~2 seconds. A tool call queues one command in the shared store
//! ([`crate::state`]), the bridge claims it, executes it against the
//! ExtendScript DOM, and POSTs the result back.
//!
//! Two bridge vehicles ship with this server, both speaking the same
//! protocol and the same command library (`bridge/commands.jsx`):
//!
//! - the **CEP panel** (`bridge/cep/`, installed by `./install-bridge.sh`) —
//!   a persistent Window > Extensions panel that polls on a timer, exactly
//!   like the After Effects reference panel;
//! - the **pump script** (`/bridge/pump.jsx`) — a zero-install fallback run
//!   from File > Scripts (or AppleScript `do javascript`) that drains queued
//!   commands for about two minutes per run, then exits.
//!
//! Two consequences shape every tool here:
//!
//! - **Every call costs at least one poll cycle.** Building art one tool call
//!   at a time is dominated by that latency, which is what [`run_batch`]
//!   exists to avoid.
//! - **The bridge may not be there.** Not-connected is reported as a tool
//!   error with the specific fix, because the command stays queued and nothing
//!   will happen until a human (or a pump run) shows up. That is more useful
//!   to a caller than a success that silently did nothing.
//!
//! Coordinates everywhere: points from the ACTIVE ARTBOARD's top-left corner,
//! y increasing downward (screen convention — the command library converts to
//! Illustrator's y-up space). Colors: CSS hex strings, or `"none"`.
//!
//! [`run_batch`]: IllustratorServer::run_batch

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, ErrorData};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::IllustratorServer;
use crate::state;

/// How long a tool call waits for the bridge before telling the caller to
/// poll [`get_results`]. The bridge checks in every ~2 seconds.
///
/// [`get_results`]: IllustratorServer::get_results
const RESULT_WAIT_MS: u64 = 12_000;

/// Batches, file I/O, and raw scripts do real work in Illustrator and
/// legitimately outrun the single-command budget.
const SLOW_RESULT_WAIT_MS: u64 = 240_000;

/// Commands that get [`SLOW_RESULT_WAIT_MS`] instead of [`RESULT_WAIT_MS`].
const SLOW_COMMANDS: &[&str] = &[
    "runBatch",
    "exportDocument",
    "openDocument",
    "saveDocument",
    "placeImage",
    "runJsx",
];

/// A bridge that has not polled for this long is treated as gone.
const PANEL_SILENT_MS: u64 = 30_000;

/// A bridge that polled within this window counts as connected for
/// [`bridge_status`](IllustratorServer::bridge_status).
const PANEL_FRESH_MS: u64 = 10_000;

/// Shown when a bridge is polling but we refuse to serve it. Without this the
/// symptom is identical to no bridge running, and the obvious fix (open the
/// panel) is the one thing that won't help.
const STALE_PANEL_HINT: &str = "An outdated bridge is polling and cannot be served commands. \
     Run ./install-bridge.sh again, then reopen the Illustrator MCP Bridge \
     panel (or rerun the pump script from /bridge/pump.jsx).";

/// Every command the bridge knows how to execute.
///
/// Each variant has a dedicated tool; this enum exists so [`run_script`] and
/// [`run_batch`] can name commands with the same validation the tools get,
/// and so a new bridge script is reachable before it has a tool of its own.
///
/// [`run_script`]: IllustratorServer::run_script
/// [`run_batch`]: IllustratorServer::run_batch
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PanelScript {
    GetDocumentInfo,
    ListDocuments,
    NewDocument,
    OpenDocument,
    SaveDocument,
    CloseDocument,
    ExportDocument,
    PlaceImage,
    ListArtboards,
    AddArtboard,
    SetActiveArtboard,
    AddLayer,
    ListLayers,
    SetLayer,
    DeleteLayer,
    DrawRectangle,
    DrawEllipse,
    DrawLine,
    DrawPolygon,
    DrawStar,
    AddText,
    SetTextFrame,
    ListTextFrames,
    ListPageItems,
    SelectAll,
    DeselectAll,
    SelectByName,
    GetSelection,
    MoveSelection,
    ScaleSelection,
    RotateSelection,
    DuplicateSelection,
    DeleteSelection,
    SetFill,
    SetStroke,
    SetOpacity,
    GroupSelection,
    UngroupSelection,
    BringToFront,
    SendToBack,
    ListSwatches,
    ListFonts,
    Undo,
    Redo,
    RunBatch,
    RunJsx,
}

impl PanelScript {
    /// The name the bridge's dispatch table switches on. Kept in sync with
    /// the `camelCase` serde renaming above, which is what goes over the wire
    /// when a script is named inside `run_batch`.
    fn as_command(self) -> &'static str {
        match self {
            Self::GetDocumentInfo => "getDocumentInfo",
            Self::ListDocuments => "listDocuments",
            Self::NewDocument => "newDocument",
            Self::OpenDocument => "openDocument",
            Self::SaveDocument => "saveDocument",
            Self::CloseDocument => "closeDocument",
            Self::ExportDocument => "exportDocument",
            Self::PlaceImage => "placeImage",
            Self::ListArtboards => "listArtboards",
            Self::AddArtboard => "addArtboard",
            Self::SetActiveArtboard => "setActiveArtboard",
            Self::AddLayer => "addLayer",
            Self::ListLayers => "listLayers",
            Self::SetLayer => "setLayer",
            Self::DeleteLayer => "deleteLayer",
            Self::DrawRectangle => "drawRectangle",
            Self::DrawEllipse => "drawEllipse",
            Self::DrawLine => "drawLine",
            Self::DrawPolygon => "drawPolygon",
            Self::DrawStar => "drawStar",
            Self::AddText => "addText",
            Self::SetTextFrame => "setTextFrame",
            Self::ListTextFrames => "listTextFrames",
            Self::ListPageItems => "listPageItems",
            Self::SelectAll => "selectAll",
            Self::DeselectAll => "deselectAll",
            Self::SelectByName => "selectByName",
            Self::GetSelection => "getSelection",
            Self::MoveSelection => "moveSelection",
            Self::ScaleSelection => "scaleSelection",
            Self::RotateSelection => "rotateSelection",
            Self::DuplicateSelection => "duplicateSelection",
            Self::DeleteSelection => "deleteSelection",
            Self::SetFill => "setFill",
            Self::SetStroke => "setStroke",
            Self::SetOpacity => "setOpacity",
            Self::GroupSelection => "groupSelection",
            Self::UngroupSelection => "ungroupSelection",
            Self::BringToFront => "bringToFront",
            Self::SendToBack => "sendToBack",
            Self::ListSwatches => "listSwatches",
            Self::ListFonts => "listFonts",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::RunBatch => "runBatch",
            Self::RunJsx => "runJsx",
        }
    }
}

// ---------------------------------------------------------------------------
// Parameter types
//
// Field names are camelCase because that is what the ExtendScript bridge reads
// off the wire; the structs are serialized straight into the queued command.
// `Option` + `skip_serializing_if` matters: the bridge distinguishes "absent"
// (leave alone / use the default) from any concrete value.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunScriptParams {
    /// Bridge script to run.
    pub script: PanelScript,
    /// Arguments for the script, shaped as that script's dedicated tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunJsxParams {
    /// Raw ExtendScript (JSX) source to evaluate inside Illustrator. The
    /// value of the last expression is returned as a string (objects are
    /// JSON-stringified where possible).
    pub script: String,
}

/// Document color mode for [`new_document`](IllustratorServer::new_document).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    Rgb,
    Cmyk,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewDocumentParams {
    /// Artboard width in points (default 1920).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// Artboard height in points (default 1080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    /// Color mode (default rgb).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_mode: Option<ColorMode>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenDocumentParams {
    /// Absolute path to a .ai / .svg / .pdf / .eps file on this machine.
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SaveDocumentParams {
    /// Absolute `.ai` path for save-as. Omit to save in place (the document
    /// must already have a path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Allow replacing an existing file (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloseDocumentParams {
    /// Save before closing (default false — changes are discarded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExportDocumentParams {
    /// Absolute output path; the extension picks the format:
    /// `.svg`, `.png`, `.jpg`, or `.pdf`.
    pub path: String,
    /// Resolution scale percent for raster exports (100 = 72 ppi, 300 ≈ 216
    /// ppi). Default 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// JPEG quality 0-100 (default 85; .jpg only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg_quality: Option<f64>,
    /// Clip the export to the active artboard (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artboard_clipping: Option<bool>,
    /// Allow replacing an existing file (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceImageParams {
    /// Absolute path to the image file (PNG/JPEG/TIFF/PSD/PDF).
    pub path: String,
    /// X of the image's top-left corner, points from the artboard top-left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    /// Y of the image's top-left corner, points from the artboard top-left
    /// (y increases downward).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    /// Target width in points (keeps aspect if height is omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// Target height in points (keeps aspect if width is omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    /// Name for the placed item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Embed the image into the document instead of linking it (default
    /// false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed: Option<bool>,
    /// Layer to place onto (default: the active layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddArtboardParams {
    /// X offset of the new artboard's left edge from the ACTIVE artboard's
    /// top-left corner, in points.
    pub x: f64,
    /// Y offset of the new artboard's top edge from the ACTIVE artboard's
    /// top-left corner, in points (y increases downward).
    pub y: f64,
    /// Artboard width in points.
    pub width: f64,
    /// Artboard height in points.
    pub height: f64,
    /// Name for the artboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetActiveArtboardParams {
    /// 0-based artboard index (see `list_artboards`).
    pub index: i64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddLayerParams {
    /// Name for the new layer. It becomes the active layer.
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerParams {
    /// Layer to modify, by name.
    pub name: String,
    /// Show or hide the layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// Lock or unlock the layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// Make it the active layer (new objects land on it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    /// Rename the layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteLayerParams {
    /// Layer to delete, by name. Its contents are deleted with it.
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrawRectangleParams {
    /// X of the top-left corner, points from the artboard top-left.
    pub x: f64,
    /// Y of the top-left corner, points from the artboard top-left
    /// (y increases downward).
    pub y: f64,
    /// Width in points.
    pub width: f64,
    /// Height in points.
    pub height: f64,
    /// CSS hex fill color (e.g. "#ff5733") or "none" (default "#000000").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// CSS hex stroke color or "none" (default none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<String>,
    /// Stroke width in points (default 1 when a stroke color is given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Corner radius in points for a rounded rectangle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner_radius: Option<f64>,
    /// Opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// Name for the new item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer to draw onto (default: the active layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrawEllipseParams {
    /// X of the top-left bounding corner, points from the artboard top-left.
    pub x: f64,
    /// Y of the top-left bounding corner, points from the artboard top-left
    /// (y increases downward).
    pub y: f64,
    /// Width (horizontal diameter) in points.
    pub width: f64,
    /// Height (vertical diameter) in points.
    pub height: f64,
    /// CSS hex fill color or "none" (default "#000000").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// CSS hex stroke color or "none" (default none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<String>,
    /// Stroke width in points (default 1 when a stroke color is given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// Name for the new item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer to draw onto (default: the active layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrawLineParams {
    /// Start X, points from the artboard top-left.
    pub x1: f64,
    /// Start Y, points from the artboard top-left (y increases downward).
    pub y1: f64,
    /// End X.
    pub x2: f64,
    /// End Y.
    pub y2: f64,
    /// CSS hex stroke color (default "#000000").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<String>,
    /// Stroke width in points (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Name for the new item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer to draw onto (default: the active layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrawPolygonParams {
    /// Vertices as `[[x, y], ...]`, points from the artboard top-left
    /// (y increases downward). At least 2 points; 3+ for a closed shape.
    pub points: Vec<Vec<f64>>,
    /// Close the path (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<bool>,
    /// CSS hex fill color or "none" (default "#000000" when closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// CSS hex stroke color or "none" (default none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<String>,
    /// Stroke width in points (default 1 when a stroke color is given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// Name for the new item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer to draw onto (default: the active layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrawStarParams {
    /// Center X, points from the artboard top-left.
    pub center_x: f64,
    /// Center Y, points from the artboard top-left (y increases downward).
    pub center_y: f64,
    /// Outer radius in points.
    pub radius: f64,
    /// Inner radius in points (default radius/2). Equal to `radius` is not
    /// allowed by Illustrator — use `sides` on a regular polygon instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner_radius: Option<f64>,
    /// Number of star points (default 5). With `polygon: true` this is the
    /// number of sides instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i64>,
    /// Draw a regular polygon instead of a star (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polygon: Option<bool>,
    /// CSS hex fill color or "none" (default "#000000").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// CSS hex stroke color or "none" (default none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<String>,
    /// Stroke width in points (default 1 when a stroke color is given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// Name for the new item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer to draw onto (default: the active layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

/// Paragraph justification for text tools.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Justification {
    Left,
    Center,
    Right,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddTextParams {
    /// The text content.
    pub content: String,
    /// X of the text's top-left, points from the artboard top-left.
    pub x: f64,
    /// Y of the text's top-left, points from the artboard top-left
    /// (y increases downward).
    pub y: f64,
    /// Font size in points (default 24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    /// PostScript font name, e.g. "Helvetica-Bold", "ArialMT" (default:
    /// Illustrator's current default). `list_fonts` shows what is installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    /// CSS hex text color (default "#000000").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Area-text width in points. When given (with `height`), an area text
    /// frame is created; otherwise point text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// Area-text height in points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    /// Paragraph justification (default left).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<Justification>,
    /// Opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// Name for the new frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer to draw onto (default: the active layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetTextFrameParams {
    /// 0-based text frame index (see `list_text_frames`). Takes precedence
    /// over `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// Text frame name (alternative to `index`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Replace the text content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// New font size in points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    /// New PostScript font name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    /// New CSS hex text color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Move the frame: new X, points from the artboard top-left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    /// Move the frame: new Y, points from the artboard top-left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListPageItemsParams {
    /// Restrict the listing to one layer, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Include colors, stroke, opacity, and text details per item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<bool>,
    /// Maximum items to return (default 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SelectByNameParams {
    /// Item name to select (every page item whose name matches).
    pub name: String,
    /// Add to the current selection instead of replacing it (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveSelectionParams {
    /// Horizontal delta in points (positive = right).
    pub dx: f64,
    /// Vertical delta in points (positive = down).
    pub dy: f64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScaleSelectionParams {
    /// Horizontal scale percent (100 = unchanged).
    pub scale_x: f64,
    /// Vertical scale percent (100 = unchanged).
    pub scale_y: f64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RotateSelectionParams {
    /// Angle in degrees; positive rotates counter-clockwise.
    pub angle: f64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateSelectionParams {
    /// Horizontal offset for the copies in points (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dx: Option<f64>,
    /// Vertical offset for the copies in points, positive = down (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dy: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetFillParams {
    /// CSS hex fill color for all selected objects, or "none" to remove.
    pub color: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetStrokeParams {
    /// CSS hex stroke color, or "none" to remove. Omit to leave the color
    /// unchanged (width-only edits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Stroke width in points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// Dash pattern `[dash, gap, ...]` in points; empty array = solid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dash: Option<Vec<f64>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetOpacityParams {
    /// Opacity 0-100 for all selected objects.
    pub opacity: f64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListFontsParams {
    /// Only fonts whose name contains this substring (case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// Maximum fonts to return (default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

/// One entry in a [`run_batch`](IllustratorServer::run_batch) command list.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchCommand {
    /// Bridge script to run.
    pub command: PanelScript,
    /// Arguments, shaped as that script's dedicated tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunBatchParams {
    /// Commands to run in order. Keep batches to roughly 100.
    pub commands: Vec<BatchCommand>,
    /// Keep going after a failing command (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router(router = live_router, vis = "pub(crate)")]
impl IllustratorServer {
    // --- bridge plumbing ---------------------------------------------------

    /// Reports whether the bridge is polling, and what it is doing.
    #[tool(
        description = "Check whether the Illustrator bridge (CEP panel or pump script) is \
                       connected and polling. Call this first when a command seems to hang."
    )]
    #[tracing::instrument(name = "tool.bridge_status", skip(self))]
    async fn bridge_status(&self) -> Result<CallToolResult, ErrorData> {
        let age = state::last_poll_age_ms().map_err(store_error)?;
        let connected = age.is_some_and(|age| age < PANEL_FRESH_MS);
        let stale_age = state::last_stale_poll_age_ms().map_err(store_error)?;
        let stale_polling = !connected && stale_age.is_some_and(|age| age < PANEL_FRESH_MS);
        Ok(CallToolResult::structured(json!({
            "panelConnected": connected,
            "lastPollAgeMs": age,
            "stalePanelPolling": stale_polling,
            "lastStalePollAgeMs": stale_age,
            "currentCommand": state::current_command().map_err(store_error)?,
            "hint": if connected {
                "Bridge is polling normally.".to_owned()
            } else if stale_polling {
                STALE_PANEL_HINT.to_owned()
            } else {
                NOT_CONNECTED_HINT.to_owned()
            },
        })))
    }

    /// Returns whatever the bridge reported last.
    #[tool(
        description = "Fetch the result of the most recently executed Illustrator command — \
                       use this after a tool call reports that it timed out waiting."
    )]
    #[tracing::instrument(name = "tool.get_results", skip(self))]
    async fn get_results(&self) -> Result<CallToolResult, ErrorData> {
        match state::latest_result().map_err(store_error)? {
            Some(result) => Ok(CallToolResult::structured(result)),
            None => Ok(CallToolResult::structured(json!({
                "status": "no-results",
                "message": "No results yet. Queue a command first, and make sure the \
                            Illustrator bridge (CEP panel or pump script) is running.",
            }))),
        }
    }

    /// Setup and reference material for the whole integration.
    #[tool(
        description = "How to set up and use the Illustrator bridge: install steps for the CEP \
                       panel and the pump script, the coordinate and color conventions, and the \
                       batching guidance that makes scene-building fast."
    )]
    #[tracing::instrument(name = "tool.get_help", skip(self))]
    async fn get_help(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::structured(json!({ "help": HELP_TEXT })))
    }

    /// Escape hatch onto the bridge's dispatch table.
    #[tool(
        description = "Run a bridge script by name with raw arguments. Every script also has a \
                       dedicated, schema-checked tool — prefer those. This exists for a bridge \
                       script that is newer than this component, and for forwarding arguments \
                       built elsewhere."
    )]
    #[tracing::instrument(name = "tool.run_script", skip(self))]
    async fn run_script(
        &self,
        Parameters(params): Parameters<RunScriptParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.script == PanelScript::RunJsx {
            if let Some(denied) = raw_jsx_denied() {
                return Ok(denied);
            }
        }
        let args = params.parameters.unwrap_or_else(|| json!({}));
        queue_and_wait(params.script.as_command(), args).await
    }

    /// Arbitrary ExtendScript, for everything without a dedicated tool.
    #[tool(
        description = "Execute arbitrary ExtendScript (JSX) inside Illustrator — the escape \
                       hatch onto the full scripting DOM for anything without a dedicated tool. \
                       The last expression's value is returned. Requires MCP_ALLOW_RAW_JSX=true \
                       in the workload environment."
    )]
    #[tracing::instrument(name = "tool.run_jsx", skip(self))]
    async fn run_jsx(
        &self,
        Parameters(params): Parameters<RunJsxParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(denied) = raw_jsx_denied() {
            return Ok(denied);
        }
        dispatch("runJsx", &params).await
    }

    // --- reading the document ----------------------------------------------

    #[tool(
        description = "Information about the active Illustrator document: name, size, color \
                       mode, artboards, layers, and object counts."
    )]
    #[tracing::instrument(name = "tool.get_document_info", skip(self))]
    async fn get_document_info(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("getDocumentInfo", json!({})).await
    }

    #[tool(description = "List all open Illustrator documents.")]
    #[tracing::instrument(name = "tool.list_documents", skip(self))]
    async fn list_documents(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("listDocuments", json!({})).await
    }

    #[tool(
        description = "List the page items of the active document (type, name, bounds in \
                       artboard coordinates). Ask for `detail` to get colors, stroke, opacity, \
                       and text styling; narrow with `layer` — a full detailed listing of a \
                       large document is a lot of output."
    )]
    #[tracing::instrument(name = "tool.list_page_items", skip(self))]
    async fn list_page_items(
        &self,
        Parameters(params): Parameters<ListPageItemsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("listPageItems", &params).await
    }

    #[tool(description = "List all text frames with content, position, font, size, and color.")]
    #[tracing::instrument(name = "tool.list_text_frames", skip(self))]
    async fn list_text_frames(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("listTextFrames", json!({})).await
    }

    #[tool(description = "List all artboards with their position and size.")]
    #[tracing::instrument(name = "tool.list_artboards", skip(self))]
    async fn list_artboards(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("listArtboards", json!({})).await
    }

    #[tool(description = "List all layers with visibility and lock state.")]
    #[tracing::instrument(name = "tool.list_layers", skip(self))]
    async fn list_layers(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("listLayers", json!({})).await
    }

    #[tool(description = "List the document's color swatches with their RGB values.")]
    #[tracing::instrument(name = "tool.list_swatches", skip(self))]
    async fn list_swatches(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("listSwatches", json!({})).await
    }

    #[tool(
        description = "List installed fonts by PostScript name. Filter with `contains`; \
                       the full list is thousands of entries."
    )]
    #[tracing::instrument(name = "tool.list_fonts", skip(self))]
    async fn list_fonts(
        &self,
        Parameters(params): Parameters<ListFontsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("listFonts", &params).await
    }

    // --- documents ----------------------------------------------------------

    #[tool(
        description = "Create a new Illustrator document (default 1920x1080 pt, RGB). \
                       It becomes the active document."
    )]
    #[tracing::instrument(name = "tool.new_document", skip(self))]
    async fn new_document(
        &self,
        Parameters(params): Parameters<NewDocumentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("newDocument", &params).await
    }

    #[tool(description = "Open a .ai/.svg/.pdf/.eps file as a document.")]
    #[tracing::instrument(name = "tool.open_document", skip(self))]
    async fn open_document(
        &self,
        Parameters(params): Parameters<OpenDocumentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("openDocument", &params).await
    }

    #[tool(
        description = "Save the active document as native .ai. With no path, saves in place. \
                       Refuses to overwrite an existing file unless `overwrite` is set."
    )]
    #[tracing::instrument(name = "tool.save_document", skip(self))]
    async fn save_document(
        &self,
        Parameters(params): Parameters<SaveDocumentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("saveDocument", &params).await
    }

    #[tool(description = "Close the active document, discarding changes unless `save` is set.")]
    #[tracing::instrument(name = "tool.close_document", skip(self))]
    async fn close_document(
        &self,
        Parameters(params): Parameters<CloseDocumentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("closeDocument", &params).await
    }

    #[tool(
        description = "Export the active document; the path extension picks the format \
                       (.svg/.png/.jpg/.pdf) — the way to visually verify work. Refuses to \
                       overwrite unless `overwrite` is set."
    )]
    #[tracing::instrument(name = "tool.export_document", skip(self))]
    async fn export_document(
        &self,
        Parameters(params): Parameters<ExportDocumentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("exportDocument", &params).await
    }

    #[tool(
        description = "Place an image file (PNG/JPEG/TIFF/PSD/PDF) into the active document, \
                       optionally sized to a target width/height in points. Linked by default; \
                       set `embed` to embed it."
    )]
    #[tracing::instrument(name = "tool.place_image", skip(self))]
    async fn place_image(
        &self,
        Parameters(params): Parameters<PlaceImageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("placeImage", &params).await
    }

    // --- artboards ----------------------------------------------------------

    #[tool(
        description = "Add a new artboard. x/y are offsets from the ACTIVE artboard's top-left \
                       (y down), so x = activeWidth + 100 places it to the right."
    )]
    #[tracing::instrument(name = "tool.add_artboard", skip(self))]
    async fn add_artboard(
        &self,
        Parameters(params): Parameters<AddArtboardParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("addArtboard", &params).await
    }

    #[tool(
        description = "Make an artboard active by 0-based index. Drawing coordinates are \
                       relative to the active artboard."
    )]
    #[tracing::instrument(name = "tool.set_active_artboard", skip(self))]
    async fn set_active_artboard(
        &self,
        Parameters(params): Parameters<SetActiveArtboardParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setActiveArtboard", &params).await
    }

    // --- layers -------------------------------------------------------------

    #[tool(description = "Add a new layer; it becomes the active layer.")]
    #[tracing::instrument(name = "tool.add_layer", skip(self))]
    async fn add_layer(
        &self,
        Parameters(params): Parameters<AddLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("addLayer", &params).await
    }

    #[tool(
        description = "Modify a layer by name: visibility, lock state, rename, or make it the \
                       active layer new objects land on."
    )]
    #[tracing::instrument(name = "tool.set_layer", skip(self))]
    async fn set_layer(
        &self,
        Parameters(params): Parameters<SetLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayer", &params).await
    }

    #[tool(description = "Delete a layer by name, including everything on it.")]
    #[tracing::instrument(name = "tool.delete_layer", skip(self))]
    async fn delete_layer(
        &self,
        Parameters(params): Parameters<DeleteLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("deleteLayer", &params).await
    }

    // --- drawing ------------------------------------------------------------

    #[tool(
        description = "Draw a rectangle (optionally rounded). Coordinates are points from the \
                       active artboard's top-left, y increasing downward."
    )]
    #[tracing::instrument(name = "tool.draw_rectangle", skip(self))]
    async fn draw_rectangle(
        &self,
        Parameters(params): Parameters<DrawRectangleParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("drawRectangle", &params).await
    }

    #[tool(
        description = "Draw an ellipse (or circle) inside the given bounding box. Coordinates \
                       are points from the active artboard's top-left, y increasing downward."
    )]
    #[tracing::instrument(name = "tool.draw_ellipse", skip(self))]
    async fn draw_ellipse(
        &self,
        Parameters(params): Parameters<DrawEllipseParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("drawEllipse", &params).await
    }

    #[tool(description = "Draw a straight line between two points.")]
    #[tracing::instrument(name = "tool.draw_line", skip(self))]
    async fn draw_line(
        &self,
        Parameters(params): Parameters<DrawLineParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("drawLine", &params).await
    }

    #[tool(
        description = "Draw a path from a list of [x, y] vertices — closed polygon by default, \
                       set closed=false for an open polyline."
    )]
    #[tracing::instrument(name = "tool.draw_polygon", skip(self))]
    async fn draw_polygon(
        &self,
        Parameters(params): Parameters<DrawPolygonParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.points.len() < 2 {
            return Err(ErrorData::invalid_params(
                "need at least 2 points".to_owned(),
                None,
            ));
        }
        if params.points.iter().any(|p| p.len() != 2) {
            return Err(ErrorData::invalid_params(
                "every point must be [x, y]".to_owned(),
                None,
            ));
        }
        dispatch("drawPolygon", &params).await
    }

    #[tool(
        description = "Draw a star (or, with polygon=true, a regular polygon) centered at a \
                       point."
    )]
    #[tracing::instrument(name = "tool.draw_star", skip(self))]
    async fn draw_star(
        &self,
        Parameters(params): Parameters<DrawStarParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("drawStar", &params).await
    }

    #[tool(
        description = "Add text: point text by default, or an area text frame when width and \
                       height are given. Fonts are PostScript names (see list_fonts)."
    )]
    #[tracing::instrument(name = "tool.add_text", skip(self))]
    async fn add_text(
        &self,
        Parameters(params): Parameters<AddTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("addText", &params).await
    }

    #[tool(
        description = "Modify an existing text frame by index or name: content, font, size, \
                       color, or position."
    )]
    #[tracing::instrument(name = "tool.set_text_frame", skip(self))]
    async fn set_text_frame(
        &self,
        Parameters(params): Parameters<SetTextFrameParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setTextFrame", &params).await
    }

    // --- selection and transforms -------------------------------------------

    #[tool(description = "Select every object in the active document.")]
    #[tracing::instrument(name = "tool.select_all", skip(self))]
    async fn select_all(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("selectAll", json!({})).await
    }

    #[tool(description = "Clear the selection.")]
    #[tracing::instrument(name = "tool.deselect_all", skip(self))]
    async fn deselect_all(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("deselectAll", json!({})).await
    }

    #[tool(description = "Select page items by name (every item whose name matches).")]
    #[tracing::instrument(name = "tool.select_by_name", skip(self))]
    async fn select_by_name(
        &self,
        Parameters(params): Parameters<SelectByNameParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("selectByName", &params).await
    }

    #[tool(description = "Report the count and types of the currently selected objects.")]
    #[tracing::instrument(name = "tool.get_selection", skip(self))]
    async fn get_selection(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("getSelection", json!({})).await
    }

    #[tool(description = "Move the selected objects by a delta in points (dy positive = down).")]
    #[tracing::instrument(name = "tool.move_selection", skip(self))]
    async fn move_selection(
        &self,
        Parameters(params): Parameters<MoveSelectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("moveSelection", &params).await
    }

    #[tool(description = "Scale the selected objects by percentages (100 = unchanged).")]
    #[tracing::instrument(name = "tool.scale_selection", skip(self))]
    async fn scale_selection(
        &self,
        Parameters(params): Parameters<ScaleSelectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("scaleSelection", &params).await
    }

    #[tool(description = "Rotate the selected objects (degrees, positive = counter-clockwise).")]
    #[tracing::instrument(name = "tool.rotate_selection", skip(self))]
    async fn rotate_selection(
        &self,
        Parameters(params): Parameters<RotateSelectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("rotateSelection", &params).await
    }

    #[tool(description = "Duplicate the selected objects, offset by (dx, dy) points.")]
    #[tracing::instrument(name = "tool.duplicate_selection", skip(self))]
    async fn duplicate_selection(
        &self,
        Parameters(params): Parameters<DuplicateSelectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("duplicateSelection", &params).await
    }

    #[tool(description = "Delete the selected objects.")]
    #[tracing::instrument(name = "tool.delete_selection", skip(self))]
    async fn delete_selection(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("deleteSelection", json!({})).await
    }

    #[tool(description = "Set the fill color of the selected objects (hex, or 'none').")]
    #[tracing::instrument(name = "tool.set_fill", skip(self))]
    async fn set_fill(
        &self,
        Parameters(params): Parameters<SetFillParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setFill", &params).await
    }

    #[tool(
        description = "Set stroke color ('none' removes), width, and/or dash pattern of the \
                       selected objects."
    )]
    #[tracing::instrument(name = "tool.set_stroke", skip(self))]
    async fn set_stroke(
        &self,
        Parameters(params): Parameters<SetStrokeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.color.is_none() && params.width.is_none() && params.dash.is_none() {
            return Err(ErrorData::invalid_params(
                "give at least one of color, width, dash".to_owned(),
                None,
            ));
        }
        dispatch("setStroke", &params).await
    }

    #[tool(description = "Set the opacity (0-100) of the selected objects.")]
    #[tracing::instrument(name = "tool.set_opacity", skip(self))]
    async fn set_opacity(
        &self,
        Parameters(params): Parameters<SetOpacityParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setOpacity", &params).await
    }

    #[tool(description = "Group the selected objects.")]
    #[tracing::instrument(name = "tool.group_selection", skip(self))]
    async fn group_selection(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("groupSelection", json!({})).await
    }

    #[tool(description = "Ungroup the selected groups.")]
    #[tracing::instrument(name = "tool.ungroup_selection", skip(self))]
    async fn ungroup_selection(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("ungroupSelection", json!({})).await
    }

    #[tool(description = "Bring the selected objects to the front of the stacking order.")]
    #[tracing::instrument(name = "tool.bring_to_front", skip(self))]
    async fn bring_to_front(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("bringToFront", json!({})).await
    }

    #[tool(description = "Send the selected objects to the back of the stacking order.")]
    #[tracing::instrument(name = "tool.send_to_back", skip(self))]
    async fn send_to_back(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("sendToBack", json!({})).await
    }

    // --- history ------------------------------------------------------------

    #[tool(description = "Undo the last operation in the active document.")]
    #[tracing::instrument(name = "tool.undo", skip(self))]
    async fn undo(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("undo", json!({})).await
    }

    #[tool(description = "Redo the last undone operation in the active document.")]
    #[tracing::instrument(name = "tool.redo", skip(self))]
    async fn redo(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("redo", json!({})).await
    }

    // --- batching -----------------------------------------------------------

    #[tool(
        description = "Run many commands in one round trip. Strongly preferred when building a \
                       scene: every other tool costs a ~2s poll cycle, so 100 individual calls \
                       take minutes while one batch takes seconds. Keep batches to roughly 100 \
                       commands. Stops at the first error unless continueOnError is set."
    )]
    #[tracing::instrument(name = "tool.run_batch", skip(self))]
    async fn run_batch(
        &self,
        Parameters(params): Parameters<RunBatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params
            .commands
            .iter()
            .any(|entry| entry.command == PanelScript::RunJsx)
        {
            if let Some(denied) = raw_jsx_denied() {
                return Ok(denied);
            }
        }
        dispatch("runBatch", &params).await
    }
}

/// When raw JSX execution is disabled, the tool error explaining how to turn
/// it on; `None` when it is allowed.
fn raw_jsx_denied() -> Option<CallToolResult> {
    let allowed =
        std::env::var("MCP_ALLOW_RAW_JSX").is_ok_and(|v| v == "true" || v == "1");
    (!allowed).then(|| {
        CallToolResult::structured_error(json!({
            "status": "raw-jsx-disabled",
            "message": "Arbitrary ExtendScript execution is disabled. Set \
                        MCP_ALLOW_RAW_JSX=true in the workload environment to enable \
                        run_jsx, or use the dedicated schema-checked tools.",
        }))
    })
}

/// Serializes `params` into the bridge's argument shape and dispatches it.
async fn dispatch<P: Serialize>(command: &str, params: &P) -> Result<CallToolResult, ErrorData> {
    let args = serde_json::to_value(params).map_err(|err| {
        ErrorData::internal_error(format!("failed to encode arguments: {err}"), None)
    })?;
    queue_and_wait(command, args).await
}

const NOT_CONNECTED_HINT: &str = "Bridge not connected. Zero-install (macOS): fetch \
     GET /bridge/shuttle.sh and run it in a terminal (`curl -H 'Host: \
     illustrator-mcp.localhost' http://127.0.0.1:8200/bridge/shuttle.sh -o shuttle.sh && bash \
     shuttle.sh`) — it relays commands into the running Illustrator via AppleScript until \
     Ctrl-C. Persistent: run ./install-bridge.sh, restart Illustrator, open Window > \
     Extensions > Illustrator MCP Bridge. (The /bridge/pump.jsx in-app pump works only on \
     Illustrator 2022 and older — newer versions removed ExtendScript's Socket class.)";

/// Queues one command and waits for the bridge to report back.
///
/// The three non-result outcomes are deliberately distinct, because they have
/// different fixes: no bridge has ever connected (install and open it), a
/// bridge went quiet (open it again / rerun the pump), and a stale bridge is
/// polling but cannot be served (reinstall — reopening will not help).
async fn queue_and_wait(command: &str, args: Value) -> Result<CallToolResult, ErrorData> {
    // A command already in flight explains the silence: the bridge cannot
    // poll while it is executing, and a long batch blocks it for many
    // seconds. Only treat a quiet bridge as "gone" when nothing is running.
    let busy = state::current_command()
        .map_err(store_error)?
        .and_then(|command| {
            command
                .get("status")
                .and_then(Value::as_str)
                .map(|status| status == "dispatched")
        })
        .unwrap_or(false);

    // A stale bridge polling right now explains the silence better than
    // anything else, and needs different advice.
    let stale_polling = state::last_stale_poll_age_ms()
        .map_err(store_error)?
        .is_some_and(|age| age < PANEL_SILENT_MS);

    let poll_age = state::last_poll_age_ms().map_err(store_error)?;
    let unreachable = match poll_age {
        None => Some(if stale_polling {
            STALE_PANEL_HINT.to_owned()
        } else {
            format!("The bridge has never connected. {NOT_CONNECTED_HINT}")
        }),
        Some(age) if age > PANEL_SILENT_MS && !busy => Some(if stale_polling {
            STALE_PANEL_HINT.to_owned()
        } else {
            format!(
                "The bridge has not polled for {}s. {NOT_CONNECTED_HINT}",
                age / 1000
            )
        }),
        Some(_) => None,
    };

    // Queue regardless: the command runs as soon as a bridge shows up, so the
    // caller's work is not lost while they go and open it.
    let id = state::queue_command(command, &args).map_err(store_error)?;

    if let Some(hint) = unreachable {
        tracing::warn!(command, id, "queued command with no bridge listening");
        // A tool error, not a success: nothing happened in Illustrator, and
        // nothing will until a human acts.
        return Ok(CallToolResult::structured_error(json!({
            "status": "queued-not-executed",
            "command": command,
            "commandId": id,
            "message": hint,
            "nextStep": "Once the bridge is running, call get_results to fetch the outcome.",
        })));
    }

    let wait_ms = if SLOW_COMMANDS.contains(&command) {
        SLOW_RESULT_WAIT_MS
    } else {
        RESULT_WAIT_MS
    };
    match state::wait_for_result(id, wait_ms)
        .await
        .map_err(store_error)?
    {
        Some(result) => {
            let failed = result.get("status").and_then(Value::as_str) == Some("error")
                || result.get("success").and_then(Value::as_bool) == Some(false);
            let result = match result {
                Value::Object(_) => result,
                other => json!({ "result": other }),
            };
            if failed {
                Ok(CallToolResult::structured_error(result))
            } else {
                Ok(CallToolResult::structured(result))
            }
        }
        None => {
            tracing::warn!(command, id, wait_ms, "timed out waiting for bridge result");
            Ok(CallToolResult::structured(json!({
                "status": "pending",
                "command": command,
                "commandId": id,
                "message": format!(
                    "No result within {}s. The bridge may still be working on it.",
                    wait_ms / 1000
                ),
                "nextStep": "Call bridge_status to check the connection, then get_results \
                             to fetch the outcome once it lands.",
            })))
        }
    }
}

/// A keyvalue failure is an infrastructure problem, not a bad tool argument,
/// so it becomes a protocol-level error rather than a tool result.
fn store_error(message: String) -> ErrorData {
    tracing::error!(error = %message, "bridge state store failure");
    ErrorData::internal_error(message, None)
}

const HELP_TEXT: &str = r##"# Illustrator MCP bridge

## Setup (choose one)

### A. Zero-install shuttle (macOS; no restart needed)
1. Save `GET /bridge/shuttle.sh`:
   curl -H 'Host: illustrator-mcp.localhost' \
     http://127.0.0.1:8200/bridge/shuttle.sh -o shuttle.sh
2. Run `bash shuttle.sh` in a terminal and leave it running (Ctrl-C stops
   it). It claims queued commands and executes them in the running
   Illustrator via AppleScript — nothing is installed.

### B. Persistent CEP panel (recommended long-term)
1. Run `./install-bridge.sh` from the project directory. It installs the
   "Illustrator MCP Bridge" CEP extension into your user CEP folder and
   enables Adobe's PlayerDebugMode (required for unsigned extensions).
2. Restart Illustrator.
3. Open Window > Extensions > Illustrator MCP Bridge and leave it open;
   it polls this server every ~2 seconds.

### C. In-app pump (Illustrator 2022 and older only)
`GET /bridge/pump.jsx`, then File > Scripts > Other Script… — drains queued
commands for ~2 minutes per run. Illustrator 2023+ removed ExtendScript's
Socket class, so use the shuttle or the CEP panel there.

## How it works
A tool call queues one command; the bridge claims it on its next poll, runs
it in Illustrator, and posts the result back. Most tools wait up to ~12s and
return the result directly (batches, file I/O, and run_jsx wait up to 4
minutes). If a call times out, `bridge_status` checks the bridge and
`get_results` fetches the outcome.

## Conventions
- Coordinates: POINTS from the ACTIVE ARTBOARD's top-left corner, y
  increasing DOWNWARD. The command library converts to Illustrator's y-up
  space, so what you compute from a screenshot or an After Effects comp maps
  1:1.
- Colors: CSS hex strings ("#ff8800") or "none". `hex_to_rgb` / `rgb_to_hex`
  convert.
- Sizes: points everywhere; 1 px (at 72 ppi) == 1 pt. `convert_units` maps
  mm/cm/in/pica.
- Fonts: PostScript names ("Helvetica-Bold", "ArialMT"); `list_fonts
  contains=...` finds them.

## Building scenes quickly
Every tool call costs at least one ~2s poll cycle. Use `run_batch` to send a
whole scene in one round trip — it is the difference between minutes and
seconds. Typical flow: new_document → run_batch [addLayer, drawRectangle...,
addText...] → export_document to PNG to verify.

## Escape hatch
`run_jsx` executes arbitrary ExtendScript against the full Illustrator DOM
(gradients, symbols, live effects, actions...). It is enabled by
MCP_ALLOW_RAW_JSX=true in the workload environment; remove that to lock the
server down to the schema-checked tools.
"##;
