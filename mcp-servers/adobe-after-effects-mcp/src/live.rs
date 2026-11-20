//! Live Adobe After Effects control.
//!
//! After Effects has no remote API, so this drives it the only way a sandboxed
//! component can: the **MCP Bridge Auto** panel runs *inside* After Effects and
//! polls this component every ~2 seconds. A tool call queues one command in the
//! shared store ([`crate::state`]), the panel claims it, executes it against
//! the ExtendScript DOM, and POSTs the result back.
//!
//! Two consequences shape every tool here:
//!
//! - **Every call costs at least one poll cycle.** Building a scene one tool
//!   call at a time is dominated by that latency, which is what [`run_batch`]
//!   exists to avoid.
//! - **The panel may not be there.** Not-connected is reported as a tool error
//!   with the specific fix, because the command stays queued and nothing will
//!   happen until a human (or a panel reopen) shows up. That is more useful to
//!   a caller than a success that silently did nothing.
//!
//! Conventions everywhere: positions are `[x, y]` in composition pixels, y
//! increasing downward, naming the layer's **centre**. Colours are `[r, g, b]`
//! floats in 0..1 (the exception is a composition's `backgroundColor`, which
//! After Effects takes as 0-255 integers). Times are seconds, not frames.
//!
//! Tool names are hyphenated (`create-shape-layer`) and pinned with
//! `#[tool(name = …)]`: they are the stable public surface, and existing client
//! config and scripts address them by those names.
//!
//! [`run_batch`]: AfterEffectsServer::run_batch

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, ErrorData};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::AfterEffectsServer;
use crate::state;

/// How long a tool call waits for the panel before telling the caller to poll
/// [`get_results`]. The panel checks in every ~2 seconds.
///
/// [`get_results`]: AfterEffectsServer::get_results
const RESULT_WAIT_MS: u64 = 12_000;

/// Batches and frame exports do real work in After Effects and legitimately
/// outrun the single-command budget. Large batches get slower as a project
/// accumulates expression-driven layers, so this budget is generous.
const SLOW_RESULT_WAIT_MS: u64 = 240_000;

/// Commands that get [`SLOW_RESULT_WAIT_MS`] instead of [`RESULT_WAIT_MS`].
const SLOW_COMMANDS: &[&str] = &["runBatch", "saveFramePng", "saveProject"];

/// A panel that has not polled for this long is treated as gone.
const PANEL_SILENT_MS: u64 = 30_000;

/// A panel that polled within this window counts as connected for
/// [`bridge_status`](AfterEffectsServer::bridge_status).
const PANEL_FRESH_MS: u64 = 10_000;

/// Shown when a panel is polling but we refuse to serve it. Without this the
/// symptom is identical to no panel running, and the obvious fix (open the
/// panel) is the one thing that won't help.
const STALE_PANEL_HINT: &str = "An outdated bridge panel is polling and cannot be served \
     commands. Run ./install-bridge.sh, then close and reopen Window > \
     mcp-bridge-auto.jsx in After Effects.";

/// Shown when no panel is polling at all.
const NOT_CONNECTED_HINT: &str = "Panel not connected. Run ./install-bridge.sh, enable \
     Settings > Scripting & Expressions > \"Allow Scripts to Write Files and Access \
     Network\", restart After Effects, then open Window > mcp-bridge-auto.jsx and leave \
     it open — it polls this server every ~2 seconds.";

// ---------------------------------------------------------------------------
// Shared argument types
// ---------------------------------------------------------------------------

/// Every command the bridge panel knows how to execute.
///
/// Most variants have a dedicated tool; this enum exists so [`run_script`] and
/// [`run_batch`] can name commands with the same validation the tools get, and
/// so a new bridge script is reachable before it has a tool of its own. It is
/// also the allow-list: a name that is not a variant is rejected before
/// anything is queued.
///
/// [`run_script`]: AfterEffectsServer::run_script
/// [`run_batch`]: AfterEffectsServer::run_batch
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PanelScript {
    GetProjectInfo,
    ListCompositions,
    GetLayerInfo,
    CreateComposition,
    CreateTextLayer,
    CreateShapeLayer,
    CreateSolidLayer,
    SetLayerProperties,
    SetLayerKeyframe,
    SetLayerExpression,
    ApplyEffect,
    ApplyEffectTemplate,
    BridgeTestEffects,
    CreateCamera,
    BatchSetLayerProperties,
    SetCompositionProperties,
    DuplicateLayer,
    DeleteLayer,
    SetLayerMask,
    RunBatch,
    AddImageLayer,
    SaveFramePng,
    SaveProject,
    DeleteComposition,
}

