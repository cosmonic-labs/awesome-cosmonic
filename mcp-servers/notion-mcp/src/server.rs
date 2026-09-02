//! The MCP server: tool definitions and result rendering for Notion.
//!
//! Every tool follows the same shape: read the configuration (the token is a
//! secret injected through the environment), validate and clamp the caller's
//! arguments locally, make one or two calls through [`crate::notion::Client`],
//! and render a compact structured result. Failures the caller can act on —
//! missing token, upstream errors, read-only deployments, local limits — are
//! tool errors (`isError: true`) with a message written for the caller;
//! JSON-RPC errors are reserved for requests the server cannot route.
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::notion::{
    self, block_summary, clamp_page_size, coerce_properties, comment_summary, flatten_properties,
    in_trash, normalize_id, object_title, page_summary, pagination, parent_summary,
    rich_text_chunks, schema_summary, search_entry, title_property_name, truncate_lines,
    user_entry, Client, INTEGRATIONS_URL, MARKDOWN_VERSION, MAX_ARRAY_ITEMS, SECRET_REF, TOKEN_ENV,
};
use crate::{markdown, skills};

/// Integration capabilities, named in 403 hints.
const CAP_READ: &str = "Read content";
const CAP_UPDATE: &str = "Update content";
const CAP_INSERT: &str = "Insert content";
const CAP_READ_COMMENTS: &str = "Read comments";
const CAP_INSERT_COMMENTS: &str = "Insert comments";
const CAP_USERS: &str = "User information";

/// 404 hints per object kind.
const NF_PAGE: &str = "If the id came from a database URL it is a database (call get_database) \
                       or a view id (?v=...), not a page.";
const NF_BLOCK: &str = "A page id works as a block id; a database or data source id does not.";
const NF_DATABASE: &str = "If this id is a data source or page id, use get_data_source or \
                           get_page instead.";
const NF_DATA_SOURCE: &str = "If this is a database id (from a database URL), call get_database \
                              first and use one of its data_sources[].id.";
const NF_COMMENT_TARGET: &str = "page_id / block_id must be a page or block shared with the \
                                 integration; discussion_id must come from list_comments.";
const NF_USER: &str = "";

/// Bounds for get_page_content / update_page_markdown output.
const MARKDOWN_MIN_CHARS: i64 = 1000;
const MARKDOWN_MAX_CHARS: i64 = 500_000;
const MARKDOWN_DEFAULT_CHARS: i64 = 60_000;
/// Longest search query forwarded.
const SEARCH_QUERY_MAX_CHARS: usize = 2000;
/// Most content_updates in one update_page_markdown call.
const MAX_CONTENT_UPDATES: usize = 50;
/// Longest comment: 100 rich-text runs of 2000 characters.
const COMMENT_MAX_CHARS: usize = 200_000;
/// Longest emoji icon accepted (a few code points with modifiers).
const EMOJI_MAX_CHARS: usize = 16;
/// Longest Markdown body accepted for page creation / replacement.
const BODY_MAX_CHARS: usize = 400_000;

