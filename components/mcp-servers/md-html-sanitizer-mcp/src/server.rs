//! Markdown/HTML sanitizer MCP server — pure-compute, zero-egress.
//!
//! Two tools turn untrusted input into safe HTML entirely on-device:
//! [`sanitize_html`](SanitizerServer::sanitize_html) runs raw HTML through
//! ammonia's allowlist, and [`render_markdown`](SanitizerServer::render_markdown)
//! renders CommonMark and passes the result back through the same allowlist.
//! Neither performs outbound network calls, and the workload ships with an empty
//! `allowedHosts` (deny-all), so the content this server sees can never be
//! exfiltrated — the sandbox holds no network at all.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::sanitize::{self, SanitizeError};

/// Markdown/HTML sanitizer MCP server. Stateless per request.
#[derive(Clone)]
pub struct SanitizerServer {
    tool_router: ToolRouter<Self>,
}

/// Arguments for [`sanitize_html`](SanitizerServer::sanitize_html).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SanitizeHtmlParams {
    /// The untrusted HTML to sanitize. Capped at 256 KiB; larger input is
    /// rejected.
    pub html: String,
}

/// Arguments for [`render_markdown`](SanitizerServer::render_markdown).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenderMarkdownParams {
    /// The untrusted CommonMark markdown to render to safe HTML. Capped at
    /// 256 KiB; larger input is rejected.
    pub markdown: String,
}

#[tool_router]
impl SanitizerServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Sanitize untrusted HTML, on-device, with no network access.
    #[tool(
        description = "Sanitize untrusted HTML into safe HTML using an allowlist-based cleaner \
                       (ammonia), entirely on-device with no network access. Strips <script>, \
                       <style>, <iframe>/<object>/<embed>, event-handler attributes (onclick, \
                       onerror, ...), and dangerous URL schemes (javascript:, data:), while \
                       keeping a safe subset of tags and attributes. Pass 'html' to clean. \
                       Returns the sanitized HTML and a 'removed' flag (true when anything was \
                       stripped or rewritten, i.e. output != input)."
    )]
    #[tracing::instrument(name = "tool.sanitize_html", skip(self, params), fields(html_len = params.html.len()))]
    async fn sanitize_html(
        &self,
        Parameters(params): Parameters<SanitizeHtmlParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match sanitize::sanitize_html(&params.html) {
            Ok(result) => Ok(CallToolResult::structured(serde_json::json!({
                "sanitized": result.sanitized,
                "removed": result.removed,
            }))),
            Err(err @ SanitizeError::TooLarge { .. }) => {
                Err(ErrorData::invalid_params(err.to_string(), None))
            }
        }
    }

    /// Render untrusted markdown to safe HTML, on-device, with no network
    /// access.
    #[tool(
        description = "Render untrusted CommonMark markdown to safe HTML, entirely on-device with \
                       no network access. The markdown is rendered with pulldown-cmark and the \
                       result is passed through the ammonia allowlist, so any raw HTML embedded \
                       in the markdown (CommonMark permits inline HTML) — including <script> — is \
                       neutralized. Pass 'markdown' to render. Returns the rendered, sanitized \
                       HTML."
    )]
    #[tracing::instrument(name = "tool.render_markdown", skip(self, params), fields(markdown_len = params.markdown.len()))]
    async fn render_markdown(
        &self,
        Parameters(params): Parameters<RenderMarkdownParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match sanitize::render_markdown(&params.markdown) {
            Ok(result) => Ok(CallToolResult::structured(serde_json::json!({
                "html": result.html,
            }))),
            Err(err @ SanitizeError::TooLarge { .. }) => {
                Err(ErrorData::invalid_params(err.to_string(), None))
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SanitizerServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Markdown/HTML sanitizer: turns untrusted HTML and untrusted markdown into safe \
                 HTML, entirely on-device with no network access. This server holds NO network \
                 access — its workload's outbound allowedHosts is empty (deny-all), so the content \
                 it processes cannot be exfiltrated. Two tools: 'sanitize_html' runs raw HTML \
                 through ammonia's allowlist (dropping <script>/<style>/<iframe>, event handlers, \
                 and javascript:/data: URLs, keeping a safe subset) and returns { sanitized, \
                 removed }; 'render_markdown' renders CommonMark with pulldown-cmark and passes \
                 the result back through ammonia so embedded raw HTML is neutralized, returning \
                 { html }. Both cap input at 256 KiB.",
            )
    }
}