impl PanelScript {
    /// The name the panel's dispatch table switches on. Kept in sync with the
    /// `camelCase` serde renaming above, which is what goes over the wire when
    /// a script is named inside `run-batch`.
    fn as_command(self) -> &'static str {
        match self {
            Self::GetProjectInfo => "getProjectInfo",
            Self::ListCompositions => "listCompositions",
            Self::GetLayerInfo => "getLayerInfo",
            Self::CreateComposition => "createComposition",
            Self::CreateTextLayer => "createTextLayer",
            Self::CreateShapeLayer => "createShapeLayer",
            Self::CreateSolidLayer => "createSolidLayer",
            Self::SetLayerProperties => "setLayerProperties",
            Self::SetLayerKeyframe => "setLayerKeyframe",
            Self::SetLayerExpression => "setLayerExpression",
            Self::ApplyEffect => "applyEffect",
            Self::ApplyEffectTemplate => "applyEffectTemplate",
            Self::BridgeTestEffects => "bridgeTestEffects",
            Self::CreateCamera => "createCamera",
            Self::BatchSetLayerProperties => "batchSetLayerProperties",
            Self::SetCompositionProperties => "setCompositionProperties",
            Self::DuplicateLayer => "duplicateLayer",
            Self::DeleteLayer => "deleteLayer",
            Self::SetLayerMask => "setLayerMask",
            Self::RunBatch => "runBatch",
            Self::AddImageLayer => "addImageLayer",
            Self::SaveFramePng => "saveFramePng",
            Self::SaveProject => "saveProject",
            Self::DeleteComposition => "deleteComposition",
        }
    }
}

/// A composition background colour: RGB 0-255 integers.
///
/// The odd one out — every *layer* colour in this server is `[r, g, b]` floats
/// in 0..1. After Effects' composition background takes bytes, and the bridge
/// scripts pass it through unchanged.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct BackgroundColor {
    /// Red 0-255.
    pub r: u16,
    /// Green 0-255.
    pub g: u16,
    /// Blue 0-255.
    pub b: u16,
}

/// The shapes `create-shape-layer` can draw.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ShapeType {
    Rectangle,
    Ellipse,
    Polygon,
    Star,
}

/// Paragraph justification for a text layer.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Alignment {
    Left,
    Center,
    Right,
}

