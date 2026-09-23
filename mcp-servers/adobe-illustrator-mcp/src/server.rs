//! Illustrator MCP server — pure-compute helpers for vector-graphics work.
//!
//! These tools do the arithmetic an Illustrator artist (or an agent driving
//! one) reaches for constantly: color conversion into Illustrator's 0-255
//! `RGBColor` convention and document-unit conversion into points, the unit
//! every coordinate in this server speaks. No host application is involved —
//! they answer from a request alone, which is why they stay useful even when
//! the bridge is closed.
//!
//! The tools that drive a *live* Illustrator instance live in [`crate::live`];
//! both routers are merged in [`IllustratorServer::new`].
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::skills;

/// Illustrator MCP server. Stateless — one instance per request.
#[derive(Clone)]
pub struct IllustratorServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HexToRgbParams {
    /// Hex color, `#RGB`, `#RRGGBB`, or `#RRGGBBAA` (leading `#` optional).
    pub hex: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RgbToHexParams {
    /// Red 0-255.
    pub r: u16,
    /// Green 0-255.
    pub g: u16,
    /// Blue 0-255.
    pub b: u16,
}

/// A length unit Illustrator documents commonly use.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    /// Points (1/72 inch) — the unit every coordinate in this server uses.
    Pt,
    /// Pixels at Illustrator's 72 ppi convention (1 px == 1 pt on canvas).
    Px,
    /// Inches.
    In,
    /// Millimetres.
    Mm,
    /// Centimetres.
    Cm,
    /// Picas (12 pt).
    Pica,
}

impl Unit {
    /// Points per one of this unit.
    fn points(self) -> f64 {
        match self {
            Self::Pt | Self::Px => 1.0,
            Self::In => 72.0,
            Self::Mm => 72.0 / 25.4,
            Self::Cm => 72.0 / 2.54,
            Self::Pica => 12.0,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConvertUnitsParams {
    /// The value to convert.
    pub value: f64,
    /// Unit the value is currently in.
    pub from: Unit,
    /// Unit to convert to.
    pub to: Unit,
}

#[tool_router]
impl IllustratorServer {
    pub fn new() -> Self {
        Self {
            // Pure-compute helpers plus the live-control tools from
            // `crate::live`, which share this struct's router.
            tool_router: Self::tool_router() + Self::live_router(),
        }
    }

    /// Names of the tools this server exposes, read off the generated router
    /// so the discovery document (see [`crate::discovery`]) cannot drift from
    /// what `tools/list` actually returns.
    pub fn tool_names() -> Vec<String> {
        (Self::tool_router() + Self::live_router())
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    /// Converts a hex color to Illustrator's 0-255 RGB values plus 0..1 floats.
    #[tool(
        description = "Convert a hex color (#RGB/#RRGGBB/#RRGGBBAA) to Illustrator RGBColor \
                       0-255 values and 0..1 floats"
    )]
    #[tracing::instrument(name = "tool.hex_to_rgb", skip(self))]
    async fn hex_to_rgb(
        &self,
        Parameters(params): Parameters<HexToRgbParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (r, g, b, a) = parse_hex(&params.hex)?;
        let norm = |v: u8| f64::from(v) / 255.0;
        Ok(CallToolResult::structured(serde_json::json!({
            "rgba_255": [r, g, b, a],
            "rgba_float": [norm(r), norm(g), norm(b), norm(a)],
            "hex": format!("#{r:02x}{g:02x}{b:02x}"),
        })))
    }

    /// Converts 0-255 RGB components to a hex color string.
    #[tool(description = "Convert RGB 0-255 components to a #RRGGBB hex color string")]
    #[tracing::instrument(name = "tool.rgb_to_hex", skip(self))]
    async fn rgb_to_hex(
        &self,
        Parameters(params): Parameters<RgbToHexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let clamp = |v: u16| -> Result<u8, ErrorData> {
            u8::try_from(v).map_err(|_| {
                ErrorData::invalid_params("RGB components must be 0-255".to_owned(), None)
            })
        };
        let (r, g, b) = (clamp(params.r)?, clamp(params.g)?, clamp(params.b)?);
        Ok(CallToolResult::structured(serde_json::json!({
            "hex": format!("#{r:02x}{g:02x}{b:02x}"),
            "rgb_255": [r, g, b],
        })))
    }

    /// Converts a length between document units.
    #[tool(
        description = "Convert a length between document units (pt, px, in, mm, cm, pica). \
                       Every coordinate the drawing tools take is in points."
    )]
    #[tracing::instrument(name = "tool.convert_units", skip(self))]
    async fn convert_units(
        &self,
        Parameters(params): Parameters<ConvertUnitsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if !params.value.is_finite() {
            return Err(ErrorData::invalid_params(
                "value must be a finite number".to_owned(),
                None,
            ));
        }
        let points = params.value * params.from.points();
        let converted = points / params.to.points();
        if !converted.is_finite() {
            return Err(ErrorData::invalid_params(
                "converted value overflowed to a non-finite number".to_owned(),
                None,
            ));
        }
        Ok(CallToolResult::structured(serde_json::json!({
            "value": converted,
            "points": points,
        })))
    }
}

