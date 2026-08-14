//! Web-fetch MCP server — retrieve the contents of a URL.
//!
//! One tool, `fetch_url`, performs an outbound HTTP(S) GET over `wasi:http`
//! (see [`crate::fetch`]). The set of hosts it can reach is exactly the
//! workload's outbound `allowedHosts` allowlist — the egress boundary — so a
//! URL on any other host comes back as a friendly "not in the allowlist"
//! error rather than data.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::fetch;

/// Web-fetch MCP server. Stateless per request.
#[derive(Clone)]
pub struct WebFetchServer {
    tool_router: ToolRouter<Self>,
}

/// Arguments for [`fetch_url`](WebFetchServer::fetch_url).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchUrlParams {
    /// The URL to fetch. Must be `http://` or `https://`, and its host must be
    /// in this workload's outbound allowlist (`allowedHosts`).
    pub url: String,
    /// Output format: `"text"` (default) strips HTML tags and collapses
    /// whitespace to readable plain text; `"raw"` returns the body unchanged.
    #[serde(default)]
    pub format: Option<String>,
}

#[tool_router]
impl WebFetchServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Fetch the contents of a URL over HTTP(S).
    #[tool(
        description = "Fetch the contents of a URL over HTTP or HTTPS and return them. Set \
                       'format' to \"text\" (default) for readable plain text with HTML tags \
                       stripped, or \"raw\" for the body unchanged. The response is capped at \
                       ~100 KB (a 'truncated' flag marks when it was cut). Only hosts in this \
                       workload's egress allowlist (allowedHosts) can be reached."
    )]
    #[tracing::instrument(name = "tool.fetch_url", skip(self))]
    async fn fetch_url(
        &self,
        Parameters(params): Parameters<FetchUrlParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match fetch::fetch_url(&params.url, params.format.as_deref()).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }
}

/// Emits a JSON value as both `structuredContent` and a pretty-printed text
/// block (so plain clients see readable output).
fn structured_text(value: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WebFetchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Fetch the contents of a URL over HTTP or HTTPS with the fetch_url tool. \
                 Pass format=\"text\" (default) for readable plain text, or format=\"raw\" for \
                 the unmodified body; responses are capped at ~100 KB. This server can only \
                 reach hosts that the workload's egress allowlist (allowedHosts) grants — a URL \
                 on any other host returns an allowlist error, not data.",
            )
    }
}