/// The presets `apply-effect-template` understands.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EffectTemplate {
    GaussianBlur,
    DirectionalBlur,
    ColorBalance,
    BrightnessContrast,
    Curves,
    Glow,
    DropShadow,
    CinematicLook,
    TextPop,
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunScriptParams {
    /// Bridge script to run.
    pub script: PanelScript,
    /// Arguments, shaped as that script's dedicated tool takes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateCompositionParams {
    /// Composition name.
    pub name: String,
    /// Width in pixels (default 1920).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    /// Height in pixels (default 1080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// Pixel aspect ratio (default 1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_aspect: Option<f64>,
    /// Duration in seconds (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Frames per second (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    /// Preview backdrop, RGB 0-255. It never renders into the alpha channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_color: Option<BackgroundColor>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetCompositionPropertiesParams {
    /// Composition name (defaults to the active composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Frames per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    /// Width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    /// Height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// Preview backdrop, RGB 0-255. Use black for comps meant to be composited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_color: Option<BackgroundColor>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteCompositionParams {
    /// Name of the composition(s) to delete.
    pub comp_name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateTextLayerParams {
    /// The text content.
    pub text: String,
    /// Composition name (defaults to the active composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// `[x, y]` baseline position in composition pixels (default `[960, 540]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// Font size (default 72).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    /// `[r, g, b]`, each 0..1 (default white).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Vec<f64>>,
    /// Font family (default Arial). It must resolve on this machine — After
    /// Effects substitutes silently when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Paragraph justification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment: Option<Alignment>,
    /// Layer start time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    /// Layer duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateShapeLayerParams {
    /// Shape to draw.
    pub shape_type: ShapeType,
    /// Composition name (defaults to the active composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// `[x, y]` **centre** position in composition pixels. Converting from a
    /// design tool's top-left box: `[x + width / 2, y + height / 2]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// `[width, height]` in composition pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Vec<f64>>,
    /// `[r, g, b]`, each 0..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_color: Option<Vec<f64>>,
    /// `[r, g, b]`, each 0..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_color: Option<Vec<f64>>,
    /// Stroke width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f64>,
    /// Stroke opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_opacity: Option<f64>,
    /// `[dashLength, gapLength]` for a dashed or dotted outline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dash: Option<Vec<f64>>,
    /// Corner radius for rectangles, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roundness: Option<f64>,
    /// Fill opacity 0-100 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_opacity: Option<f64>,
    /// Omit the fill entirely (outline-only shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_none: Option<bool>,
    /// Point count for polygon/star (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i64>,
    /// Layer name. Supply it — layer names are the handle for every later edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer start time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    /// Layer duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateSolidLayerParams {
    /// Composition name (defaults to the active composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// `[r, g, b]`, each 0..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Vec<f64>>,
    /// Width in pixels (defaults to the composition width).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    /// Height in pixels (defaults to the composition height).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// Make it an adjustment layer rather than a solid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_adjustment: Option<bool>,
    /// Layer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Layer start time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    /// Layer duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddImageLayerParams {
    /// Absolute path to the image file.
    pub path: String,
    /// Composition name (defaults to the active composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Layer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `[x, y]` centre position in composition pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// Target height in composition pixels (aspect preserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    /// Target width in composition pixels (aspect preserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// Explicit `[x, y]` scale percentages, instead of a target size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Vec<f64>>,
    /// Opacity 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// Layer start time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    /// Layer duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerPropertiesParams {
    /// Composition name (defaults to the active composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the target layer within the composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_index: Option<i64>,
    /// Target the layer by name instead of index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// `[x, y]` centre position in composition pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Vec<f64>>,
    /// `[x, y]` scale percentages (100 = unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Vec<f64>>,
    /// Rotation in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,
    /// Opacity 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// New text content, for a text layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Layer start time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<f64>,
    /// Layer duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerKeyframeParams {
    /// 1-based index of the target layer within the composition.
    pub layer_index: i64,
    /// Target composition by name (preferred — item indices shift when
    /// footage is imported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the target composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// Property to keyframe, e.g. `Position`, `Scale`, `Opacity`, `Rotation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_name: Option<String>,
    /// Time in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_in_seconds: Option<f64>,
    /// Value at that time — a number for scalars, an array for Position/Scale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetLayerExpressionParams {
    /// 1-based index of the target layer within the composition.
    pub layer_index: i64,
    /// Target composition by name (preferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the target composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// Property to drive, e.g. `Position`, `Opacity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_name: Option<String>,
    /// The expression. An empty string removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression_string: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyEffectParams {
    /// 1-based index of the target layer within the composition.
    pub layer_index: i64,
    /// Target composition by name (preferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the target composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// After Effects match name, e.g. `ADBE Gaussian Blur 2`. `get-help` lists
    /// the common ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_match_name: Option<String>,
    /// Display name, if you do not have the match name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_name: Option<String>,
    /// Effect parameters, keyed by property name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_settings: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyEffectTemplateParams {
    /// 1-based index of the target layer within the composition.
    pub layer_index: i64,
    /// Target composition by name (preferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// 1-based index of the target composition in the project panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_index: Option<i64>,
    /// Template to apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_name: Option<EffectTemplate>,
    /// Overrides for the template defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_settings: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SaveFramePngParams {
    /// Absolute output path ending in `.png`.
    pub path: String,
    /// Composition name (defaults to the active composition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_name: Option<String>,
    /// Time in seconds to render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    /// Allow replacing an existing file (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SaveProjectParams {
    /// Absolute `.aep` path. Omit to save in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Allow replacing an existing file (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

/// One entry in a [`run_batch`](AfterEffectsServer::run_batch) command list.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchCommand {
    /// Bridge script to run.
    pub command: PanelScript,
    /// Arguments, shaped as that script's dedicated tool takes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunBatchParams {
    /// Commands to run in order. Keep batches to roughly 100.
    pub commands: Vec<BatchCommand>,
    /// Undo group label shown in After Effects. Set it to something the user
    /// will recognize — one Cmd-Z backs the whole batch out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_group: Option<String>,
    /// Keep going after a failing command (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router(router = live_router, vis = "pub(crate)")]
impl AfterEffectsServer {
    // --- panel plumbing ----------------------------------------------------

    #[tool(
        name = "bridge-status",
        description = "Check whether the After Effects MCP Bridge Auto panel is connected \
                       and polling. Call this first when a command seems to hang — the \
                       reply names the specific fix."
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
                "Bridge panel is polling normally."
            } else if stale_polling {
                STALE_PANEL_HINT
            } else {
                NOT_CONNECTED_HINT
            },
        })))
    }

    #[tool(
        name = "get-results",
        description = "Fetch the result of the most recently executed After Effects command \
                       — use it after a call reports that it queued a command without \
                       waiting for the outcome."
    )]
    #[tracing::instrument(name = "tool.get_results", skip(self))]
    async fn get_results(&self) -> Result<CallToolResult, ErrorData> {
        match state::latest_result().map_err(store_error)? {
            Some(result) => Ok(CallToolResult::structured(result)),
            None => Ok(CallToolResult::structured(json!({
                "status": "no-results",
                "message": "No results yet. Queue a command first, and make sure the MCP \
                            Bridge Auto panel is open in After Effects.",
            }))),
        }
    }

    #[tool(
        name = "get-help",
        description = "How to use the After Effects MCP integration: setup steps, the \
                       common effect match names, the effect templates, and the advanced \
                       scripts reachable through run-script."
    )]
    #[tracing::instrument(name = "tool.get_help", skip(self))]
    async fn get_help(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::structured(json!({ "help": HELP_TEXT })))
    }

    #[tool(
        name = "run-script",
        description = "Run a bridge script by name with raw arguments. Every script with a \
                       dedicated tool is reachable here too; this is the route to the ones \
                       without: createCamera, duplicateLayer, deleteLayer, setLayerMask, \
                       batchSetLayerProperties, bridgeTestEffects."
    )]
    #[tracing::instrument(name = "tool.run_script", skip(self, params), fields(script = ?params.script))]
    async fn run_script(
        &self,
        Parameters(params): Parameters<RunScriptParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = params.parameters.unwrap_or_else(|| json!({}));
        queue_and_wait(params.script.as_command(), args).await
    }

    // --- reading the project ------------------------------------------------

    #[tool(
        name = "get-project-info",
        description = "Information about the current After Effects project: items and the \
                       active composition."
    )]
    #[tracing::instrument(name = "tool.get_project_info", skip(self))]
    async fn get_project_info(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("getProjectInfo", json!({})).await
    }

    #[tool(
        name = "list-compositions",
        description = "List all compositions in the current After Effects project."
    )]
    #[tracing::instrument(name = "tool.list_compositions", skip(self))]
    async fn list_compositions(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("listCompositions", json!({})).await
    }

    #[tool(
        name = "get-layer-info",
        description = "List the layers of the active composition."
    )]
    #[tracing::instrument(name = "tool.get_layer_info", skip(self))]
    async fn get_layer_info(&self) -> Result<CallToolResult, ErrorData> {
        queue_and_wait("getLayerInfo", json!({})).await
    }

    // --- compositions -------------------------------------------------------

    #[tool(name = "create-composition", description = "Create a new composition.")]
    #[tracing::instrument(name = "tool.create_composition", skip(self, params), fields(comp = %params.name))]
    async fn create_composition(
        &self,
        Parameters(params): Parameters<CreateCompositionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createComposition", &params).await
    }

    #[tool(
        name = "set-composition-properties",
        description = "Change a composition's duration, frame rate, dimensions, or \
                       background colour. Note the background colour is a preview backdrop \
                       only — it never renders into the alpha channel, so a composition \
                       with no full-bleed layer is already transparent when rendered with \
                       an alpha channel."
    )]
    #[tracing::instrument(name = "tool.set_composition_properties", skip(self, params))]
    async fn set_composition_properties(
        &self,
        Parameters(params): Parameters<SetCompositionPropertiesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setCompositionProperties", &params).await
    }

    #[tool(
        name = "delete-composition",
        description = "Delete every composition with the given name — useful to rebuild a \
                       scene from scratch."
    )]
    #[tracing::instrument(name = "tool.delete_composition", skip(self, params), fields(comp = %params.comp_name))]
    async fn delete_composition(
        &self,
        Parameters(params): Parameters<DeleteCompositionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("deleteComposition", &params).await
    }

    // --- layers -------------------------------------------------------------

    #[tool(
        name = "create-text-layer",
        description = "Create a live text layer — the right way to bring a headline or \
                       label in from a design, since it stays editable and animatable. \
                       Position is the BASELINE, left-justified by default."
    )]
    #[tracing::instrument(name = "tool.create_text_layer", skip(self, params))]
    async fn create_text_layer(
        &self,
        Parameters(params): Parameters<CreateTextLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createTextLayer", &params).await
    }

    #[tool(
        name = "create-shape-layer",
        description = "Create a shape layer (rectangle, ellipse, polygon, or star) — the \
                       right way to bring a box or graphic element in from a design, since \
                       it stays editable and animatable. Position is the layer's CENTRE."
    )]
    #[tracing::instrument(name = "tool.create_shape_layer", skip(self, params), fields(shape = ?params.shape_type))]
    async fn create_shape_layer(
        &self,
        Parameters(params): Parameters<CreateShapeLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createShapeLayer", &params).await
    }

    #[tool(
        name = "create-solid-layer",
        description = "Create a solid or adjustment layer. A design's background belongs \
                       here, not in the composition background colour, which never renders."
    )]
    #[tracing::instrument(name = "tool.create_solid_layer", skip(self, params))]
    async fn create_solid_layer(
        &self,
        Parameters(params): Parameters<CreateSolidLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("createSolidLayer", &params).await
    }

    #[tool(
        name = "add-image-layer",
        description = "Import an image file (PNG/JPEG/TIFF — not WebP, which After Effects \
                       cannot read) and add it as a layer. Size it with an explicit scale \
                       or by target height/width in composition pixels. Re-imports are \
                       reused. Reserve this for genuinely raster content: vector artwork \
                       and text should be rebuilt with create-shape-layer and \
                       create-text-layer so they stay editable."
    )]
    #[tracing::instrument(name = "tool.add_image_layer", skip(self, params))]
    async fn add_image_layer(
        &self,
        Parameters(params): Parameters<AddImageLayerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("addImageLayer", &params).await
    }

    #[tool(
        name = "set-layer-properties",
        description = "Set transform and timing properties on a layer (position, scale, \
                       rotation, opacity, in/out points, text content). Prefer this over \
                       deleting and recreating a layer — recreating loses keyframes and \
                       effects already applied."
    )]
    #[tracing::instrument(name = "tool.set_layer_properties", skip(self, params))]
    async fn set_layer_properties(
        &self,
        Parameters(params): Parameters<SetLayerPropertiesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayerProperties", &params).await
    }

    // --- animation ----------------------------------------------------------

    #[tool(
        name = "set-layer-keyframe",
        description = "Set a keyframe on a layer property at a given time."
    )]
    #[tracing::instrument(name = "tool.set_layer_keyframe", skip(self, params), fields(layer = params.layer_index))]
    async fn set_layer_keyframe(
        &self,
        Parameters(params): Parameters<SetLayerKeyframeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayerKeyframe", &params).await
    }

    #[tool(
        name = "set-layer-expression",
        description = "Set or remove an expression on a layer property. Pass an empty \
                       string to remove. Prefer compName over compIndex: project item \
                       indices shift when footage is imported."
    )]
    #[tracing::instrument(name = "tool.set_layer_expression", skip(self, params), fields(layer = params.layer_index))]
    async fn set_layer_expression(
        &self,
        Parameters(params): Parameters<SetLayerExpressionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("setLayerExpression", &params).await
    }

    // --- effects ------------------------------------------------------------

    #[tool(
        name = "apply-effect",
        description = "Apply an effect to a layer, by display name or match name. \
                       `get-help` lists the common match names."
    )]
    #[tracing::instrument(name = "tool.apply_effect", skip(self, params), fields(layer = params.layer_index))]
    async fn apply_effect(
        &self,
        Parameters(params): Parameters<ApplyEffectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("applyEffect", &params).await
    }

    #[tool(
        name = "apply-effect-template",
        description = "Apply a predefined effect template to a layer. Prefer these over \
                       apply-effect unless you need a parameter the template does not set."
    )]
    #[tracing::instrument(name = "tool.apply_effect_template", skip(self, params), fields(layer = params.layer_index))]
    async fn apply_effect_template(
        &self,
        Parameters(params): Parameters<ApplyEffectTemplateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("applyEffectTemplate", &params).await
    }

    // --- output -------------------------------------------------------------

    #[tool(
        name = "save-frame-png",
        description = "Render a single frame of a composition to a PNG file — this is the \
                       verification tool. Render and look at the result rather than \
                       asserting it is correct."
    )]
    #[tracing::instrument(name = "tool.save_frame_png", skip(self, params), fields(path = %params.path))]
    async fn save_frame_png(
        &self,
        Parameters(params): Parameters<SaveFramePngParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("saveFramePng", &params).await
    }

    #[tool(
        name = "save-project",
        description = "Save the After Effects project. With no path, saves in place."
    )]
    #[tracing::instrument(name = "tool.save_project", skip(self, params))]
    async fn save_project(
        &self,
        Parameters(params): Parameters<SaveProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("saveProject", &params).await
    }

    // --- batching -----------------------------------------------------------

    #[tool(
        name = "run-batch",
        description = "Run many commands in a single round trip, inside one undo group. \
                       Strongly preferred when building a scene: the bridge costs a ~2s \
                       poll cycle per command, so 200 individual calls take minutes while \
                       one batch takes seconds. Keep batches to roughly 100 commands. Each \
                       entry is {command, args} using the underlying script names \
                       (createComposition, createTextLayer, createShapeLayer, \
                       setLayerExpression, ...). Returns a per-command status summary; \
                       stops at the first error unless continueOnError is set."
    )]
    #[tracing::instrument(name = "tool.run_batch", skip(self, params), fields(count = params.commands.len()))]
    async fn run_batch(
        &self,
        Parameters(params): Parameters<RunBatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        dispatch("runBatch", &params).await
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Serializes `params` into the bridge's argument shape and dispatches it.
///
/// The params structs are `camelCase` with `skip_serializing_if` on every
/// optional, so the panel receives exactly the arguments the caller supplied —
/// an omitted optional is absent, not `null`, which the ExtendScript side
/// distinguishes.
async fn dispatch<P: Serialize>(command: &str, params: &P) -> Result<CallToolResult, ErrorData> {
    let args = serde_json::to_value(params).map_err(|err| {
        ErrorData::internal_error(format!("failed to encode arguments: {err}"), None)
    })?;
    queue_and_wait(command, args).await
}

/// Queues one command and waits for the panel to report back.
///
/// The three non-result outcomes are deliberately distinct, because they have
/// different fixes: no panel has ever connected (install and open it), a panel
/// went quiet (open it again), and a stale panel is polling but cannot be
/// served (reinstall — reopening will not help).
async fn queue_and_wait(command: &str, args: Value) -> Result<CallToolResult, ErrorData> {
    // A command already in flight explains the silence: the panel cannot poll
    // while it is executing, and a long batch blocks it for many seconds. Only
    // treat a quiet panel as "gone" when nothing is running.
    let busy = state::current_command()
        .map_err(store_error)?
        .and_then(|command| {
            command
                .get("status")
                .and_then(Value::as_str)
                .map(|status| status == "dispatched")
        })
        .unwrap_or(false);

    // A stale panel polling right now explains the silence better than
    // anything else, and needs different advice.
    let stale_polling = state::last_stale_poll_age_ms()
        .map_err(store_error)?
        .is_some_and(|age| age < PANEL_SILENT_MS);

    let poll_age = state::last_poll_age_ms().map_err(store_error)?;
    let unreachable = match poll_age {
        None => Some(if stale_polling {
            STALE_PANEL_HINT.to_owned()
        } else {
            format!("The panel has never connected. {NOT_CONNECTED_HINT}")
        }),
        Some(age) if age > PANEL_SILENT_MS && !busy => Some(if stale_polling {
            STALE_PANEL_HINT.to_owned()
        } else {
            format!(
                "The panel has not polled for {}s. {NOT_CONNECTED_HINT}",
                age / 1000
            )
        }),
        Some(_) => None,
    };

    // Queue regardless: the command runs as soon as the panel shows up, so the
    // caller's work is not lost while they go and open it.
    let id = state::queue_command(command, &args).map_err(store_error)?;

    if let Some(hint) = unreachable {
        tracing::warn!(command, id, "queued command with no panel listening");
        // A tool error, not a success: nothing happened in After Effects, and
        // nothing will until a human acts.
        return Ok(CallToolResult::structured_error(json!({
            "status": "queued-not-executed",
            "command": command,
            "commandId": id,
            "message": hint,
            "nextStep": "Once the panel is running, call get-results to fetch the outcome.",
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
            tracing::warn!(command, id, wait_ms, "timed out waiting for panel result");
            Ok(CallToolResult::structured(json!({
                "status": "pending",
                "command": command,
                "commandId": id,
                "message": format!(
                    "No result within {}s. The panel may still be working on it.",
                    wait_ms / 1000
                ),
                "nextStep": "Call bridge-status to check the connection, then get-results \
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

const HELP_TEXT: &str = r#"# After Effects MCP bridge

## Setup (one-time)
1. Install the bridge panel: run `./install-bridge.sh` from the project
   directory (it copies mcp-bridge-auto.jsx into After Effects' ScriptUI
   Panels folder).
2. In After Effects: Settings > Scripting & Expressions > enable
   "Allow Scripts to Write Files and Access Network", then restart.
3. Open the panel: Window > mcp-bridge-auto.jsx. Leave it open; it polls
   this server every ~2 seconds.

The panel source is also served at GET /bridge/panel.jsx, for a manual
install or to check the copy you have against the one this server expects.

## How it works
A tool call queues one command; the panel claims it on its next poll, runs
it in After Effects, and posts the result back. Most tools wait up to ~12s
and return the result directly (batches, frame renders, and project saves
wait up to 4 minutes). If a call times out, `bridge-status` checks the panel
and `get-results` fetches the outcome.

## Conventions
- Positions: [x, y] in composition PIXELS, y increasing DOWNWARD, naming the
  layer's CENTRE. Converting from a design tool's top-left box:
  [x + width/2, y + height/2].
- Colours: [r, g, b] floats in 0..1 — not 0-255, not hex. The exception is a
  composition's backgroundColor, which takes 0-255 integers.
- Text layers position at the BASELINE, left-justified by default; start at
  roughly y + 0.8 * fontSize below a design's text-box top, then verify.
- Times are seconds, not frames.
- Address compositions by compName; project item indices shift when footage
  is imported.

## Building scenes quickly
Every tool call costs at least one ~2s poll cycle. Use `run-batch` to send a
whole scene in one round trip — it is the difference between minutes and
seconds, and it lands in one undo group. Build BOTTOM LAYER FIRST: After
Effects stacks each new layer on top, so creating in back-to-front order
reproduces a design's z-order for free. Name every layer as you create it.

## Common effect match names (for `apply-effect`)
- Gaussian Blur: "ADBE Gaussian Blur 2"
- Directional Blur: "ADBE Directional Blur"
- Brightness & Contrast: "ADBE Brightness & Contrast 2"
- Color Balance: "ADBE Color Balance (HLS)"
- Curves: "ADBE CurvesCustom"
- Hue/Saturation: "ADBE HUE SATURATION"
- Levels: "ADBE Pro Levels2"
- Glow: "ADBE Glow"
- Drop Shadow: "ADBE Drop Shadow"
- Fractal Noise: "ADBE Fractal Noise"

## Effect templates (for `apply-effect-template`)
gaussian-blur, directional-blur, color-balance, brightness-contrast, curves,
glow, drop-shadow, cinematic-look, text-pop

## Advanced scripts (via `run-script`)
createCamera, duplicateLayer, deleteLayer, setLayerMask,
batchSetLayerProperties, bridgeTestEffects

## Bringing a design in from Illustrator
Rebuild it as native, editable layers — create-shape-layer per box,
create-text-layer per string — rather than importing a flat PNG of the
canvas, which cannot be retimed, recoloured, retyped, or moved per element.
Read skill://after-effects-mcp/references/HANDOFF.md for the full procedure.
"#;
