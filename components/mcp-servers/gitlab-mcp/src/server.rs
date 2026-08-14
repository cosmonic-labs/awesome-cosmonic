//! GitLab MCP server — search and read GitLab for an agent.
//!
//! Four tools (`search_projects`, `get_project`, `list_issues`,
//! `get_file_contents`) each make an outbound HTTPS call to the GitLab REST API
//! v4 (see [`crate::gitlab`]). The only host they reach is `gitlab.com`, which
//! is also the sole entry in the workload's outbound `allowedHosts` allowlist —
//! the egress boundary.
//!
//! Authentication is optional. With no token the server works against public
//! projects at GitLab's unauthenticated rate limit; set a `GITLAB_TOKEN` (via a
//! Cosmonic secret) to raise the limit and reach private projects.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::gitlab;

/// GitLab MCP server. Stateless per request.
#[derive(Clone)]
pub struct GitLabServer {
    tool_router: ToolRouter<Self>,
}

/// Arguments for [`search_projects`](GitLabServer::search_projects).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchProjectsParams {
    /// The search query, matched against project name and path (e.g. `gitlab`,
    /// or `kubernetes operator`).
    pub query: String,
    /// Maximum results to return (1–10, default 10).
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// Arguments for [`get_project`](GitLabServer::get_project).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetProjectParams {
    /// The project, as either a numeric ID (e.g. `278964`) or its full path
    /// `namespace/project` (e.g. `gitlab-org/gitlab`). A path is URL-encoded
    /// for you.
    pub id: String,
}

/// Arguments for [`list_issues`](GitLabServer::list_issues).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListIssuesParams {
    /// The project, as a numeric ID or its full path `namespace/project`.
    pub id: String,
    /// Which issues to list: `"opened"` (default), `"closed"`, or `"all"`.
    /// `"open"` is accepted as an alias for `"opened"`.
    #[serde(default)]
    pub state: Option<String>,
}

/// Arguments for [`get_file_contents`](GitLabServer::get_file_contents).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileContentsParams {
    /// The project, as a numeric ID or its full path `namespace/project`.
    pub id: String,
    /// Path to a file within the repository (e.g. `README.md` or
    /// `src/main.rs`).
    pub path: String,
    /// Optional git reference (branch, tag, or commit SHA). Defaults to the
    /// project's default branch.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
}

#[tool_router]
impl GitLabServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Search public projects.
    #[tool(
        description = "Search GitLab for projects matching a query. Returns up to 10 results \
                       (path_with_namespace, name, description, star_count, web_url), ordered by \
                       star count. Matches against project name and path."
    )]
    #[tracing::instrument(name = "tool.search_projects", skip(self))]
    async fn search_projects(
        &self,
        Parameters(params): Parameters<SearchProjectsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match gitlab::search_projects(&params.query, params.per_page.unwrap_or(10)).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// Fetch metadata for one project.
    #[tool(
        description = "Get key metadata for a single GitLab project: name, path_with_namespace, \
                       description, star_count, forks_count, default_branch, visibility, and \
                       web_url. The project is addressed by numeric ID or by its \
                       'namespace/project' path."
    )]
    #[tracing::instrument(name = "tool.get_project", skip(self))]
    async fn get_project(
        &self,
        Parameters(params): Parameters<GetProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match gitlab::get_project(&params.id).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// List issues for a project.
    #[tool(
        description = "List issues for a GitLab project. Set 'state' to \"opened\" (default), \
                       \"closed\", or \"all\" (\"open\" is accepted as an alias for \"opened\"). \
                       Returns up to 20 issues (iid, title, state, author, labels, web_url). The \
                       project is addressed by numeric ID or by its 'namespace/project' path."
    )]
    #[tracing::instrument(name = "tool.list_issues", skip(self))]
    async fn list_issues(
        &self,
        Parameters(params): Parameters<ListIssuesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = params.state.as_deref().unwrap_or("opened");
        match gitlab::list_issues(&params.id, state).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// Read a file from a project.
    #[tool(
        description = "Read a file's contents from a GitLab project (base64 is decoded to text, \
                       capped at ~100 KB with a 'truncated' flag). Optionally pin to a branch, \
                       tag, or commit SHA with 'ref'; without it, the project's default branch is \
                       used. The project is addressed by numeric ID or by its 'namespace/project' \
                       path."
    )]
    #[tracing::instrument(name = "tool.get_file_contents", skip(self))]
    async fn get_file_contents(
        &self,
        Parameters(params): Parameters<GetFileContentsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match gitlab::get_file_contents(&params.id, &params.path, params.git_ref.as_deref()).await {
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
impl ServerHandler for GitLabServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Search and read GitLab. search_projects finds projects; get_project returns one \
                 project's metadata; list_issues lists a project's issues; get_file_contents reads \
                 a file (base64 decoded, ~100 KB cap). Projects are addressed by numeric ID or by \
                 their 'namespace/project' path. This server reaches only gitlab.com and runs \
                 unauthenticated by default (public projects). Providing an optional `gitlab-token` \
                 secret (exposed as GITLAB_TOKEN) raises the rate limit and enables private \
                 projects.",
            )
    }
}