type ToolResult = Result<CallToolResult, ErrorData>;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so no per-session state
/// lives here. The data source schema cache is a static on the warm instance
/// (see [`crate::notion`]).
#[derive(Clone)]
pub struct NotionServer {
    tool_router: ToolRouter<Self>,
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// Structured result with a pretty-printed text fallback.
fn ok_structured(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

/// Structured result whose text fallback is a document (Markdown), not JSON.
fn ok_document(value: Value, text: String) -> CallToolResult {
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

macro_rules! try_tool {
    ($expr:expr) => {
        match $expr {
            Ok(value) => value,
            Err(message) => return Ok(tool_error(message)),
        }
    };
}

/// The `credentials` block of the discovery document (presence only).
pub fn credentials() -> Value {
    json!([{
        "ref": SECRET_REF,
        "env": TOKEN_ENV,
        "kind": "bearer-token",
        "status": if notion::token_present() { "configured" } else { "missing" },
        "description": "Notion internal integration secret (ntn_...); share each page and database with the integration (... menu -> Connections)",
        "obtainUrl": INTEGRATIONS_URL,
        "scopes": ["read content", "update content", "insert content", "read comments", "insert comments", "read user information"],
        "validate": "check_auth",
    }])
}

fn remediation() -> String {
    format!(
        "Create an internal integration at {INTEGRATIONS_URL} (Internal; capabilities: Read, \
         Update and Insert content, Read and Insert comments, User information), share your \
         pages and databases with it (... menu -> Connections -> Connect), copy its Internal \
         Integration Secret (ntn_...), and register it as secret `{SECRET_REF}` with env \
         {TOKEN_ENV}: paste it in Cosmonic Desktop -> Secrets, or cosmonic_set_secret \
         name={SECRET_REF} uri=keychain://cosmonic/{SECRET_REF} env={TOKEN_ENV} value=<token>. \
         Then run check_auth again."
    )
}

fn clamp_max_chars(requested: Option<i64>) -> usize {
    requested
        .unwrap_or(MARKDOWN_DEFAULT_CHARS)
        .clamp(MARKDOWN_MIN_CHARS, MARKDOWN_MAX_CHARS) as usize
}

/// Renders a Markdown endpoint response (GET or PATCH) as a tool result:
/// the Markdown itself is the text block; the structured content carries
/// the (locally bounded) Markdown plus the upstream's truncation flags.
fn markdown_result(id: &str, response: &Value, max_chars: usize) -> CallToolResult {
    let markdown = response
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (markdown, local_truncated) = truncate_lines(markdown, max_chars);
    let upstream_truncated = response
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let unknown = response
        .get("unknown_block_ids")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let mut notes = Vec::new();
    if local_truncated {
        notes.push(format!(
            "truncated locally at {max_chars} characters; raise max_chars (up to \
             {MARKDOWN_MAX_CHARS}) to read more"
        ));
    }
    if upstream_truncated {
        notes.push(
            "Notion cut the render at its block cap (~20k blocks); the Markdown is not the \
             whole page"
                .to_owned(),
        );
    }
    if unknown.as_array().is_some_and(|ids| !ids.is_empty()) {
        notes.push(
            "some blocks could not be rendered (unknown_block_ids); read them with \
             get_block_children or ask the user to share the nested pages"
                .to_owned(),
        );
    }
    let note = notes.join("; ");
    let mut text = markdown.clone();
    if !note.is_empty() {
        text.push_str("\n\n…[");
        text.push_str(&note);
        text.push(']');
    }
    let value = json!({
        "id": response.get("id").cloned().unwrap_or_else(|| Value::String(id.to_owned())),
        "chars_returned": markdown.chars().count(),
        "markdown": markdown,
        "local_truncated": local_truncated,
        "truncated": upstream_truncated,
        "unknown_block_ids": unknown,
        "note": note,
    });
    ok_document(value, text)
}

fn emoji_icon(emoji: &str) -> Result<Value, String> {
    let emoji = emoji.trim();
    if emoji.is_empty() || emoji.chars().count() > EMOJI_MAX_CHARS {
        return Err(format!(
            "icon_emoji must be a single emoji (1..{EMOJI_MAX_CHARS} characters)"
        ));
    }
    Ok(json!({ "type": "emoji", "emoji": emoji }))
}

fn ensure_body_len(body: &str, field: &str) -> Result<(), String> {
    if body.chars().count() > BODY_MAX_CHARS {
        return Err(format!(
            "{field} is longer than {BODY_MAX_CHARS} characters (Notion's payload cap is 500 KB); \
             create the page with a shorter body and append the rest with append_blocks or \
             update_page_markdown"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Title text to match (Notion search matches titles only, not body
    /// text). Omit to list everything shared with the integration.
    pub query: Option<String>,
    /// Restrict to one object type: "page" or "data_source" (tables live in
    /// data sources since API 2025-09-03; there is no "database" value).
    pub object: Option<String>,
    /// Sort by last_edited_time: "ascending" or "descending". Omit for
    /// relevance order.
    pub sort_direction: Option<String>,
    /// Results per page, clamped to 1..100 (default 20).
    pub page_size: Option<i64>,
    /// Cursor from a previous result's next_cursor.
    pub start_cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPageParams {
    /// Page id (32 hex, hyphenated UUID) or a notion.so page URL.
    pub page_id: String,
    /// Property ids to return (max 100); omit for all properties.
    pub filter_properties: Option<Vec<String>>,
    /// Return Notion's raw property objects instead of flattened values.
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPageContentParams {
    /// Page id or notion.so URL.
    pub page_id: String,
    /// Include meeting-note transcripts inline (default false).
    pub include_transcript: Option<bool>,
    /// Maximum characters of Markdown to return, clamped to 1000..500000
    /// (default 60000); longer content is cut at a line boundary.
    pub max_chars: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetBlockChildrenParams {
    /// Block id — a page id works, a page is a block.
    pub block_id: String,
    /// Children per page, clamped to 1..100 (default 50).
    pub page_size: Option<i64>,
    /// Cursor from a previous result's next_cursor.
    pub start_cursor: Option<String>,
    /// Return Notion's raw block objects instead of summaries.
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreatePageParams {
    /// Parent page id or URL (exactly one of parent_page_id /
    /// parent_data_source_id).
    pub parent_page_id: Option<String>,
    /// Parent data source id (from get_database's data_sources[]); creates
    /// a row. A database id here yields a 404 — resolve it first.
    pub parent_data_source_id: Option<String>,
    /// Page title. Under a page parent this is the only settable property;
    /// under a data source it fills that source's title property.
    pub title: Option<String>,
    /// Raw Notion property-value objects keyed by property name (merged over
    /// title). Read exact names/types with get_data_source first.
    pub properties: Option<Map<String, Value>>,
    /// Page body as Notion-flavored Markdown, parsed by Notion itself.
    pub body_markdown: Option<String>,
    /// Single emoji to use as the page icon.
    pub icon_emoji: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateDataSourceItemParams {
    /// Data source id (from get_database's data_sources[] or a search
    /// result of object "data_source").
    pub data_source_id: String,
    /// Value for the title property (whatever it is named).
    pub title: Option<String>,
    /// Plain values keyed by property name: string for title/rich_text/
    /// select/status/url/email/phone_number, number, boolean for checkbox,
    /// [names] for multi_select, ISO date string or {start,end} for date,
    /// [page ids] for relation, [user ids] for people. Coerced via the
    /// schema; names match exactly, then case-insensitively.
    pub properties: Option<Map<String, Value>>,
    /// Page body as Notion-flavored Markdown.
    pub body_markdown: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatePageParams {
    /// Page id or URL.
    pub page_id: String,
    /// Raw Notion property-value objects keyed by name (data-source pages:
    /// any property; plain pages: only "title").
    pub properties: Option<Map<String, Value>>,
    /// true moves the page to the trash, false restores it.
    pub in_trash: Option<bool>,
    /// Single emoji to set as the icon.
    pub icon_emoji: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContentUpdate {
    /// Existing text to find; must match exactly once unless replace_all.
    pub old_str: String,
    /// Replacement text (empty string deletes the match).
    pub new_str: String,
    /// Replace every match instead of requiring a unique one.
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatePageMarkdownParams {
    /// Page id or URL.
    pub page_id: String,
    /// "update" for targeted find-and-replace (needs `updates`) or
    /// "replace" to overwrite the whole body (needs `new_markdown`).
    pub mode: String,
    /// mode=update: 1..50 {old_str, new_str, replace_all?} edits.
    pub updates: Option<Vec<ContentUpdate>>,
    /// mode=replace: the new body as Notion-flavored Markdown.
    pub new_markdown: Option<String>,
    /// Allow the edit to delete child pages/databases (default false;
    /// Notion refuses such edits otherwise).
    pub allow_deleting_content: Option<bool>,
    /// Maximum characters of the resulting Markdown to return
    /// (1000..500000, default 60000).
    pub max_chars: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppendBlocksParams {
    /// Page id or block id (or URL) to append under.
    pub block_id: String,
    /// Flat Markdown subset: `#`/`##`/`###` headings, paragraphs, `-` or `*`
    /// bullets, `1.` numbered items, `- [ ]` / `- [x]` to-dos, fenced code,
    /// `>` quotes, `---` dividers. At most 100 blocks per call; long text is
    /// split into 2000-character runs.
    pub markdown: String,
    /// "end" (default) or "start" of the parent's children.
    pub position: Option<String>,
    /// Insert right after this sibling block id instead.
    pub after_block_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDatabaseParams {
    /// Database id or the database URL (the trailing id in the path, not
    /// the ?v= view id).
    pub database_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDataSourceParams {
    /// Data source id.
    pub data_source_id: String,
    /// Return Notion's raw data source object.
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryDataSourceParams {
    /// Data source id (a database id returns 404 — call get_database).
    pub data_source_id: String,
    /// Notion filter object, passed through unchanged.
    pub filter: Option<Value>,
    /// Notion sort objects [{property|timestamp, direction}].
    pub sorts: Option<Vec<Value>>,
    /// Rows per page, clamped to 1..100 (default 25).
    pub page_size: Option<i64>,
    /// Cursor from a previous result's next_cursor.
    pub start_cursor: Option<String>,
    /// Return trashed rows instead of live ones.
    pub include_trashed: Option<bool>,
    /// Property ids/names to include (speeds up wide tables).
    pub filter_properties: Option<Vec<String>>,
    /// Return raw property objects instead of flattened values.
    pub raw: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCommentsParams {
    /// Page id (page-level comments) or block id.
    pub block_id: String,
    /// Comments per page, clamped to 1..100 (default 50).
    pub page_size: Option<i64>,
    /// Cursor from a previous result's next_cursor.
    pub start_cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateCommentParams {
    /// Comment text (1..200000 characters; split into 2000-char runs).
    pub text: String,
    /// Comment on a whole page (exactly one of page_id / block_id /
    /// discussion_id).
    pub page_id: Option<String>,
    /// Comment on a specific block.
    pub block_id: Option<String>,
    /// Reply in an existing discussion thread (from list_comments).
    pub discussion_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListUsersParams {
    /// Users per page, clamped to 1..100 (default 50).
    pub page_size: Option<i64>,
    /// Cursor from a previous result's next_cursor.
    pub start_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl NotionServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Names of the tools this server exposes, read off the generated router
    /// so the discovery document (see [`crate::discovery`]) cannot drift from
    /// what `tools/list` actually returns.
    pub fn tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    #[tool(
        description = "Verify the Notion credential: reports status ok|missing|invalid|insufficient, the integration's bot identity and workspace, and exact remediation steps. Call first; never retry a missing/invalid result."
    )]
    #[tracing::instrument(name = "tool.check_auth", skip(self))]
    async fn check_auth(&self) -> ToolResult {
        let base = json!({
            "ref": SECRET_REF,
            "env": TOKEN_ENV,
            "obtainUrl": INTEGRATIONS_URL,
        });
        let with = |status: &str, extra: Value| {
            let mut out = base.as_object().cloned().unwrap_or_default();
            out.insert("status".into(), Value::String(status.to_owned()));
            if let Some(map) = extra.as_object() {
                for (k, v) in map {
                    out.insert(k.clone(), v.clone());
                }
            }
            Value::Object(out)
        };
        if !notion::token_present() {
            return Ok(CallToolResult::structured_error(with(
                "missing",
                json!({ "message": notion::missing_token_message(), "remediation": remediation() }),
            )));
        }
        let client = match Client::from_env() {
            Ok(client) => client,
            Err(message) => {
                return Ok(CallToolResult::structured_error(with(
                    "error",
                    json!({ "message": message, "remediation": remediation() }),
                )))
            }
        };
        match client.get("/v1/users/me", &[], None).await {
            Ok(me) => Ok(ok_structured(with(
                "ok",
                json!({
                    "identity": self_summary(&me),
                    "api_version": client.version(),
                    "read_only": notion::read_only(),
                    "capabilities": "not reported by the API; a 403 restricted_resource on a tool names the missing one",
                }),
            ))),
            Err(err) => {
                let status = match err.status {
                    401 => "invalid",
                    403 => "insufficient",
                    _ => "error",
                };
                Ok(CallToolResult::structured_error(with(
                    status,
                    json!({
                        "message": err.explain(CAP_READ, NF_USER),
                        "http_status": err.status,
                        "code": err.code,
                        "transient": err.is_transient(),
                        "remediation": remediation(),
                    }),
                )))
            }
        }
    }

    #[tool(
        description = "Return the integration's bot user and workspace (GET /v1/users/me). Cheapest connectivity and token check."
    )]
    #[tracing::instrument(name = "tool.get_self", skip(self))]
    async fn get_self(&self) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let me = try_tool!(client
            .get("/v1/users/me", &[], None)
            .await
            .map_err(|e| e.explain(CAP_READ, NF_USER)));
        Ok(ok_structured(self_summary(&me)))
    }

    #[tool(
        description = "Search pages and data sources shared with the integration by title (title-only, eventually consistent). Returns compact rows with ids, titles, URLs, parents; paginate with next_cursor."
    )]
    #[tracing::instrument(name = "tool.search", skip(self))]
    async fn search(&self, Parameters(params): Parameters<SearchParams>) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let mut body = Map::new();
        if let Some(query) = params
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            if query.chars().count() > SEARCH_QUERY_MAX_CHARS {
                return Ok(tool_error(format!(
                    "query is longer than {SEARCH_QUERY_MAX_CHARS} characters; search matches \
                     titles, so a short distinctive phrase works best"
                )));
            }
            body.insert("query".into(), Value::String(query.to_owned()));
        }
        if let Some(object) = params
            .object
            .as_deref()
            .map(str::trim)
            .filter(|o| !o.is_empty())
        {
            match object {
                "page" | "data_source" => {
                    body.insert(
                        "filter".into(),
                        json!({ "property": "object", "value": object }),
                    );
                }
                "database" => {
                    return Ok(tool_error(
                        "Notion search has no \"database\" filter: since API 2025-09-03 tables \
                         are data sources, so pass object=\"data_source\" (each result carries \
                         its database_id), then call get_database if you need the container.",
                    ))
                }
                other => {
                    return Ok(tool_error(format!(
                        "object must be \"page\" or \"data_source\", got {other:?}"
                    )))
                }
            }
        }
        if let Some(direction) = params
            .sort_direction
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            match direction {
                "ascending" | "descending" => {
                    body.insert(
                        "sort".into(),
                        json!({ "timestamp": "last_edited_time", "direction": direction }),
                    );
                }
                other => {
                    return Ok(tool_error(format!(
                        "sort_direction must be \"ascending\" or \"descending\", got {other:?}"
                    )))
                }
            }
        }
        let page_size = clamp_page_size(params.page_size, 20);
        body.insert("page_size".into(), json!(page_size));
        if let Some(cursor) = params.start_cursor.filter(|c| !c.trim().is_empty()) {
            body.insert("start_cursor".into(), Value::String(cursor));
        }
        let list = try_tool!(client
            .post("/v1/search", &[], Value::Object(body), None)
            .await
            .map_err(|e| e.explain(CAP_READ, NF_PAGE)));
        let results: Vec<Value> = list
            .get("results")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(search_entry).collect())
            .unwrap_or_default();
        let mut out = Map::new();
        out.insert("count".into(), json!(results.len()));
        out.insert("page_size".into(), json!(page_size));
        out.insert("results".into(), Value::Array(results));
        out.extend(pagination(&list));
        Ok(ok_structured(Value::Object(out)))
    }

    #[tool(
        description = "Read a page's metadata and property values (flattened to plain values; raw=true for Notion's objects). Body content is NOT included — use get_page_content."
    )]
    #[tracing::instrument(name = "tool.get_page", skip(self))]
    async fn get_page(&self, Parameters(params): Parameters<GetPageParams>) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let id = try_tool!(normalize_id(&params.page_id));
        let query = try_tool!(filter_properties_query(params.filter_properties.as_deref()));
        let page = try_tool!(client
            .get(&format!("/v1/pages/{id}"), &query, None)
            .await
            .map_err(|e| e.explain(CAP_READ, NF_PAGE)));
        Ok(ok_structured(page_summary(
            &page,
            params.raw.unwrap_or(false),
        )))
    }

    #[tool(
        description = "Read a page's body as Notion-flavored Markdown in one call (headings, lists, to-dos, code, callouts, toggles, tables, child page/database links). Reports upstream truncation and unreadable block ids."
    )]
    #[tracing::instrument(name = "tool.get_page_content", skip(self))]
    async fn get_page_content(
        &self,
        Parameters(params): Parameters<GetPageContentParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let id = try_tool!(normalize_id(&params.page_id));
        let max_chars = clamp_max_chars(params.max_chars);
        let mut query: Vec<(&str, String)> = Vec::new();
        if params.include_transcript == Some(true) {
            query.push(("include_transcript", "true".to_owned()));
        }
        let response = try_tool!(client
            .get(
                &format!("/v1/pages/{id}/markdown"),
                &query,
                Some(MARKDOWN_VERSION)
            )
            .await
            .map_err(|e| e.explain(CAP_READ, NF_PAGE)));
        Ok(markdown_result(&id, &response, max_chars))
    }

    #[tool(
        description = "List the direct child blocks of a page or block with ids, types and plain text — for commenting on, inserting after, or inspecting specific blocks. First level only; paginate with next_cursor."
    )]
    #[tracing::instrument(name = "tool.get_block_children", skip(self))]
    async fn get_block_children(
        &self,
        Parameters(params): Parameters<GetBlockChildrenParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let id = try_tool!(normalize_id(&params.block_id));
        let page_size = clamp_page_size(params.page_size, 50);
        let mut query = vec![("page_size", page_size.to_string())];
        if let Some(cursor) = params.start_cursor.filter(|c| !c.trim().is_empty()) {
            query.push(("start_cursor", cursor));
        }
        let list = try_tool!(client
            .get(&format!("/v1/blocks/{id}/children"), &query, None)
            .await
            .map_err(|e| e.explain(CAP_READ, NF_BLOCK)));
        let raw = params.raw.unwrap_or(false);
        let blocks: Vec<Value> = list
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|b| if raw { b.clone() } else { block_summary(b) })
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Map::new();
        out.insert("block_id".into(), Value::String(id));
        out.insert("count".into(), json!(blocks.len()));
        out.insert("page_size".into(), json!(page_size));
        out.insert("blocks".into(), Value::Array(blocks));
        out.extend(pagination(&list));
        Ok(ok_structured(Value::Object(out)))
    }

    #[tool(
        description = "Create a page under a parent page or as a row in a data source, with an optional Markdown body parsed by Notion. Gated by NOTION_READ_ONLY; needs the Insert content capability."
    )]
    #[tracing::instrument(name = "tool.create_page", skip(self))]
    async fn create_page(&self, Parameters(params): Parameters<CreatePageParams>) -> ToolResult {
        let client = try_tool!(Client::from_env());
        if let Some(refusal) = client.refuse_write("create_page") {
            return Ok(tool_error(refusal));
        }
        let page_parent = params
            .parent_page_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let source_parent = params
            .parent_data_source_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (parent, source_id) =
            match (page_parent, source_parent) {
                (Some(page), None) => {
                    let id = try_tool!(normalize_id(page));
                    (json!({ "type": "page_id", "page_id": id }), None)
                }
                (None, Some(source)) => {
                    let id = try_tool!(normalize_id(source));
                    (
                        json!({ "type": "data_source_id", "data_source_id": id.clone() }),
                        Some(id),
                    )
                }
                _ => return Ok(tool_error(
                    "pass exactly one of parent_page_id or parent_data_source_id (a database id \
                     is neither: call get_database and use one of its data_sources[].id)",
                )),
            };
        let mut properties = Map::new();
        if let Some(title) = params.title.as_deref() {
            let name =
                match &source_id {
                    Some(id) => {
                        let schema = try_tool!(client
                            .data_source_schema(id)
                            .await
                            .map_err(|e| e.explain(CAP_READ, NF_DATA_SOURCE)));
                        match title_property_name(&schema) {
                            Some(name) => name,
                            None => return Ok(tool_error(
                                "the data source has no title property; pass properties instead",
                            )),
                        }
                    }
                    None => "title".to_owned(),
                };
            properties.insert(name, json!({ "title": rich_text_chunks(title) }));
        }
        if let Some(extra) = params.properties {
            properties.extend(extra);
        }
        let mut body = Map::new();
        body.insert("parent".into(), parent);
        body.insert("properties".into(), Value::Object(properties));
        let mut version = None;
        if let Some(markdown) = params.body_markdown.filter(|m| !m.trim().is_empty()) {
            try_tool!(ensure_body_len(&markdown, "body_markdown"));
            body.insert("markdown".into(), Value::String(markdown));
            version = Some(MARKDOWN_VERSION);
        }
        if let Some(emoji) = params.icon_emoji.as_deref() {
            body.insert("icon".into(), try_tool!(emoji_icon(emoji)));
        }
        let page = try_tool!(client
            .post("/v1/pages", &[], Value::Object(body), version)
            .await
            .map_err(|e| e.explain(CAP_INSERT, NF_PAGE)));
        Ok(ok_structured(json!({
            "id": page.get("id").cloned().unwrap_or(Value::Null),
            "url": page.get("url").cloned().unwrap_or(Value::Null),
            "title": object_title(&page),
            "parent": parent_summary(page.get("parent")),
        })))
    }

    #[tool(
        description = "Add a row to a data source from PLAIN values (strings, numbers, booleans, option names, ISO dates, id lists) coerced through the data source schema. Gated by NOTION_READ_ONLY; needs Insert content."
    )]
    #[tracing::instrument(name = "tool.create_data_source_item", skip(self))]
    async fn create_data_source_item(
        &self,
        Parameters(params): Parameters<CreateDataSourceItemParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        if let Some(refusal) = client.refuse_write("create_data_source_item") {
            return Ok(tool_error(refusal));
        }
        let id = try_tool!(normalize_id(&params.data_source_id));
        if params.title.is_none() && params.properties.as_ref().is_none_or(Map::is_empty) {
            return Ok(tool_error("pass a title and/or at least one property"));
        }
        let schema = try_tool!(client
            .data_source_schema(&id)
            .await
            .map_err(|e| e.explain(CAP_READ, NF_DATA_SOURCE)));
        let properties = try_tool!(coerce_properties(
            &schema,
            params.title.as_deref(),
            params.properties.as_ref(),
        ));
        let mut body = Map::new();
        body.insert(
            "parent".into(),
            json!({ "type": "data_source_id", "data_source_id": id }),
        );
        body.insert("properties".into(), Value::Object(properties));
        let mut version = None;
        if let Some(markdown) = params.body_markdown.filter(|m| !m.trim().is_empty()) {
            try_tool!(ensure_body_len(&markdown, "body_markdown"));
            body.insert("markdown".into(), Value::String(markdown));
            version = Some(MARKDOWN_VERSION);
        }
        let page = try_tool!(client
            .post("/v1/pages", &[], Value::Object(body), version)
            .await
            .map_err(|e| e.explain(CAP_INSERT, NF_DATA_SOURCE)));
        Ok(ok_structured(json!({
            "id": page.get("id").cloned().unwrap_or(Value::Null),
            "url": page.get("url").cloned().unwrap_or(Value::Null),
            "title": object_title(&page),
            "properties": flatten_properties(page.get("properties")),
        })))
    }

    #[tool(
        description = "Update a page's property values, move it to/from the trash, or set an emoji icon. Cannot move pages or edit the body (use update_page_markdown / append_blocks). Gated by NOTION_READ_ONLY; needs Update content."
    )]
    #[tracing::instrument(name = "tool.update_page", skip(self))]
    async fn update_page(&self, Parameters(params): Parameters<UpdatePageParams>) -> ToolResult {
        let client = try_tool!(Client::from_env());
        if let Some(refusal) = client.refuse_write("update_page") {
            return Ok(tool_error(refusal));
        }
        let id = try_tool!(normalize_id(&params.page_id));
        let mut body = Map::new();
        if let Some(properties) = params.properties.filter(|p| !p.is_empty()) {
            body.insert("properties".into(), Value::Object(properties));
        }
        if let Some(trash) = params.in_trash {
            let field = if client.legacy() {
                "archived"
            } else {
                "in_trash"
            };
            body.insert(field.into(), Value::Bool(trash));
        }
        if let Some(emoji) = params.icon_emoji.as_deref() {
            body.insert("icon".into(), try_tool!(emoji_icon(emoji)));
        }
        if body.is_empty() {
            return Ok(tool_error(
                "nothing to update: pass properties, in_trash, and/or icon_emoji",
            ));
        }
        let page = try_tool!(client
            .patch(&format!("/v1/pages/{id}"), Value::Object(body), None)
            .await
            .map_err(|e| e.explain(CAP_UPDATE, NF_PAGE)));
        Ok(ok_structured(json!({
            "id": page.get("id").cloned().unwrap_or(Value::Null),
            "url": page.get("url").cloned().unwrap_or(Value::Null),
            "title": object_title(&page),
            "in_trash": in_trash(&page),
            "last_edited_time": page.get("last_edited_time").cloned().unwrap_or(Value::Null),
            "properties": flatten_properties(page.get("properties")),
        })))
    }

    #[tool(
        description = "Edit a page body as Markdown: mode=update applies find-and-replace edits (old_str must match exactly once unless replace_all), mode=replace overwrites the whole body. Returns the resulting Markdown. Gated by NOTION_READ_ONLY; needs Update content."
    )]
    #[tracing::instrument(name = "tool.update_page_markdown", skip(self))]
    async fn update_page_markdown(
        &self,
        Parameters(params): Parameters<UpdatePageMarkdownParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        if let Some(refusal) = client.refuse_write("update_page_markdown") {
            return Ok(tool_error(refusal));
        }
        let id = try_tool!(normalize_id(&params.page_id));
        let max_chars = clamp_max_chars(params.max_chars);
        let allow_deleting = params.allow_deleting_content.unwrap_or(false);
        let body = match params.mode.trim() {
            "update" => {
                let updates = params.updates.unwrap_or_default();
                if updates.is_empty() || updates.len() > MAX_CONTENT_UPDATES {
                    return Ok(tool_error(format!(
                        "mode=update needs 1..{MAX_CONTENT_UPDATES} entries in `updates`, got {}",
                        updates.len()
                    )));
                }
                let mut content_updates = Vec::with_capacity(updates.len());
                for (index, update) in updates.iter().enumerate() {
                    if update.old_str.is_empty() {
                        return Ok(tool_error(format!(
                            "updates[{index}].old_str is empty; it must be the exact existing text"
                        )));
                    }
                    try_tool!(ensure_body_len(&update.new_str, "new_str"));
                    content_updates.push(json!({
                        "old_str": update.old_str,
                        "new_str": update.new_str,
                        "replace_all_matches": update.replace_all.unwrap_or(false),
                    }));
                }
                json!({
                    "type": "update_content",
                    "update_content": {
                        "content_updates": content_updates,
                        "allow_deleting_content": allow_deleting,
                    }
                })
            }
            "replace" => {
                let Some(new_markdown) = params.new_markdown else {
                    return Ok(tool_error("mode=replace needs `new_markdown`"));
                };
                try_tool!(ensure_body_len(&new_markdown, "new_markdown"));
                json!({
                    "type": "replace_content",
                    "replace_content": {
                        "new_str": new_markdown,
                        "allow_deleting_content": allow_deleting,
                    }
                })
            }
            other => {
                return Ok(tool_error(format!(
                    "mode must be \"update\" or \"replace\", got {other:?}"
                )))
            }
        };
        let response = try_tool!(client
            .patch(
                &format!("/v1/pages/{id}/markdown"),
                body,
                Some(MARKDOWN_VERSION)
            )
            .await
            .map_err(|e| e.explain(CAP_UPDATE, NF_PAGE)));
        Ok(markdown_result(&id, &response, max_chars))
    }

    #[tool(
        description = "Append blocks to a page or block from a flat Markdown subset (headings, paragraphs, bullets, numbered, to-dos, fenced code, quotes, dividers), at the end, start, or after a given block. Max 100 blocks per call. Gated by NOTION_READ_ONLY; needs Insert content."
    )]
    #[tracing::instrument(name = "tool.append_blocks", skip(self))]
    async fn append_blocks(
        &self,
        Parameters(params): Parameters<AppendBlocksParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        if let Some(refusal) = client.refuse_write("append_blocks") {
            return Ok(tool_error(refusal));
        }
        let id = try_tool!(normalize_id(&params.block_id));
        let children = try_tool!(markdown::to_blocks(&params.markdown));
        let after = match params.after_block_id.as_deref().map(str::trim) {
            Some(after) if !after.is_empty() => Some(try_tool!(normalize_id(after))),
            _ => None,
        };
        let position = params
            .position
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or("end");
        let mut body = Map::new();
        match (position, after) {
            (_, Some(after_id)) if position == "end" || position == "after_block" => {
                if client.legacy() {
                    body.insert("after".into(), Value::String(after_id));
                } else {
                    body.insert(
                        "position".into(),
                        json!({ "type": "after_block", "after_block": { "id": after_id } }),
                    );
                }
            }
            (_, Some(_)) => {
                return Ok(tool_error(
                    "after_block_id cannot be combined with position=\"start\"",
                ))
            }
            ("end", None) => {
                if !client.legacy() {
                    body.insert("position".into(), json!({ "type": "end" }));
                }
            }
            ("start", None) => {
                if client.legacy() {
                    return Ok(tool_error(
                        "position=\"start\" needs Notion-Version 2026-03-11; this deployment \
                         runs NOTION_VERSION=2025-09-03 which only supports after_block_id",
                    ));
                }
                body.insert("position".into(), json!({ "type": "start" }));
            }
            (other, None) => {
                return Ok(tool_error(format!(
                    "position must be \"end\" or \"start\" (or pass after_block_id), got {other:?}"
                )))
            }
        }
        let count = children.len();
        body.insert("children".into(), Value::Array(children));
        let list = try_tool!(client
            .patch(
                &format!("/v1/blocks/{id}/children"),
                Value::Object(body),
                None
            )
            .await
            .map_err(|e| e.explain(CAP_INSERT, NF_BLOCK)));
        let created: Vec<Value> = list
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|b| {
                        json!({
                            "id": b.get("id").cloned().unwrap_or(Value::Null),
                            "type": b.get("type").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ok_structured(json!({
            "parent_id": id,
            "sent": count,
            "appended": created.len(),
            "blocks": created,
        })))
    }

    #[tool(
        description = "Resolve a database (container) to its data sources — required before query_data_source or create_data_source_item when you only have a database id or URL."
    )]
    #[tracing::instrument(name = "tool.get_database", skip(self))]
    async fn get_database(&self, Parameters(params): Parameters<GetDatabaseParams>) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let id = try_tool!(normalize_id(&params.database_id));
        let database = try_tool!(client
            .get(&format!("/v1/databases/{id}"), &[], None)
            .await
            .map_err(|e| e.explain(CAP_READ, NF_DATABASE)));
        let sources: Vec<Value> = database
            .get("data_sources")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|s| {
                        json!({
                            "id": s.get("id").cloned().unwrap_or(Value::Null),
                            "name": s.get("name").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ok_structured(json!({
            "id": database.get("id").cloned().unwrap_or(Value::Null),
            "title": object_title(&database),
            "url": database.get("url").cloned().unwrap_or(Value::Null),
            "is_inline": database.get("is_inline").cloned().unwrap_or(Value::Null),
            "in_trash": in_trash(&database),
            "parent": parent_summary(database.get("parent")),
            "data_sources": sources,
        })))
    }

    #[tool(
        description = "Read a data source's schema: property names, ids, types, select/status/multi_select options, relation targets, formula expressions. Read this before writing properties or building query filters."
    )]
    #[tracing::instrument(name = "tool.get_data_source", skip(self))]
    async fn get_data_source(
        &self,
        Parameters(params): Parameters<GetDataSourceParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let id = try_tool!(normalize_id(&params.data_source_id));
        let source = try_tool!(client
            .get(&format!("/v1/data_sources/{id}"), &[], None)
            .await
            .map_err(|e| e.explain(CAP_READ, NF_DATA_SOURCE)));
        if params.raw.unwrap_or(false) {
            return Ok(ok_structured(source));
        }
        Ok(ok_structured(json!({
            "id": source.get("id").cloned().unwrap_or(Value::Null),
            "title": object_title(&source),
            "url": source.get("url").cloned().unwrap_or(Value::Null),
            "database_id": source
                .get("database_parent").and_then(|p| p.get("database_id"))
                .or_else(|| source.get("parent").and_then(|p| p.get("database_id")))
                .cloned().unwrap_or(Value::Null),
            "parent": parent_summary(source.get("parent")),
            "in_trash": in_trash(&source),
            "title_property": title_property_name(&source),
            "properties": schema_summary(&source),
        })))
    }

    #[tool(
        description = "Query rows of a data source with Notion filter/sorts JSON passed through; rows come back with flattened property values. Paginate with next_cursor; a single query caps at 10,000 rows upstream."
    )]
    #[tracing::instrument(name = "tool.query_data_source", skip(self))]
    async fn query_data_source(
        &self,
        Parameters(params): Parameters<QueryDataSourceParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let id = try_tool!(normalize_id(&params.data_source_id));
        let page_size = clamp_page_size(params.page_size, 25);
        let mut body = Map::new();
        if let Some(filter) = params.filter {
            if !filter.is_object() {
                return Ok(tool_error(
                    "filter must be a Notion filter object such as \
                     {\"property\":\"Status\",\"select\":{\"equals\":\"Done\"}} or \
                     {\"and\":[...]}",
                ));
            }
            body.insert("filter".into(), filter);
        }
        if let Some(sorts) = params.sorts.filter(|s| !s.is_empty()) {
            if sorts.iter().any(|s| !s.is_object()) {
                return Ok(tool_error(
                    "sorts must be objects like {\"property\":\"Due\",\"direction\":\"ascending\"} \
                     or {\"timestamp\":\"last_edited_time\",\"direction\":\"descending\"}",
                ));
            }
            body.insert("sorts".into(), Value::Array(sorts));
        }
        body.insert("page_size".into(), json!(page_size));
        if let Some(cursor) = params.start_cursor.filter(|c| !c.trim().is_empty()) {
            body.insert("start_cursor".into(), Value::String(cursor));
        }
        if params.include_trashed == Some(true) {
            body.insert("is_archived".into(), Value::Bool(true));
        }
        let query = try_tool!(filter_properties_query(params.filter_properties.as_deref()));
        let list = try_tool!(client
            .post(
                &format!("/v1/data_sources/{id}/query"),
                &query,
                Value::Object(body),
                None
            )
            .await
            .map_err(|e| e.explain(CAP_READ, NF_DATA_SOURCE)));
        let raw = params.raw.unwrap_or(false);
        let rows: Vec<Value> = list
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|page| {
                        json!({
                            "id": page.get("id").cloned().unwrap_or(Value::Null),
                            "url": page.get("url").cloned().unwrap_or(Value::Null),
                            "title": object_title(page),
                            "in_trash": in_trash(page),
                            "last_edited_time": page.get("last_edited_time").cloned().unwrap_or(Value::Null),
                            "properties": if raw {
                                page.get("properties").cloned().unwrap_or(Value::Null)
                            } else {
                                flatten_properties(page.get("properties"))
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Map::new();
        out.insert("data_source_id".into(), Value::String(id));
        out.insert("count".into(), json!(rows.len()));
        out.insert("page_size".into(), json!(page_size));
        out.insert("rows".into(), Value::Array(rows));
        out.extend(pagination(&list));
        Ok(ok_structured(Value::Object(out)))
    }

    #[tool(
        description = "List open comment threads on a page (pass the page id) or on a block. Needs the Read comments capability."
    )]
    #[tracing::instrument(name = "tool.list_comments", skip(self))]
    async fn list_comments(
        &self,
        Parameters(params): Parameters<ListCommentsParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let id = try_tool!(normalize_id(&params.block_id));
        let page_size = clamp_page_size(params.page_size, 50);
        let mut query = vec![
            ("block_id", id.clone()),
            ("page_size", page_size.to_string()),
        ];
        if let Some(cursor) = params.start_cursor.filter(|c| !c.trim().is_empty()) {
            query.push(("start_cursor", cursor));
        }
        let list = try_tool!(client
            .get("/v1/comments", &query, None)
            .await
            .map_err(|e| e.explain(CAP_READ_COMMENTS, NF_BLOCK)));
        let comments: Vec<Value> = list
            .get("results")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(comment_summary).collect())
            .unwrap_or_default();
        let mut out = Map::new();
        out.insert("block_id".into(), Value::String(id));
        out.insert("count".into(), json!(comments.len()));
        out.insert("page_size".into(), json!(page_size));
        out.insert("comments".into(), Value::Array(comments));
        out.extend(pagination(&list));
        Ok(ok_structured(Value::Object(out)))
    }

    #[tool(
        description = "Post a comment on a page or block, or reply in an existing discussion thread. Gated by NOTION_READ_ONLY; needs the Insert comments capability."
    )]
    #[tracing::instrument(name = "tool.create_comment", skip(self))]
    async fn create_comment(
        &self,
        Parameters(params): Parameters<CreateCommentParams>,
    ) -> ToolResult {
        let client = try_tool!(Client::from_env());
        if let Some(refusal) = client.refuse_write("create_comment") {
            return Ok(tool_error(refusal));
        }
        let text = params.text.trim();
        let len = text.chars().count();
        if len == 0 || len > COMMENT_MAX_CHARS {
            return Ok(tool_error(format!(
                "text must be 1..{COMMENT_MAX_CHARS} characters, got {len}"
            )));
        }
        let clean = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let targets = [
            clean(&params.page_id),
            clean(&params.block_id),
            clean(&params.discussion_id),
        ];
        if targets.iter().filter(|t| t.is_some()).count() != 1 {
            return Ok(tool_error(
                "pass exactly one of page_id, block_id, or discussion_id",
            ));
        }
        let mut body = Map::new();
        if let Some(page) = &targets[0] {
            let id = try_tool!(normalize_id(page));
            body.insert("parent".into(), json!({ "type": "page_id", "page_id": id }));
        } else if let Some(block) = &targets[1] {
            let id = try_tool!(normalize_id(block));
            body.insert(
                "parent".into(),
                json!({ "type": "block_id", "block_id": id }),
            );
        } else if let Some(discussion) = &targets[2] {
            let id = try_tool!(normalize_id(discussion));
            body.insert("discussion_id".into(), Value::String(id));
        }
        let runs = rich_text_chunks(text);
        if runs.len() > MAX_ARRAY_ITEMS {
            return Ok(tool_error(format!(
                "text splits into {} runs; Notion accepts at most {MAX_ARRAY_ITEMS}",
                runs.len()
            )));
        }
        body.insert("rich_text".into(), Value::Array(runs));
        let comment = try_tool!(client
            .post("/v1/comments", &[], Value::Object(body), None)
            .await
            .map_err(|e| e.explain(CAP_INSERT_COMMENTS, NF_COMMENT_TARGET)));
        Ok(ok_structured(json!({
            "id": comment.get("id").cloned().unwrap_or(Value::Null),
            "discussion_id": comment.get("discussion_id").cloned().unwrap_or(Value::Null),
            "parent": parent_summary(comment.get("parent")),
            "created_time": comment.get("created_time").cloned().unwrap_or(Value::Null),
        })))
    }

    #[tool(
        description = "List workspace members and bots (no guests) to resolve user ids for people properties or created_by fields. Needs the User information capability; emails appear only if that capability includes them."
    )]
    #[tracing::instrument(name = "tool.list_users", skip(self))]
    async fn list_users(&self, Parameters(params): Parameters<ListUsersParams>) -> ToolResult {
        let client = try_tool!(Client::from_env());
        let page_size = clamp_page_size(params.page_size, 50);
        let mut query = vec![("page_size", page_size.to_string())];
        if let Some(cursor) = params.start_cursor.filter(|c| !c.trim().is_empty()) {
            query.push(("start_cursor", cursor));
        }
        let list = try_tool!(client
            .get("/v1/users", &query, None)
            .await
            .map_err(|e| e.explain(CAP_USERS, NF_USER)));
        let users: Vec<Value> = list
            .get("results")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(user_entry).collect())
            .unwrap_or_default();
        let mut out = Map::new();
        out.insert("count".into(), json!(users.len()));
        out.insert("page_size".into(), json!(page_size));
        out.insert("users".into(), Value::Array(users));
        out.extend(pagination(&list));
        Ok(ok_structured(Value::Object(out)))
    }
}

