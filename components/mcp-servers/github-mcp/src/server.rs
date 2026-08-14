//! GitHub MCP server — search and read GitHub for an agent.
//!
//! Four tools (`search_repositories`, `get_repository`, `list_issues`,
//! `get_file_contents`) each make an outbound HTTPS call to the GitHub REST API
//! (see [`crate::github`]). The only host they reach is `api.github.com`, which
//! is also the sole entry in the workload's outbound `allowedHosts` allowlist —
//! the egress boundary.
//!
//! Authentication is optional. With no token the server works against public
//! data at GitHub's unauthenticated rate limit (60 requests/hour); set a
//! `GITHUB_TOKEN` (via a Cosmonic secret) to raise the limit and reach private
//! repositories.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::github;

/// GitHub MCP server. Stateless per request.
#[derive(Clone)]
pub struct GitHubServer {
    tool_router: ToolRouter<Self>,
}

/// Arguments for [`search_repositories`](GitHubServer::search_repositories).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchRepositoriesParams {
    /// The search query, using GitHub's repository search syntax (e.g.
    /// `tetris language:rust`, or just free text).
    pub query: String,
    /// Maximum results to return (1–10, default 10).
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// Arguments for [`get_repository`](GitHubServer::get_repository).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetRepositoryParams {
    /// The repository owner (user or organization login).
    pub owner: String,
    /// The repository name.
    pub repo: String,
}

/// Arguments for [`list_issues`](GitHubServer::list_issues).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListIssuesParams {
    /// The repository owner (user or organization login).
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// Which issues to list: `"open"` (default), `"closed"`, or `"all"`.
    #[serde(default)]
    pub state: Option<String>,
}

/// Arguments for [`get_file_contents`](GitHubServer::get_file_contents).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileContentsParams {
    /// The repository owner (user or organization login).
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// Path to a file or directory within the repository (e.g. `README.md` or
    /// `src`).
    pub path: String,
    /// Optional git reference (branch, tag, or commit SHA). Defaults to the
    /// repository's default branch.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
}

#[tool_router]
impl GitHubServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Search public repositories.
    #[tool(
        description = "Search GitHub for repositories matching a query. Returns up to 10 results \
                       (name, full_name, description, stars, language, html_url). Supports \
                       GitHub's repository search qualifiers (e.g. 'language:rust stars:>100')."
    )]
    #[tracing::instrument(name = "tool.search_repositories", skip(self))]
    async fn search_repositories(
        &self,
        Parameters(params): Parameters<SearchRepositoriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match github::search_repositories(&params.query, params.per_page.unwrap_or(10)).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// Fetch metadata for one repository.
    #[tool(
        description = "Get key metadata for a single GitHub repository: description, stars, forks, \
                       language, open issues, license, default branch, and html_url."
    )]
    #[tracing::instrument(name = "tool.get_repository", skip(self))]
    async fn get_repository(
        &self,
        Parameters(params): Parameters<GetRepositoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match github::get_repository(&params.owner, &params.repo).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// List issues for a repository (pull requests excluded).
    #[tool(
        description = "List issues for a GitHub repository (pull requests are excluded). Set \
                       'state' to \"open\" (default), \"closed\", or \"all\". Returns up to 20 \
                       issues (number, title, state, user, labels, html_url)."
    )]
    #[tracing::instrument(name = "tool.list_issues", skip(self))]
    async fn list_issues(
        &self,
        Parameters(params): Parameters<ListIssuesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = params.state.as_deref().unwrap_or("open");
        match github::list_issues(&params.owner, &params.repo, state).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// Read a file (or list a directory) from a repository.
    #[tool(
        description = "Read a file's contents from a GitHub repository (base64 is decoded to \
                       text, capped at ~100 KB with a 'truncated' flag). If the path is a \
                       directory, returns the list of entries instead. Optionally pin to a \
                       branch, tag, or commit SHA with 'ref'."
    )]
    #[tracing::instrument(name = "tool.get_file_contents", skip(self))]
    async fn get_file_contents(
        &self,
        Parameters(params): Parameters<GetFileContentsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match github::get_file_contents(
            &params.owner,
            &params.repo,
            &params.path,
            params.git_ref.as_deref(),
        )
        .await
        {
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
impl ServerHandler for GitHubServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Search and read GitHub. search_repositories finds repositories; get_repository \
                 returns one repo's metadata; list_issues lists a repo's issues (pull requests \
                 excluded); get_file_contents reads a file (base64 decoded, ~100 KB cap) or lists \
                 a directory. This server reaches only api.github.com and runs unauthenticated by \
                 default (public data, 60 requests/hour). Providing an optional `github-token` \
                 secret (exposed as GITHUB_TOKEN) raises the rate limit and enables private \
                 repositories.",
            )
    }
}