/// Parses `#RGB`, `#RRGGBB`, or `#RRGGBBAA` (leading `#` optional) into RGBA
/// byte components. Alpha defaults to 255 when absent.
pub(crate) fn parse_hex(hex: &str) -> Result<(u8, u8, u8, u8), ErrorData> {
    let s = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ErrorData::invalid_params(
            "hex must contain only 0-9a-f digits".to_owned(),
            None,
        ));
    }
    let byte = |slice: &str| u8::from_str_radix(slice, 16).unwrap_or(0);
    match s.len() {
        3 => {
            // #RGB shorthand: each nibble doubled.
            let mut chars = s.chars();
            let mut expand = || {
                let v = chars.next().and_then(|c| c.to_digit(16)).unwrap_or(0) as u8;
                v * 16 + v
            };
            Ok((expand(), expand(), expand(), 255))
        }
        6 => Ok((byte(&s[0..2]), byte(&s[2..4]), byte(&s[4..6]), 255)),
        8 => Ok((
            byte(&s[0..2]),
            byte(&s[2..4]),
            byte(&s[4..6]),
            byte(&s[6..8]),
        )),
        _ => Err(ErrorData::invalid_params(
            "hex must be 3, 6, or 8 digits".to_owned(),
            None,
        )),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for IllustratorServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                // Skills over MCP rides on the resources primitive: declaring
                // it is what makes `skill://` URIs discoverable at all.
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "Controls a live Adobe Illustrator instance through an in-app bridge \
             that polls this server every ~2 seconds: the zero-install shuttle \
             (fetch /bridge/shuttle.sh and run it in a terminal — it relays into \
             the running Illustrator via AppleScript) or the persistent \
             Illustrator MCP Bridge CEP panel (Window > Extensions, installed by \
             ./install-bridge.sh). Check `bridge_status` first if commands seem \
             to hang, and `get_results` to fetch the outcome of one that timed \
             out.\n\n\
             Coordinates: all positions are POINTS from the ACTIVE ARTBOARD'S \
             TOP-LEFT corner with y increasing DOWNWARD (screen convention). \
             Colors are CSS hex strings, or \"none\".\n\n\
             Every live call costs a poll cycle, so build scenes with `run_batch` \
             rather than one tool call per object, and verify with \
             `export_document`. `get_help` has the setup steps; `run_jsx` is the \
             escape hatch onto the full ExtendScript DOM.\n\n\
             The helper tools — hex_to_rgb, rgb_to_hex, convert_units — are pure \
             arithmetic. They need no bridge, and are the right way to work out \
             colors and sizes before sending them to Illustrator.\n\n\
             MOVING ARTWORK TO ANOTHER APP (After Effects, Premiere, a web \
             build): read the document's STRUCTURE with list_layers, \
             list_page_items, list_text_frames and list_artboards and rebuild it \
             there as native, editable layers. Do NOT export a flat PNG of the \
             canvas and hand that over — it throws away every box, string, and \
             color the other app would need to animate or edit. Read \
             skill://illustrator-mcp/references/HANDOFF.md before starting one.\n\n\
             This server publishes skills — playbooks describing when and how to \
             use its tools. Read `skill://index.json` for the catalog, then read \
             `skill://<name>/SKILL.md` for any skill whose description matches \
             the task at hand.",
        )
    }

    /// Skills over MCP: every skill file, plus the catalog, as resources.
    ///
    /// The whole set is returned in one page — a server embedding enough
    /// skills for that to be unwieldy should honour `request.cursor` and set
    /// `next_cursor` on the result instead.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(skills::resources()))
    }

    /// Parameterized `skill://` URIs, so a client can construct a skill
    /// request without having enumerated every resource first.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            skills::resource_templates(),
        ))
    }

    #[tracing::instrument(name = "resources.read", skip(self, _context), fields(uri = %request.uri))]
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let (mime_type, text) = skills::read(&request.uri).ok_or_else(|| {
            ErrorData::resource_not_found(
                format!(
                    "no resource at {}; read {} for the skills this server serves",
                    request.uri,
                    skills::INDEX_URI
                ),
                None,
            )
        })?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type(mime_type)
        ])
        .into())
    }
}
