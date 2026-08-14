//! ISS / who's-in-space MCP server — live data from the Open Notify project.
//!
//! Two tools, both taking no parameters: who is currently in space, and where
//! the International Space Station is right now. Both call the public Open
//! Notify REST APIs over `wasi:http` (see [`crate::iss`]).

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};

use crate::iss;

/// ISS / who's-in-space MCP server. Stateless per request.
#[derive(Clone)]
pub struct IssServer {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl IssServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// The people currently in space, with the spacecraft each is aboard.
    #[tool(
        description = "List the people currently in space right now, with the spacecraft each \
                       is aboard. Live data from Open Notify."
    )]
    #[tracing::instrument(name = "tool.who_is_in_space", skip(self))]
    async fn who_is_in_space(&self) -> Result<CallToolResult, ErrorData> {
        match iss::who_is_in_space().await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// The International Space Station's current latitude and longitude.
    #[tool(
        description = "Get the International Space Station's current latitude and longitude. \
                       Live data from Open Notify."
    )]
    #[tracing::instrument(name = "tool.iss_position", skip(self))]
    async fn iss_position(&self) -> Result<CallToolResult, ErrorData> {
        match iss::iss_position().await {
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
impl ServerHandler for IssServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Answer questions about who is currently in space and where the International \
                 Space Station is right now. Data is live from the Open Notify project.",
            )
    }
}