/// `filter_properties=a&filter_properties=b` pairs, bounded to 100 ids.
fn filter_properties_query(ids: Option<&[String]>) -> Result<Vec<(&'static str, String)>, String> {
    let Some(ids) = ids else {
        return Ok(Vec::new());
    };
    if ids.len() > MAX_ARRAY_ITEMS {
        return Err(format!(
            "filter_properties accepts at most {MAX_ARRAY_ITEMS} ids, got {}",
            ids.len()
        ));
    }
    Ok(ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(|id| ("filter_properties", id.to_owned()))
        .collect())
}

/// Summary of the `GET /v1/users/me` bot user.
fn self_summary(me: &Value) -> Value {
    let bot = me.get("bot");
    json!({
        "id": me.get("id").cloned().unwrap_or(Value::Null),
        "name": me.get("name").cloned().unwrap_or(Value::Null),
        "type": me.get("type").cloned().unwrap_or(Value::Null),
        "workspace_name": bot.and_then(|b| b.get("workspace_name")).cloned().unwrap_or(Value::Null),
        "workspace_id": bot.and_then(|b| b.get("workspace_id")).cloned().unwrap_or(Value::Null),
        "owner_type": bot
            .and_then(|b| b.get("owner"))
            .and_then(|o| o.get("type"))
            .cloned()
            .unwrap_or(Value::Null),
        "workspace_limits": bot.and_then(|b| b.get("workspace_limits")).cloned().unwrap_or(Value::Null),
    })
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for NotionServer {
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
            "Notion MCP server (WebAssembly component on Cosmonic Desktop) speaking the \
             Notion REST API with an internal-integration token. Call `check_auth` first; a \
             missing/invalid result is a configuration problem, not a retry. Find content with \
             `search` (titles only) or a notion.so URL, read properties with `get_page` and the \
             body as Markdown with `get_page_content`. Tables: `get_database` resolves a \
             database to its data sources, `get_data_source` reads the schema, \
             `query_data_source` returns rows, `create_data_source_item` adds rows from plain \
             values. Writes (`create_page`, `update_page`, `update_page_markdown`, \
             `append_blocks`, `create_comment`) are refused when NOTION_READ_ONLY=true. A 404 \
             almost always means the page is not shared with the integration.\n\n\
             This server publishes skills — playbooks describing when and how to use its \
             tools, the error catalogue, and Notion's quirks. Read `skill://index.json` for \
             the catalog, then `skill://notion-mcp/SKILL.md`.",
        )
    }

    /// Skills over MCP: every skill file, plus the catalog, as resources.
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
