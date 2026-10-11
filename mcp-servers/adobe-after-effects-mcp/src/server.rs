//! The MCP server implementation.
//!
//! Every tool this server exposes drives a *live* After Effects instance and
//! lives in [`crate::live`]; this module holds the handler those tools hang
//! off — server identity, capabilities, the instructions a client reads at
//! `initialize`, and the resource surface.
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    Implementation, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool_handler, ErrorData, ServerHandler};

use crate::skills;

/// After Effects MCP server. Stateless — one instance per request. Durable
/// state (the bridge command queue) lives in `wasi:keyvalue`, not here.
#[derive(Clone)]
pub struct AfterEffectsServer {
    tool_router: ToolRouter<Self>,
}

impl AfterEffectsServer {
    pub fn new() -> Self {
        Self {
            // Every tool drives the live application; they share this struct's
            // router, declared in `crate::live`.
            tool_router: Self::live_router(),
        }
    }

    /// Names of the tools this server exposes, read off the generated router
    /// so the discovery document (see [`crate::discovery`]) cannot drift from
    /// what `tools/list` actually returns.
    pub fn tool_names() -> Vec<String> {
        Self::live_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AfterEffectsServer {
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
            "Controls a live Adobe After Effects instance through the MCP Bridge Auto \
             panel, which polls this server every ~2 seconds (Window > \
             mcp-bridge-auto.jsx; install it with ./install-bridge.sh). Check \
             `bridge-status` first if commands seem to hang, and `get-results` to fetch \
             the outcome of one that timed out.\n\n\
             Conventions: positions are [x, y] in composition PIXELS naming the layer's \
             CENTRE, y increasing DOWNWARD. Colors are [r, g, b] floats in 0..1 — not \
             0-255, not hex — except a composition's backgroundColor, which takes 0-255 \
             integers. Times are seconds. Address compositions by compName, not \
             compIndex: item indices shift when footage is imported.\n\n\
             Every call costs a poll cycle, so build scenes with `run-batch` rather than \
             one tool call per layer, and verify with `save-frame-png`. `get-help` has \
             the setup steps, effect match names, and templates.\n\n\
             BRINGING ARTWORK IN FROM ILLUSTRATOR (or any other design tool): rebuild it \
             as NATIVE, EDITABLE LAYERS — `create-shape-layer` per box, \
             `create-text-layer` per string, one layer per element, built bottom-first. \
             Do NOT import a flat PNG of the design and animate that: it cannot be \
             retimed, recolored, retyped, or moved per element, which is the whole point \
             of bringing it here. Reserve `add-image-layer` for genuinely raster assets \
             (photographs, gradient meshes) — each as its own transparent layer. Read \
             skill://after-effects-mcp/references/HANDOFF.md before starting one.\n\n\
             This server publishes skills — playbooks describing when and how to use its \
             tools. Read `skill://index.json` for the catalog, then read \
             `skill://<name>/SKILL.md` for any skill whose description matches the task \
             at hand.",
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
