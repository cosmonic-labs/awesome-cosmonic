//! The MCP server implementation: tool definitions and result rendering.
//!
//! Every tool reads its configuration from the environment on each call,
//! validates parameters in-guest (paths, sizes, enums), sends the request
//! through [`crate::obsidian::Client`], and renders the answer as
//! `structuredContent` plus a readable text block. Upstream and policy
//! failures come back as `CallToolResult::error` so the caller sees the
//! message; JSON-RPC errors are reserved for requests the server cannot
//! route at all.
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use bytes::Bytes;
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
use serde_json::{json, Value};

use crate::obsidian::{
    self, Budget, Client, Config, Error, Reply, UrlTarget, VaultPath, ENABLE_COMMANDS_ENV,
    MAX_BATCH_FILES, MAX_BATCH_INPUT, MAX_COMMAND_ID_CHARS, MAX_CONTENT_BYTES, MAX_IF_MATCH_CHARS,
    MAX_JSONLOGIC_BYTES, MAX_QUERY_CHARS, MT_DOC_MAP, MT_JSONLOGIC, MT_MARKDOWN, MT_NOTE_JSON,
    MT_PATCH, PERIODIC_CODE_NOT_ENABLED, PERIODIC_CODE_NO_NOTE, PERIODIC_CODE_UNKNOWN_PERIOD,
    READ_ONLY_ENV,
};
use crate::skills;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so do not keep per-session
/// state on this struct.
#[derive(Clone)]
pub struct ObsidianServer {
    tool_router: ToolRouter<Self>,
}

// --- parameter types --------------------------------------------------------

/// Representation of a note returned by the read tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// The raw markdown text (default).
    Markdown,
    /// Parsed metadata: path, content, frontmatter, tags, links, backlinks,
    /// unresolvedLinks and stat {ctime, mtime, size} (epoch milliseconds).
    Metadata,
    /// Structure only: headings tree, block references, frontmatter fields
    /// and the document version token used by patch_content if_match.
    DocumentMap,
}

impl Format {
    fn accept(self) -> &'static str {
        match self {
            Format::Markdown => MT_MARKDOWN,
            Format::Metadata => MT_NOTE_JSON,
            Format::DocumentMap => MT_DOC_MAP,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Format::Markdown => "markdown",
            Format::Metadata => "metadata",
            Format::DocumentMap => "document_map",
        }
    }
}

/// Which part of a targeted section a read or patch applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    /// The text under the heading / at the block, without the marker line
    /// itself (default).
    Content,
    /// Only the marker line (the heading text or the block reference).
    Marker,
    /// Marker and content together.
    MarkerAndContent,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Scope::Content => "content",
            Scope::Marker => "marker",
            Scope::MarkerAndContent => "markerAndContent",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDirParams {
    /// Vault-relative folder path such as `Projects` or `Projects/Archive`
    /// (no leading `/`, no `..`; a trailing `/` is accepted). An empty
    /// string lists the vault root.
    pub dirpath: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileParams {
    /// Vault-relative note path such as `Projects/Plan.md`.
    pub filepath: String,
    /// `markdown` (default), `metadata`, or `document_map`.
    pub format: Option<Format>,
    /// Read only the section under this heading path, from the top-level
    /// heading down, e.g. `["Projects", "Q3"]`. Each element is one heading
    /// text (a `/` inside a heading is fine).
    pub heading: Option<Vec<String>>,
    /// Read only the paragraph carrying this block reference id (bare id,
    /// without the `^`).
    pub block: Option<String>,
    /// Read only this frontmatter field.
    pub frontmatter_key: Option<String>,
    /// With a heading/block target: `content` (default), `marker`, or
    /// `markerAndContent`.
    pub scope: Option<Scope>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchGetParams {
    /// Vault-relative note paths: at most 200 entries (longer lists are
    /// refused); duplicates are removed and only the first 20 distinct paths
    /// are read (`clamped: true` when more were given). A missing note
    /// becomes an inline `Error 404` entry and does not abort the batch.
    pub filepaths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimpleSearchParams {
    /// Full-text query (1..1000 characters). Matches note basenames too.
    pub query: String,
    /// Characters of context around each match (default 100, clamped 0..1000).
    pub context_length: Option<i64>,
    /// Maximum number of files returned (default 20, clamped 1..100). The
    /// plugin itself returns every match; the server cuts the list and sets
    /// `truncated`.
    pub limit: Option<i64>,
    /// Maximum matches kept per file (default 10, clamped 1..50).
    pub max_matches_per_file: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ComplexSearchParams {
    /// A JsonLogic query object evaluated against each note's metadata
    /// (`path`, `tags` (no `#`), `frontmatter.*`, `stat.mtime`/`ctime`/`size`
    /// in epoch ms, `links`, `backlinks`; `content` only when the query text
    /// literally mentions "content"). Operators: var, ==, !=, in, glob,
    /// regexp, and, or, if, <, <=, >, >=. Example:
    /// `{"in": ["project", {"var": "tags"}]}`. Serialized size <= 64 KiB.
    pub query: Value,
    /// Maximum results (default 100, clamped 1..500); the total is reported.
    pub limit: Option<i64>,
}

impl std::fmt::Debug for ComplexSearchParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComplexSearchParams")
            .field("query_bytes", &self.query.to_string().len())
            .field("limit", &self.limit)
            .finish()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentChangesParams {
    /// Look back this many days (default 90, clamped 1..3650).
    pub days: Option<i64>,
    /// Maximum notes returned, newest first (default 10, clamped 1..100).
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTagsParams {
    /// Maximum tags returned (default 200, clamped 1..2000).
    pub limit: Option<i64>,
    /// Keep only tags starting with this prefix (case-insensitive, no `#`).
    pub prefix: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActiveFileParams {
    /// `markdown` (default) or `metadata`.
    pub format: Option<Format>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AppendParams {
    /// Vault-relative note path. Created (with parent folders) if missing.
    /// Must end in `.md` unless `allow_non_markdown` is true.
    pub filepath: String,
    /// Markdown to append (1 byte .. 1 MiB). The plugin adds a newline
    /// before it.
    pub content: String,
    /// Append inside this heading section instead of at the end of the note
    /// (heading path from the top level down).
    pub heading: Option<Vec<String>>,
    /// Refuse when the exact content is already in the note ("already
    /// present" = already done). With `heading` the plugin enforces it
    /// (HTTP 409); without one the server reads the note first and refuses
    /// in-guest, because the plugin ignores the flag on whole-note appends.
    /// Costs one extra request and is not atomic.
    pub reject_if_content_preexists: Option<bool>,
    /// Allow a path that does not end in `.md`.
    pub allow_non_markdown: Option<bool>,
}

impl std::fmt::Debug for AppendParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppendParams")
            .field("filepath", &self.filepath)
            .field("content_bytes", &self.content.len())
            .field("heading", &self.heading)
            .field(
                "reject_if_content_preexists",
                &self.reject_if_content_preexists,
            )
            .field("allow_non_markdown", &self.allow_non_markdown)
            .finish()
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct PutParams {
    /// Vault-relative note path. Created (with parent folders) if missing,
    /// fully overwritten otherwise.
    pub filepath: String,
    /// The complete new note text (0 .. 1 MiB).
    pub content: String,
    /// Abort instead of creating the note when it does not exist yet.
    pub require_existing: Option<bool>,
}

impl std::fmt::Debug for PutParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PutParams")
            .field("filepath", &self.filepath)
            .field("content_bytes", &self.content.len())
            .field("require_existing", &self.require_existing)
            .finish()
    }
}

/// Patch operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Append,
    Prepend,
    Replace,
    /// Remove the target (plugin >= 5.0 only).
    Delete,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Operation::Append => "append",
            Operation::Prepend => "prepend",
            Operation::Replace => "replace",
            Operation::Delete => "delete",
        }
    }
}

/// What a patch addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetType {
    Heading,
    Block,
    Frontmatter,
}

impl TargetType {
    fn name(self) -> &'static str {
        match self {
            TargetType::Heading => "heading",
            TargetType::Block => "block",
            TargetType::Frontmatter => "frontmatter",
        }
    }
}

/// A heading path (array) or a single target text. A heading given as one
/// string is split on `::` for compatibility with older clients.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TargetSpec {
    Path(Vec<String>),
    Text(String),
}

#[derive(Deserialize, JsonSchema)]
pub struct PatchParams {
    /// Vault-relative note path.
    pub filepath: String,
    /// `append`, `prepend`, `replace`, or `delete`.
    pub operation: Operation,
    /// `heading`, `block`, or `frontmatter`.
    pub target_type: TargetType,
    /// heading: an array from the top-level heading down (`["Projects",
    /// "Q3"]`; `"Projects::Q3"` is accepted and split); block: the bare
    /// block id; frontmatter: the field name.
    pub target: TargetSpec,
    /// Markdown/text payload. Do not include the heading line itself: the
    /// plugin keeps it. Exactly one of `content` / `value` unless
    /// operation is `delete`.
    pub content: Option<String>,
    /// JSON payload for frontmatter fields (typed values, lists) or table
    /// rows.
    pub value: Option<Value>,
    /// `content` (default), `marker` (e.g. rename a heading), or
    /// `markerAndContent`.
    pub scope: Option<Scope>,
    /// Plugin-specific sub-target selector (see references/PATCH.md).
    pub within: Option<i64>,
    /// Create the heading/field when it does not exist instead of 404.
    pub create_target_if_missing: Option<bool>,
    /// Document version token from `get_file_contents format=document_map`;
    /// the patch fails with 412 if the note changed since.
    pub if_match: Option<String>,
}

impl std::fmt::Debug for PatchParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatchParams")
            .field("filepath", &self.filepath)
            .field("operation", &self.operation)
            .field("target_type", &self.target_type)
            .field("target", &self.target)
            .field("content_bytes", &self.content.as_ref().map(String::len))
            .field("has_value", &self.value.is_some())
            .field("scope", &self.scope)
            .field("within", &self.within)
            .field("create_target_if_missing", &self.create_target_if_missing)
            .field("if_match", &self.if_match)
            .finish()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteParams {
    /// Vault-relative note path (folders cannot be deleted through the API).
    pub filepath: String,
    /// Bypass Obsidian's trash and delete permanently (default false).
    pub permanent: Option<bool>,
    /// Must be `true`; without it nothing is sent.
    pub confirm: Option<bool>,
}

/// A periodic-note period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Daily,
    Weekly,
    Monthly,
    Quarterly,
    Yearly,
}

impl Period {
    fn name(self) -> &'static str {
        match self {
            Period::Daily => "daily",
            Period::Weekly => "weekly",
            Period::Monthly => "monthly",
            Period::Quarterly => "quarterly",
            Period::Yearly => "yearly",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeriodicParams {
    /// `daily`, `weekly`, `monthly`, `quarterly`, or `yearly`.
    pub period: Period,
    /// `YYYY-MM-DD`; default today (UTC).
    pub date: Option<String>,
    /// `markdown` (default) or `metadata`.
    pub format: Option<Format>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentPeriodicParams {
    /// `daily`, `weekly`, `monthly`, `quarterly`, or `yearly`.
    pub period: Period,
    /// Number of periods to look back, including the current one (default
    /// 5, clamped 1..10). Each is one lookup; missing notes are skipped.
    pub limit: Option<i64>,
    /// Include each note's text (clamped to 10 000 characters each).
    pub include_content: Option<bool>,
    /// The user's UTC offset in minutes (e.g. 120 for Berlin in summer, -420
    /// for Los Angeles in summer; -840..840, default 0 = UTC). Decides which
    /// calendar day the dated look-backs start from; the current period
    /// itself is always resolved by the plugin in Obsidian's local time.
    pub tz_offset_minutes: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpenFileParams {
    /// Vault-relative note path. A missing note is CREATED by Obsidian.
    pub filepath: String,
    /// Open in a new pane instead of the active one.
    pub new_leaf: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCommandsParams {
    /// Case-insensitive substring filter on command id or name.
    pub filter: Option<String>,
    /// Maximum commands returned (default 100, clamped 1..1000).
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteCommandParams {
    /// Command id from list_commands, e.g. `editor:toggle-bold` (1..200
    /// characters).
    pub command_id: String,
}

// --- small helpers ----------------------------------------------------------

/// A structured result with a hand-written readable text block instead of
/// the JSON dump `CallToolResult::structured` would produce.
fn shaped(value: Value, text: impl Into<String>) -> CallToolResult {
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

fn failed(err: &Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

fn finish(outcome: Result<CallToolResult, Error>) -> Result<CallToolResult, ErrorData> {
    Ok(outcome.unwrap_or_else(|err| failed(&err)))
}

fn clamp(value: Option<i64>, default: i64, min: i64, max: i64) -> usize {
    value.unwrap_or(default).clamp(min, max) as usize
}

fn require_writes(cfg: &Config) -> Result<(), Error> {
    if cfg.read_only {
        return Err(Error::Gated(format!(
            "this server is deployed read-only ({READ_ONLY_ENV}=true): append_content, \
             put_content, patch_content, delete_file, open_file and execute_command are \
             refused. Ask the operator to set {READ_ONLY_ENV}=false in deploy/workload.yaml; \
             read tools keep working."
        )));
    }
    Ok(())
}

fn require_commands(cfg: &Config) -> Result<(), Error> {
    if !cfg.commands_enabled {
        return Err(Error::Gated(format!(
            "command execution is disabled; set {ENABLE_COMMANDS_ENV}=true in \
             deploy/workload.yaml to allow list_commands/execute_command (Obsidian commands \
             can do anything the app can, so this is off by default)."
        )));
    }
    Ok(())
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn single_target(spec: &TargetSpec, kind: &str) -> Result<String, Error> {
    match spec {
        TargetSpec::Text(text) => Ok(text.clone()),
        TargetSpec::Path(items) if items.len() == 1 => Ok(items[0].clone()),
        TargetSpec::Path(_) => Err(Error::invalid(format!(
            "{kind} targets take a single string, not an array"
        ))),
    }
}

fn looks_binary(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("image/")
        || ct.starts_with("audio/")
        || ct.starts_with("video/")
        || ct.starts_with("application/octet-stream")
        || ct.starts_with("application/pdf")
        || ct.starts_with("application/zip")
}

/// What a read tool got back, already bounded.
struct NoteRead {
    content: Option<String>,
    data: Option<Value>,
    truncated: bool,
    content_type: String,
}

impl NoteRead {
    fn from_reply(
        reply: &Reply,
        format: Format,
        display: &str,
        max_chars: usize,
    ) -> Result<Self, Error> {
        let content_type = reply.content_type();
        match format {
            Format::Markdown => {
                if looks_binary(&content_type) {
                    return Err(Error::invalid(format!(
                        "'{display}' is not a text note: Obsidian returned {content_type} \
                         ({} bytes). This server does not dump binary attachments.",
                        reply.body.len()
                    )));
                }
                let text = reply.text().ok_or_else(|| {
                    Error::invalid(format!(
                        "'{display}' is not valid UTF-8 text ({content_type}, {} bytes); \
                         binary attachments are not returned.",
                        reply.body.len()
                    ))
                })?;
                let (content, truncated) = obsidian::clamp_marked(&text, max_chars);
                Ok(NoteRead {
                    content: Some(content),
                    data: None,
                    truncated,
                    content_type,
                })
            }
            Format::Metadata | Format::DocumentMap => {
                let mut data = reply.json().ok_or_else(|| Error::Malformed {
                    path: display.to_owned(),
                    detail: format!(
                        "expected {} JSON, got {content_type} ({} bytes)",
                        format.name(),
                        reply.body.len()
                    ),
                })?;
                let mut truncated = false;
                if let Some(Value::String(text)) = data.get("content") {
                    let (clamped, cut) = obsidian::clamp_marked(text, max_chars);
                    if cut {
                        data["content"] = Value::String(clamped);
                        truncated = true;
                    }
                }
                Ok(NoteRead {
                    content: None,
                    data: Some(data),
                    truncated,
                    content_type,
                })
            }
        }
    }

    fn text_block(&self) -> String {
        if let Some(content) = &self.content {
            return content.clone();
        }
        self.data
            .as_ref()
            .and_then(|d| serde_json::to_string_pretty(d).ok())
            .unwrap_or_default()
    }
}

/// Where a periodic-note redirect pointed.
fn resolve_location(base_url: &str, location: &str) -> Result<String, Error> {
    if location.starts_with('/') {
        return Ok(location.to_owned());
    }
    if let Some(rest) = location.strip_prefix(base_url) {
        if rest.starts_with('/') {
            return Ok(rest.to_owned());
        }
    }
    Err(Error::invalid(format!(
        "the periodic-note redirect pointed off-origin ({location}); refusing to follow it"
    )))
}

// --- tools ------------------------------------------------------------------

#[tool_router]
impl ObsidianServer {
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
        description = "Verify the Obsidian API key and connectivity: status ok|missing|invalid|unreachable, plugin and Obsidian versions, and the exact remediation. Call this first; never retry a missing/invalid result."
    )]
    #[tracing::instrument(name = "tool.check_auth", skip(self))]
    async fn check_auth(&self) -> Result<CallToolResult, ErrorData> {
        finish(self.do_check_auth().await)
    }

    #[tool(
        description = "Plugin status document (GET /): service, plugin and Obsidian versions, whether the API key authenticated, the PATCH format this server will use, and setup hints. Refreshes the cached plugin version."
    )]
    #[tracing::instrument(name = "tool.get_server_info", skip(self))]
    async fn get_server_info(&self) -> Result<CallToolResult, ErrorData> {
        finish(self.do_server_info().await)
    }

    #[tool(
        description = "List the entries at the vault root: notes as 'name.md', folders as 'dir/'. Empty folders never appear."
    )]
    #[tracing::instrument(name = "tool.list_files_in_vault", skip(self))]
    async fn list_files_in_vault(&self) -> Result<CallToolResult, ErrorData> {
        finish(self.do_list_dir(String::new()).await)
    }

    #[tool(
        description = "List the entries directly under a vault folder (notes as 'name.md', sub-folders as 'dir/'). The plugin answers 404 for a missing folder AND for a folder with no files."
    )]
    #[tracing::instrument(name = "tool.list_files_in_dir", skip(self))]
    async fn list_files_in_dir(
        &self,
        Parameters(params): Parameters<ListDirParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_list_dir(params.dirpath).await)
    }

    #[tool(
        description = "Read one note as markdown (default), its parsed metadata (frontmatter, tags, links, stat) or its document map (headings, blocks, version token); optionally only one heading section, block, or frontmatter field. Output is clamped to OBSIDIAN_MAX_CONTENT_CHARS."
    )]
    #[tracing::instrument(name = "tool.get_file_contents", skip(self))]
    async fn get_file_contents(
        &self,
        Parameters(params): Parameters<GetFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_get_file(params).await)
    }

    #[tool(
        description = "Read up to 20 notes in one call, concatenated as '# path' sections separated by '---'. A missing note becomes an inline 'Error 404' entry; the batch continues."
    )]
    #[tracing::instrument(name = "tool.batch_get_file_contents", skip(self))]
    async fn batch_get_file_contents(
        &self,
        Parameters(params): Parameters<BatchGetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_batch_get(params).await)
    }

    #[tool(
        description = "Full-text search across all notes (matches basenames too), ranked by score with context snippets. Results are unbounded upstream; this tool clamps files and matches per file and reports truncation."
    )]
    #[tracing::instrument(name = "tool.simple_search", skip(self))]
    async fn simple_search(
        &self,
        Parameters(params): Parameters<SimpleSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_simple_search(params).await)
    }

    #[tool(
        description = "JsonLogic query over note metadata (path, tags, frontmatter, stat, links) with glob/regexp operators; returns each note whose result is truthy. Result values are truncated to 2000 characters."
    )]
    #[tracing::instrument(name = "tool.complex_search", skip(self))]
    async fn complex_search(
        &self,
        Parameters(params): Parameters<ComplexSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_complex_search(params).await)
    }

    #[tool(
        description = "Recently modified notes, newest first, with ISO-8601 modification times (a JsonLogic query on stat.mtime; plugin 5.x has no Dataview search)."
    )]
    #[tracing::instrument(name = "tool.get_recent_changes", skip(self))]
    async fn get_recent_changes(
        &self,
        Parameters(params): Parameters<RecentChangesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_recent_changes(params).await)
    }

    #[tool(
        description = "All tags in the vault with usage counts (inline and frontmatter; nested tags also count toward their parents), optionally filtered by prefix."
    )]
    #[tracing::instrument(name = "tool.list_tags", skip(self))]
    async fn list_tags(
        &self,
        Parameters(params): Parameters<ListTagsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_list_tags(params).await)
    }

    #[tool(
        description = "The note currently focused in the Obsidian window: its vault path and content (or metadata). 404 when no note is active."
    )]
    #[tracing::instrument(name = "tool.get_active_file", skip(self))]
    async fn get_active_file(
        &self,
        Parameters(params): Parameters<ActiveFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_active_file(params).await)
    }

    #[tool(
        description = "Append markdown to the end of a note (creating the note and parent folders if missing) or inside a heading section. Gated by OBSIDIAN_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.append_content", skip(self))]
    async fn append_content(
        &self,
        Parameters(params): Parameters<AppendParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_append(params).await)
    }

    #[tool(
        description = "Create or completely overwrite a note with the given text (parent folders are created). Set require_existing to refuse creating a new note. Gated by OBSIDIAN_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.put_content", skip(self))]
    async fn put_content(
        &self,
        Parameters(params): Parameters<PutParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_put(params).await)
    }

    #[tool(
        description = "Structured edit of one target in a note: append/prepend/replace/delete under a heading path, at a block reference, or on a frontmatter field; rename via scope=marker; optimistic concurrency via if_match. Uses the plugin's JSON patch instruction (5.x) or legacy headers (4.x). Gated by OBSIDIAN_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.patch_content", skip(self))]
    async fn patch_content(
        &self,
        Parameters(params): Parameters<PatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_patch(params).await)
    }

    #[tool(
        description = "Delete a note: to Obsidian's trash by default (per the user's 'Deleted files' setting) or permanently. Requires confirm=true; folders cannot be deleted. Gated by OBSIDIAN_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.delete_file", skip(self))]
    async fn delete_file(
        &self,
        Parameters(params): Parameters<DeleteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_delete(params).await)
    }

    #[tool(
        description = "Read today's (or a dated) daily/weekly/monthly/quarterly/yearly note. Needs the companion plugin 'Local REST API - Periodic Notes' plus a configured Daily Notes / Periodic Notes plugin."
    )]
    #[tracing::instrument(name = "tool.get_periodic_note", skip(self))]
    async fn get_periodic_note(
        &self,
        Parameters(params): Parameters<PeriodicParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_periodic(params).await)
    }

    #[tool(
        description = "The most recent periodic notes for a period (paths, optionally content), computed by stepping back from today; missing periods are skipped and counted."
    )]
    #[tracing::instrument(name = "tool.get_recent_periodic_notes", skip(self))]
    async fn get_recent_periodic_notes(
        &self,
        Parameters(params): Parameters<RecentPeriodicParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_recent_periodic(params).await)
    }

    #[tool(
        description = "Open a note in the Obsidian window (CREATES the note if it does not exist). Gated by OBSIDIAN_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.open_file", skip(self))]
    async fn open_file(
        &self,
        Parameters(params): Parameters<OpenFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_open(params).await)
    }

    #[tool(
        description = "List Obsidian command-palette commands (id + name) for execute_command. Gated by OBSIDIAN_ENABLE_COMMANDS."
    )]
    #[tracing::instrument(name = "tool.list_commands", skip(self))]
    async fn list_commands(
        &self,
        Parameters(params): Parameters<ListCommandsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_list_commands(params).await)
    }

    #[tool(
        description = "Run one Obsidian command by id (e.g. editor:toggle-bold). Arbitrary app-level side effects. Gated by OBSIDIAN_ENABLE_COMMANDS and OBSIDIAN_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.execute_command", skip(self))]
    async fn execute_command(
        &self,
        Parameters(params): Parameters<ExecuteCommandParams>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(self.do_execute_command(params).await)
    }
}

// --- tool bodies ------------------------------------------------------------

impl ObsidianServer {
    async fn do_check_auth(&self) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::new(&cfg)?;
        if !client.has_key() {
            let remediation = Error::MissingKey {
                placeholder: cfg.key_is_placeholder,
            }
            .message();
            return Ok(shaped(
                json!({
                    "status": "missing",
                    "placeholder": cfg.key_is_placeholder,
                    "env": obsidian::API_KEY_ENV,
                    "ref": obsidian::SECRET_REF,
                    "base_url": client.base_url(),
                    "remediation": remediation,
                }),
                format!("status: missing — {remediation}"),
            ));
        }
        match client.server_info().await {
            Ok(info) => {
                let authenticated = info
                    .get("authenticated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let plugin = info
                    .get("versions")
                    .and_then(|v| v.get("self"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned();
                let app = info
                    .get("versions")
                    .and_then(|v| v.get("obsidian"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned();
                let identity = json!({
                    "service": info.get("service").cloned().unwrap_or(Value::Null),
                    "plugin_version": plugin,
                    "obsidian_version": app,
                    "base_url": client.base_url(),
                });
                if authenticated {
                    Ok(shaped(
                        json!({
                            "status": "ok",
                            "env": obsidian::API_KEY_ENV,
                            "ref": obsidian::SECRET_REF,
                            "identity": identity,
                            "read_only": cfg.read_only,
                            "commands_enabled": cfg.commands_enabled,
                            "patch_format": patch_format_name(obsidian::cached_plugin_major()),
                        }),
                        format!(
                            "status: ok — Obsidian Local REST API {plugin} on Obsidian {app} at {} \
                             (read_only={}, commands_enabled={})",
                            client.base_url(),
                            cfg.read_only,
                            cfg.commands_enabled
                        ),
                    ))
                } else {
                    let remediation = format!(
                        "The plugin answered but reported authenticated=false: the key in the \
                         `{}` secret (env {}) is wrong or stale. {}",
                        obsidian::SECRET_REF,
                        obsidian::API_KEY_ENV,
                        obsidian::setup_hint()
                    );
                    Ok(shaped(
                        json!({
                            "status": "invalid",
                            "env": obsidian::API_KEY_ENV,
                            "ref": obsidian::SECRET_REF,
                            "identity": identity,
                            "remediation": remediation,
                        }),
                        format!("status: invalid — {remediation}"),
                    ))
                }
            }
            Err(err) => {
                let remediation = err.message();
                Ok(shaped(
                    json!({
                        "status": "unreachable",
                        "env": obsidian::API_KEY_ENV,
                        "ref": obsidian::SECRET_REF,
                        "base_url": client.base_url(),
                        "retryable": err.retryable(),
                        "remediation": remediation,
                    }),
                    format!("status: unreachable — {remediation}"),
                ))
            }
        }
    }

    async fn do_server_info(&self) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::new(&cfg)?;
        let info = client.server_info().await?;
        let authenticated = info
            .get("authenticated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let major = obsidian::cached_plugin_major();
        let mut hints: Vec<String> = Vec::new();
        if !client.has_key() {
            hints.push(
                Error::MissingKey {
                    placeholder: cfg.key_is_placeholder,
                }
                .message(),
            );
        } else if !authenticated {
            hints.push(format!(
                "authenticated=false: the API key was not accepted. {}",
                obsidian::setup_hint()
            ));
        }
        match major {
            Some(m) if m < 5 => hints.push(
                "plugin < 5.0: patch_content uses the deprecated header format, /periodic/ is \
                 served natively, and operation=delete is unavailable. Upgrade the plugin."
                    .to_owned(),
            ),
            Some(_) => hints.push(
                "plugin >= 5.0: patch_content sends JSON patch instructions; periodic notes \
                 need the companion plugin 'Local REST API - Periodic Notes'."
                    .to_owned(),
            ),
            None => hints.push("plugin version not reported; assuming 5.x".to_owned()),
        }
        let versions = info.get("versions").cloned().unwrap_or(Value::Null);
        let text = format!(
            "{} {} on Obsidian {} at {} — authenticated: {} — patch format: {}",
            info.get("service").and_then(Value::as_str).unwrap_or("?"),
            versions.get("self").and_then(Value::as_str).unwrap_or("?"),
            versions
                .get("obsidian")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            client.base_url(),
            authenticated,
            patch_format_name(major)
        );
        Ok(shaped(
            json!({
                "service": info.get("service").cloned().unwrap_or(Value::Null),
                "status": info.get("status").cloned().unwrap_or(Value::Null),
                "versions": versions,
                "authenticated": authenticated,
                "key_configured": client.has_key(),
                "base_url": client.base_url(),
                "patch_format": patch_format_name(major),
                "read_only": cfg.read_only,
                "commands_enabled": cfg.commands_enabled,
                "max_content_chars": cfg.max_content_chars,
                "hints": hints,
            }),
            text,
        ))
    }

    async fn do_list_dir(&self, dirpath: String) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let path = obsidian::vault_path(&dirpath, true)?;
        let url = if path.encoded.is_empty() {
            "/vault/".to_owned()
        } else {
            format!("/vault/{}/", path.encoded)
        };
        let reply = client
            .send(http::Method::GET, &url, &[], Bytes::new())
            .await?;
        let reply = match Client::ok(reply, &url) {
            Ok(reply) => reply,
            Err(Error::Api {
                status: 404,
                message,
                ..
            }) => {
                return Err(Error::invalid(format!(
                    "folder '{}' is empty or does not exist (HTTP 404: {message}). The plugin \
                     answers 404 for both; list the parent folder to tell them apart (an \
                     existing empty folder still shows as 'name/' there).",
                    path.display
                )))
            }
            Err(err) => return Err(err),
        };
        let body = reply.json().ok_or_else(|| Error::Malformed {
            path: url.clone(),
            detail: "expected {\"files\": [...]}".to_owned(),
        })?;
        let files = string_list(&body, "files");
        let folders: Vec<&String> = files.iter().filter(|f| f.ends_with('/')).collect();
        let notes: Vec<&String> = files.iter().filter(|f| !f.ends_with('/')).collect();
        let text = if files.is_empty() {
            format!("{}: (no entries)", display_dir(&path))
        } else {
            format!("{}:\n{}", display_dir(&path), files.join("\n"))
        };
        Ok(shaped(
            json!({
                "directory": path.display,
                "entries": files,
                "count": files.len(),
                "folders": folders,
                "files": notes,
            }),
            text,
        ))
    }

    async fn do_get_file(&self, p: GetFileParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let path = obsidian::vault_path(&p.filepath, false)?;
        let format = p.format.unwrap_or(Format::Markdown);
        let target = url_target(p.heading, p.block, p.frontmatter_key)?;
        if target.is_some() && format != Format::Markdown {
            return Err(Error::invalid(
                "heading/block/frontmatter targets only apply to format=markdown",
            ));
        }
        let mut url = format!("/vault/{}", path.encoded);
        let mut headers = vec![("Accept", format.accept().to_owned())];
        if let Some(target) = &target {
            url.push_str(&target.suffix());
            if let Some(scope) = p.scope {
                headers.push(("Target-Scope", scope.name().to_owned()));
            }
        }
        let reply = client
            .send(http::Method::GET, &url, &headers, Bytes::new())
            .await?;
        let reply = Client::ok(reply, &url)?;
        let read = NoteRead::from_reply(&reply, format, &path.display, cfg.max_content_chars)?;
        Ok(shaped(
            json!({
                "path": path.display,
                "format": format.name(),
                "target": target.as_ref().map(UrlTarget::describe),
                "scope": p.scope.map(Scope::name),
                "content": read.content,
                "data": read.data,
                "truncated": read.truncated,
                "content_type": read.content_type,
            }),
            read.text_block(),
        ))
    }

    async fn do_batch_get(&self, p: BatchGetParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        if p.filepaths.is_empty() {
            return Err(Error::invalid("filepaths must contain at least one path"));
        }
        let requested = p.filepaths.len();
        if requested > MAX_BATCH_INPUT {
            return Err(Error::invalid(format!(
                "filepaths has {requested} entries; send at most {MAX_BATCH_INPUT} (only the \
                 first {MAX_BATCH_FILES} distinct paths are read per call — split the list \
                 into several calls). Nothing was sent."
            )));
        }
        // Bounded work: at most MAX_BATCH_INPUT entries are examined and the
        // scan stops at the first distinct path beyond MAX_BATCH_FILES.
        let mut unique: Vec<String> = Vec::with_capacity(MAX_BATCH_FILES);
        let mut clamped = false;
        for path in p.filepaths {
            if unique.contains(&path) {
                continue;
            }
            if unique.len() == MAX_BATCH_FILES {
                clamped = true;
                break;
            }
            unique.push(path);
        }
        let budget = Budget::start();
        let mut text = String::new();
        let mut files = Vec::new();
        let mut total_chars = 0usize;
        let mut truncated_total = false;
        let mut not_attempted = 0usize;
        for raw in &unique {
            if budget.spent() {
                // Earlier reads used up the call's wall-clock budget: report
                // the rest as not attempted instead of holding the instance.
                not_attempted += 1;
                let detail = format!(
                    "not attempted: the call's {} ms time budget was spent by the earlier \
                     reads ({} ms elapsed); re-issue the batch with the remaining paths",
                    budget.limit_ms(),
                    budget.elapsed_ms()
                );
                files.push(json!({"path": raw, "ok": false, "skipped": true, "error": detail}));
                text.push_str(&format!("# {raw}\n\nError: {detail}\n\n---\n\n"));
                continue;
            }
            let entry = match obsidian::vault_path(raw, false) {
                Ok(path) => {
                    let url = format!("/vault/{}", path.encoded);
                    match client
                        .send(
                            http::Method::GET,
                            &url,
                            &[("Accept", MT_MARKDOWN.to_owned())],
                            Bytes::new(),
                        )
                        .await
                    {
                        Ok(reply) if reply.is_success() => {
                            match NoteRead::from_reply(
                                &reply,
                                Format::Markdown,
                                &path.display,
                                cfg.max_content_chars,
                            ) {
                                Ok(read) => {
                                    let content = read.content.unwrap_or_default();
                                    let chars = content.chars().count();
                                    files.push(json!({
                                        "path": path.display,
                                        "ok": true,
                                        "status": reply.status,
                                        "chars": chars,
                                        "truncated": read.truncated,
                                    }));
                                    (path.display.clone(), content)
                                }
                                Err(err) => {
                                    files.push(json!({"path": path.display, "ok": false, "error": err.message()}));
                                    (path.display.clone(), format!("Error: {}", err.message()))
                                }
                            }
                        }
                        Ok(reply) => {
                            let status = reply.status;
                            let err = Client::ok(reply, &url)
                                .err()
                                .map(|e| e.message())
                                .unwrap_or_default();
                            files.push(json!({"path": path.display, "ok": false, "status": status, "error": err}));
                            (path.display.clone(), format!("Error {status}: {err}"))
                        }
                        Err(err) => {
                            // Credential/transport failures affect every file:
                            // stop rather than emit twenty identical errors.
                            if matches!(err, Error::Unauthorized { .. } | Error::Transport { .. }) {
                                return Err(err);
                            }
                            files.push(
                                json!({"path": path.display, "ok": false, "error": err.message()}),
                            );
                            (path.display.clone(), format!("Error: {}", err.message()))
                        }
                    }
                }
                Err(err) => {
                    files.push(json!({"path": raw, "ok": false, "error": err.message()}));
                    (raw.clone(), format!("Error: {}", err.message()))
                }
            };
            if total_chars < cfg.max_content_chars * 2 {
                text.push_str(&format!("# {}\n\n{}\n\n---\n\n", entry.0, entry.1));
                total_chars += entry.1.chars().count();
            } else {
                truncated_total = true;
                text.push_str(&format!(
                    "# {}\n\n...[omitted: batch output limit reached]\n\n---\n\n",
                    entry.0
                ));
            }
        }
        let count = files.len();
        Ok(shaped(
            json!({
                "requested": requested,
                "count": count,
                "clamped_to": MAX_BATCH_FILES,
                "clamped": clamped,
                "not_attempted": not_attempted,
                "budget_ms": budget.limit_ms(),
                "elapsed_ms": budget.elapsed_ms(),
                "total_chars": total_chars,
                "truncated": truncated_total,
                "files": files,
            }),
            text,
        ))
    }

    async fn do_simple_search(&self, p: SimpleSearchParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let query = p.query;
        if query.trim().is_empty() {
            return Err(Error::invalid("query must not be empty"));
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(Error::invalid(format!(
                "query is longer than {MAX_QUERY_CHARS} characters"
            )));
        }
        let context_length = clamp(p.context_length, 100, 0, 1000);
        let limit = clamp(p.limit, 20, 1, 100);
        let max_matches = clamp(p.max_matches_per_file, 10, 1, 50);
        let url = format!(
            "/search/simple/?query={}&contextLength={context_length}",
            obsidian::encode_segment(&query)
        );
        let reply = client
            .send(http::Method::POST, &url, &[], Bytes::new())
            .await?;
        let reply = Client::ok(reply, &url)?;
        let hits = reply
            .json()
            .and_then(|v| v.as_array().cloned())
            .ok_or_else(|| Error::Malformed {
                path: "/search/simple/".to_owned(),
                detail: "expected a JSON array of {filename, score, matches}".to_owned(),
            })?;
        let total_files = hits.len();
        let snippet_max = context_length * 2 + 200;
        let mut results = Vec::new();
        let mut lines = Vec::new();
        for hit in hits.iter().take(limit) {
            let filename = hit
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let score = hit.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let matches_all = hit
                .get("matches")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let total_matches = matches_all.len();
            let matches: Vec<Value> = matches_all
                .iter()
                .take(max_matches)
                .map(|m| {
                    let context = m.get("context").and_then(Value::as_str).unwrap_or("");
                    let (context, _) = obsidian::clamp_marked(context, snippet_max);
                    json!({
                        "source": m.get("match").and_then(|x| x.get("source")).cloned().unwrap_or(Value::Null),
                        "start": m.get("match").and_then(|x| x.get("start")).cloned().unwrap_or(Value::Null),
                        "end": m.get("match").and_then(|x| x.get("end")).cloned().unwrap_or(Value::Null),
                        "context": context,
                    })
                })
                .collect();
            let first = matches
                .first()
                .and_then(|m| m.get("context").and_then(Value::as_str))
                .unwrap_or("")
                .replace('\n', " ");
            lines.push(format!(
                "{filename} (score {score:.2}, {total_matches} matches): {first}"
            ));
            results.push(json!({
                "filename": filename,
                "score": score,
                "total_matches": total_matches,
                "matches": matches,
            }));
        }
        let truncated = total_files > limit;
        let mut text = if lines.is_empty() {
            format!("no notes match {query:?}")
        } else {
            lines.join("\n")
        };
        if truncated {
            text.push_str(&format!(
                "\n...[{total_files} files matched; showing {limit} — narrow the query]"
            ));
        }
        Ok(shaped(
            json!({
                "query": query,
                "context_length": context_length,
                "limit": limit,
                "max_matches_per_file": max_matches,
                "total_files": total_files,
                "returned": results.len(),
                "truncated": truncated,
                "results": results,
            }),
            text,
        ))
    }

    async fn do_complex_search(&self, p: ComplexSearchParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        if !p.query.is_object() {
            return Err(Error::invalid(
                "query must be a JSON object (a JsonLogic expression such as \
                 {\"in\": [\"project\", {\"var\": \"tags\"}]})",
            ));
        }
        let body = p.query.to_string();
        if body.len() > MAX_JSONLOGIC_BYTES {
            return Err(Error::invalid(format!(
                "query serializes to {} bytes; the limit is {MAX_JSONLOGIC_BYTES}",
                body.len()
            )));
        }
        let limit = clamp(p.limit, 100, 1, 500);
        let url = "/search/";
        let reply = client
            .send(
                http::Method::POST,
                url,
                &[("Content-Type", MT_JSONLOGIC.to_owned())],
                Bytes::from(body.clone()),
            )
            .await?;
        let reply = Client::ok(reply, url)?;
        let hits = reply
            .json()
            .and_then(|v| v.as_array().cloned())
            .ok_or_else(|| Error::Malformed {
                path: url.to_owned(),
                detail: "expected a JSON array of {filename, result}".to_owned(),
            })?;
        let total = hits.len();
        let results: Vec<Value> = hits
            .iter()
            .take(limit)
            .map(|hit| {
                let filename = hit.get("filename").cloned().unwrap_or(Value::Null);
                let result = bound_value(hit.get("result").cloned().unwrap_or(Value::Null), 2000);
                json!({"filename": filename, "result": result})
            })
            .collect();
        let truncated = total > limit;
        let mut text = results
            .iter()
            .map(|r| {
                format!(
                    "{}: {}",
                    r.get("filename").and_then(Value::as_str).unwrap_or("?"),
                    r.get("result").map(Value::to_string).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            text = "no notes matched (results are only returned for truthy values)".to_owned();
        }
        if truncated {
            text.push_str(&format!("\n...[{total} matched; showing {limit}]"));
        }
        let mut hints = Vec::new();
        if !body.contains("\"content\"") {
            hints.push("note content is only available to the query when its text literally mentions \"content\"");
        }
        Ok(shaped(
            json!({
                "limit": limit,
                "total": total,
                "returned": results.len(),
                "truncated": truncated,
                "results": results,
                "hints": hints,
            }),
            text,
        ))
    }

    async fn do_recent_changes(&self, p: RecentChangesParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let days = clamp(p.days, 90, 1, 3650) as u64;
        let limit = clamp(p.limit, 10, 1, 100);
        let now = obsidian::now_ms();
        let cutoff = now.saturating_sub(days.saturating_mul(86_400_000));
        let query = json!({
            "if": [
                {">=": [{"var": "stat.mtime"}, cutoff]},
                {"var": "stat.mtime"},
                false
            ]
        });
        let url = "/search/";
        let reply = client
            .send(
                http::Method::POST,
                url,
                &[("Content-Type", MT_JSONLOGIC.to_owned())],
                Bytes::from(query.to_string()),
            )
            .await?;
        let reply = Client::ok(reply, url)?;
        let hits = reply
            .json()
            .and_then(|v| v.as_array().cloned())
            .ok_or_else(|| Error::Malformed {
                path: url.to_owned(),
                detail: "expected a JSON array of {filename, result}".to_owned(),
            })?;
        let mut notes: Vec<(String, u64)> = hits
            .iter()
            .filter_map(|hit| {
                let filename = hit.get("filename").and_then(Value::as_str)?.to_owned();
                let mtime = hit.get("result").and_then(Value::as_f64)?;
                if !mtime.is_finite() || mtime < 0.0 {
                    return None;
                }
                Some((filename, mtime as u64))
            })
            .collect();
        notes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total = notes.len();
        notes.truncate(limit);
        let rendered: Vec<Value> = notes
            .iter()
            .map(|(path, ms)| {
                json!({"path": path, "mtime_ms": ms, "mtime": obsidian::iso8601_from_ms(*ms)})
            })
            .collect();
        let text = if rendered.is_empty() {
            format!("no notes modified in the last {days} days")
        } else {
            notes
                .iter()
                .map(|(path, ms)| format!("{}  {path}", obsidian::iso8601_from_ms(*ms)))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(shaped(
            json!({
                "days": days,
                "limit": limit,
                "since": obsidian::iso8601_from_ms(cutoff),
                "total": total,
                "returned": rendered.len(),
                "truncated": total > limit,
                "notes": rendered,
            }),
            text,
        ))
    }

    async fn do_list_tags(&self, p: ListTagsParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let limit = clamp(p.limit, 200, 1, 2000);
        let prefix = p
            .prefix
            .map(|s| s.trim().trim_start_matches('#').to_lowercase())
            .filter(|s| !s.is_empty());
        let url = "/tags/";
        let reply = client
            .send(http::Method::GET, url, &[], Bytes::new())
            .await?;
        let reply = Client::ok(reply, url)?;
        let body = reply.json().ok_or_else(|| Error::Malformed {
            path: url.to_owned(),
            detail: "expected {\"tags\": [{name, count}]}".to_owned(),
        })?;
        let all: Vec<(String, u64)> = body
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(|t| {
                        let name = t.get("name").and_then(Value::as_str)?.to_owned();
                        let count = t.get("count").and_then(Value::as_u64).unwrap_or(0);
                        Some((name, count))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let filtered: Vec<&(String, u64)> = all
            .iter()
            .filter(|(name, _)| {
                prefix
                    .as_ref()
                    .map(|p| name.to_lowercase().starts_with(p))
                    .unwrap_or(true)
            })
            .collect();
        let total = filtered.len();
        let kept: Vec<Value> = filtered
            .iter()
            .take(limit)
            .map(|(name, count)| json!({"name": name, "count": count}))
            .collect();
        let text = if kept.is_empty() {
            "no tags".to_owned()
        } else {
            filtered
                .iter()
                .take(limit)
                .map(|(name, count)| format!("#{name} ({count})"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(shaped(
            json!({
                "total": total,
                "returned": kept.len(),
                "truncated": total > limit,
                "prefix": prefix,
                "tags": kept,
            }),
            text,
        ))
    }

    async fn do_active_file(&self, p: ActiveFileParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let format = p.format.unwrap_or(Format::Markdown);
        if format == Format::DocumentMap {
            return Err(Error::invalid(
                "format=document_map is not available for the active file; read it by path \
                 with get_file_contents",
            ));
        }
        let url = "/active/";
        let reply = client
            .send(
                http::Method::GET,
                url,
                &[("Accept", format.accept().to_owned())],
                Bytes::new(),
            )
            .await?;
        let reply =
            match Client::ok(reply, url) {
                Ok(reply) => reply,
                Err(Error::Api { status: 404, .. }) => return Err(Error::invalid(
                    "no note is active in the Obsidian window (HTTP 404 on /active/): open one \
                     in Obsidian or read a note by path with get_file_contents",
                )),
                Err(err) => return Err(err),
            };
        let path = reply
            .header("content-location")
            .map(|loc| obsidian::percent_decode(loc.trim_start_matches("/vault/")))
            .or_else(|| {
                reply
                    .json()
                    .and_then(|v| v.get("path").and_then(Value::as_str).map(str::to_owned))
            })
            .unwrap_or_default();
        let read = NoteRead::from_reply(&reply, format, &path, cfg.max_content_chars)?;
        let text = format!("{path}\n\n{}", read.text_block());
        Ok(shaped(
            json!({
                "path": path,
                "format": format.name(),
                "content": read.content,
                "data": read.data,
                "truncated": read.truncated,
            }),
            text,
        ))
    }

    async fn do_append(&self, p: AppendParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        require_writes(&cfg)?;
        let client = Client::authenticated(&cfg)?;
        let path = obsidian::vault_path(&p.filepath, false)?;
        if p.allow_non_markdown != Some(true) && !path.display.to_lowercase().ends_with(".md") {
            return Err(Error::invalid(format!(
                "'{}' does not end in .md; set allow_non_markdown=true to append to a \
                 non-markdown file",
                path.display
            )));
        }
        if p.content.is_empty() {
            return Err(Error::invalid("content must not be empty"));
        }
        if p.content.len() > MAX_CONTENT_BYTES {
            return Err(Error::invalid(format!(
                "content is {} bytes; the limit is {MAX_CONTENT_BYTES} (1 MiB)",
                p.content.len()
            )));
        }
        let target = match p.heading {
            Some(heading) => {
                obsidian::validate_heading_path(&heading)?;
                Some(UrlTarget::Heading(heading))
            }
            None => None,
        };
        let mut url = format!("/vault/{}", path.encoded);
        if let Some(target) = &target {
            url.push_str(&target.suffix());
        }
        // The plugin reads `Reject-If-Content-Preexists` only in its targeted
        // write path (`_vaultPatchTargeted`); the whole-note append
        // (`_vaultPost`) appends unconditionally. So with a heading the header
        // does the work, and without one the note is read first and the
        // append refused in-guest. That emulation is not atomic: a writer can
        // slip in between the probe and the append.
        let duplicate_check = match (&target, p.reject_if_content_preexists) {
            (Some(_), Some(true)) => Some("plugin"),
            (None, Some(true)) => {
                let note_url = format!("/vault/{}", path.encoded);
                let probe = client
                    .send(
                        http::Method::GET,
                        &note_url,
                        &[("Accept", MT_MARKDOWN.to_owned())],
                        Bytes::new(),
                    )
                    .await?;
                // 404 = the note does not exist yet; the append will create it.
                if probe.status != 404 {
                    let probe = Client::ok(probe, &note_url)?;
                    let existing = probe.text().unwrap_or_default();
                    let trimmed = p.content.trim();
                    let needle = if trimmed.is_empty() {
                        p.content.as_str()
                    } else {
                        trimmed
                    };
                    if existing.contains(needle) {
                        return Err(Error::invalid(format!(
                            "The content is already present in '{}' (checked by reading the \
                             note first: the plugin only honours reject_if_content_preexists \
                             on heading-targeted appends). Treat it as done; do not retry.",
                            path.display
                        )));
                    }
                }
                Some("note-read")
            }
            _ => None,
        };
        // Bare media type: the plugin's guard on targeted writes is an exact
        // match, so a `; charset=` parameter would be a 400 errorCode 40012.
        let mut headers = vec![("Content-Type", MT_MARKDOWN.to_owned())];
        if duplicate_check == Some("plugin") {
            headers.push(("Reject-If-Content-Preexists", "true".to_owned()));
        }
        let bytes = p.content.len();
        let reply = client
            .send(http::Method::POST, &url, &headers, Bytes::from(p.content))
            .await?;
        let reply = Client::ok(reply, &url)?;
        let (updated, truncated) = if reply.status == 200 {
            let (text, cut) =
                obsidian::clamp_marked(&reply.text().unwrap_or_default(), cfg.max_content_chars);
            (Some(text), cut)
        } else {
            (None, false)
        };
        let where_ = target
            .as_ref()
            .map(|t| format!(" under {}", t.describe()))
            .unwrap_or_default();
        Ok(shaped(
            json!({
                "path": path.display,
                "status": "appended",
                "bytes": bytes,
                "target": target.as_ref().map(UrlTarget::describe),
                "duplicate_check": duplicate_check,
                "updated_content": updated,
                "truncated": truncated,
            }),
            format!("appended {bytes} bytes to {}{where_}", path.display),
        ))
    }

    async fn do_put(&self, p: PutParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        require_writes(&cfg)?;
        let client = Client::authenticated(&cfg)?;
        let path = obsidian::vault_path(&p.filepath, false)?;
        if p.content.len() > MAX_CONTENT_BYTES {
            return Err(Error::invalid(format!(
                "content is {} bytes; the limit is {MAX_CONTENT_BYTES} (1 MiB)",
                p.content.len()
            )));
        }
        let url = format!("/vault/{}", path.encoded);
        if p.require_existing == Some(true) {
            let probe = client
                .send(
                    http::Method::GET,
                    &url,
                    &[("Accept", MT_MARKDOWN.to_owned())],
                    Bytes::new(),
                )
                .await?;
            if probe.status == 404 {
                return Err(Error::invalid(format!(
                    "'{}' does not exist and require_existing=true; call again without it to \
                     create the note",
                    path.display
                )));
            }
            Client::ok(probe, &url)?;
        }
        let bytes = p.content.len();
        let reply = client
            .send(
                http::Method::PUT,
                &url,
                &[("Content-Type", MT_MARKDOWN.to_owned())],
                Bytes::from(p.content),
            )
            .await?;
        Client::ok(reply, &url)?;
        Ok(shaped(
            json!({"path": path.display, "status": "written", "bytes": bytes}),
            format!("wrote {bytes} bytes to {}", path.display),
        ))
    }

    async fn do_patch(&self, p: PatchParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        require_writes(&cfg)?;
        let client = Client::authenticated(&cfg)?;
        let path = obsidian::vault_path(&p.filepath, false)?;
        let (target_json, target_display, heading_path) = match p.target_type {
            TargetType::Heading => {
                let elements: Vec<String> = match &p.target {
                    TargetSpec::Path(items) => items.clone(),
                    TargetSpec::Text(text) => {
                        text.split("::").map(|s| s.trim().to_owned()).collect()
                    }
                };
                obsidian::validate_heading_path(&elements)?;
                (json!(elements), elements.join(" > "), Some(elements))
            }
            TargetType::Block => {
                let id = single_target(&p.target, "block")?;
                let id = id.trim().trim_start_matches('^').to_owned();
                obsidian::validate_target_text("block id", &id)?;
                (json!(id), id, None)
            }
            TargetType::Frontmatter => {
                let key = single_target(&p.target, "frontmatter")?;
                let key = key.trim().to_owned();
                obsidian::validate_target_text("frontmatter key", &key)?;
                (json!(key), key, None)
            }
        };
        let is_delete = p.operation == Operation::Delete;
        match (&p.content, &p.value) {
            (Some(_), Some(_)) => {
                return Err(Error::invalid(
                    "supply exactly one of content (markdown text) or value (JSON), not both",
                ))
            }
            (None, None) if !is_delete => {
                return Err(Error::invalid(format!(
                    "operation {} needs content (markdown) or value (JSON for frontmatter \
                     fields / table rows)",
                    p.operation.name()
                )))
            }
            _ => {}
        }
        if let Some(content) = &p.content {
            if content.len() > MAX_CONTENT_BYTES {
                return Err(Error::invalid(format!(
                    "content is {} bytes; the limit is {MAX_CONTENT_BYTES} (1 MiB)",
                    content.len()
                )));
            }
        }
        if let Some(value) = &p.value {
            if value.to_string().len() > MAX_CONTENT_BYTES {
                return Err(Error::invalid("value serializes to more than 1 MiB"));
            }
        }
        if let Some(token) = &p.if_match {
            if token.is_empty()
                || token.chars().count() > MAX_IF_MATCH_CHARS
                || !token.chars().all(|c| c.is_ascii_graphic() || c == ' ')
            {
                return Err(Error::invalid(
                    "if_match must be the printable version token from the document map",
                ));
            }
        }
        let payload = PatchPayload {
            target_type: p.target_type,
            operation: p.operation,
            target_json,
            target_display: target_display.clone(),
            heading_path,
            content: p.content,
            value: p.value,
            scope: p.scope,
            within: p.within,
            create: p.create_target_if_missing == Some(true),
            if_match: p.if_match,
        };
        let url = format!("/vault/{}", path.encoded);
        let mut notes: Vec<String> = Vec::new();
        // A version fetched during this very call cannot be stale; only a
        // cached one is worth re-checking after a format-shaped 400.
        let was_cached = obsidian::cached_plugin_major().is_some();
        let major = client.plugin_major().await?;
        let (headers, body, mut patch_format) = build_patch(major, &payload, &mut notes)?;
        let mut reply = client
            .send(http::Method::PATCH, &url, &headers, body)
            .await?;
        if was_cached
            && reply.status == 400
            && matches!(
                reply.error_code(),
                Some(40012 | 40053 | 40081 | 40083 | 40084)
            )
        {
            // The cached plugin version may be stale (the plugin was upgraded
            // or downgraded while this instance was warm): refresh it with one
            // GET / and re-send once if that changes the wire format. If the
            // refresh itself fails, the original diagnosis (below) stands.
            // 40081/40083/40084 are what a 5.x plugin answers to legacy
            // headers; 40053 (no Target-Type header) and 40012 (unknown
            // content type) are what a header-only < 5 plugin answers to a
            // JSON instruction.
            let fresh = match client.server_info().await {
                Ok(_) => obsidian::cached_plugin_major().unwrap_or(major),
                Err(_) => major,
            };
            if (fresh >= 5) != (major >= 5) {
                notes.push(format!(
                    "the cached plugin version was stale (plugin is now major {fresh}); the \
                     patch was re-sent in the {} format",
                    patch_format_name(Some(fresh))
                ));
                let (headers, body, format) = build_patch(fresh, &payload, &mut notes)?;
                patch_format = format;
                reply = client
                    .send(http::Method::PATCH, &url, &headers, body)
                    .await?;
            }
        }
        let reply = Client::ok(reply, &url)?;
        let warnings = reply
            .header("markdown-patch-warnings")
            .map(|raw| {
                let decoded = obsidian::percent_decode(&raw);
                serde_json::from_str::<Value>(&decoded).unwrap_or(Value::String(decoded))
            })
            .unwrap_or(Value::Array(Vec::new()));
        if let Some(deprecation) = reply.header("deprecation") {
            notes.push(format!(
                "the plugin ran this on its deprecated patch engine ({deprecation}); upgrade \
                 the plugin"
            ));
        }
        let (updated, truncated) =
            obsidian::clamp_marked(&reply.text().unwrap_or_default(), cfg.max_content_chars);
        let text = format!(
            "patched {} ({} {} '{}', {patch_format}){}\n\n{updated}",
            path.display,
            p.operation.name(),
            p.target_type.name(),
            target_display,
            if warnings.as_array().map(|w| !w.is_empty()).unwrap_or(true) {
                format!("\nwarnings: {warnings}")
            } else {
                String::new()
            }
        );
        Ok(shaped(
            json!({
                "path": path.display,
                "operation": p.operation.name(),
                "target_type": p.target_type.name(),
                "target": target_display,
                "patch_format": patch_format,
                "status": reply.status,
                "updated_content": updated,
                "truncated": truncated,
                "warnings": warnings,
                "notes": notes,
            }),
            text,
        ))
    }

    async fn do_delete(&self, p: DeleteParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        require_writes(&cfg)?;
        let client = Client::authenticated(&cfg)?;
        let path = obsidian::vault_path(&p.filepath, false)?;
        if p.confirm != Some(true) {
            return Err(Error::Gated(format!(
                "refusing to delete '{}' without confirm=true (nothing was sent). Deleting \
                 moves the note to Obsidian's trash unless permanent=true.",
                path.display
            )));
        }
        let permanent = p.permanent == Some(true);
        let url = format!("/vault/{}?permanent={permanent}", path.encoded);
        let reply = client
            .send(http::Method::DELETE, &url, &[], Bytes::new())
            .await?;
        Client::ok(reply, &url)?;
        Ok(shaped(
            json!({"path": path.display, "deleted": true, "permanent": permanent}),
            format!(
                "deleted {} ({})",
                path.display,
                if permanent {
                    "permanently"
                } else {
                    "to Obsidian's trash"
                }
            ),
        ))
    }

    async fn do_periodic(&self, p: PeriodicParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let format = p.format.unwrap_or(Format::Markdown);
        if format == Format::DocumentMap {
            return Err(Error::invalid(
                "format=document_map is not available for periodic notes; read the resolved \
                 path with get_file_contents",
            ));
        }
        let date = match &p.date {
            Some(text) => Some(obsidian::parse_ymd(text.trim()).ok_or_else(|| {
                Error::invalid(format!("date must be YYYY-MM-DD (got {text:?})"))
            })?),
            None => None,
        };
        let (resolved, reply) = fetch_periodic(&client, p.period, date, format.accept()).await?;
        let read = NoteRead::from_reply(&reply, format, &resolved, cfg.max_content_chars)?;
        let date_text = date.map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}"));
        let text = format!("{resolved}\n\n{}", read.text_block());
        Ok(shaped(
            json!({
                "period": p.period.name(),
                "date": date_text,
                "path": resolved,
                "format": format.name(),
                "content": read.content,
                "data": read.data,
                "truncated": read.truncated,
            }),
            text,
        ))
    }

    async fn do_recent_periodic(&self, p: RecentPeriodicParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        let client = Client::authenticated(&cfg)?;
        let limit = clamp(p.limit, 5, 1, 10);
        let include_content = p.include_content == Some(true);
        let offset_minutes = p.tz_offset_minutes.unwrap_or(0);
        if !(-840..=840).contains(&offset_minutes) {
            return Err(Error::invalid(format!(
                "tz_offset_minutes must be between -840 and 840 (UTC-14:00 .. UTC+14:00), got \
                 {offset_minutes}"
            )));
        }
        // "Today" in the user's zone (UTC unless tz_offset_minutes says
        // otherwise); the current period itself (i == 0) uses the undated
        // route so the plugin decides it in Obsidian's local time, exactly as
        // get_periodic_note does.
        let now_ms = i64::try_from(obsidian::now_ms()).unwrap_or(i64::MAX);
        let today_days = now_ms
            .saturating_add(offset_minutes.saturating_mul(60_000))
            .div_euclid(86_400_000);
        let (ty, tm, td) = obsidian::civil_from_days(today_days);
        let budget = Budget::start();
        let mut found: Vec<Value> = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        let mut duplicates = 0usize;
        let mut attempted = 0usize;
        let mut budget_exhausted = false;
        let mut lines = Vec::new();
        for i in 0..limit as i64 {
            if budget.spent() {
                budget_exhausted = true;
                break;
            }
            attempted += 1;
            let (y, m, d) = match p.period {
                Period::Daily => obsidian::civil_from_days(today_days - i),
                Period::Weekly => obsidian::civil_from_days(today_days - 7 * i),
                Period::Monthly => {
                    if i == 0 {
                        (ty, tm, td)
                    } else {
                        let (y, m) = obsidian::shift_months(ty, tm, -i);
                        (y, m, 1)
                    }
                }
                Period::Quarterly => {
                    if i == 0 {
                        (ty, tm, td)
                    } else {
                        let (y, m) = obsidian::shift_months(ty, tm, -3 * i);
                        (y, m, 1)
                    }
                }
                Period::Yearly => {
                    if i == 0 {
                        (ty, tm, td)
                    } else {
                        (ty - i, 1, 1)
                    }
                }
            };
            let date = format!("{y:04}-{m:02}-{d:02}");
            let requested = if i == 0 { None } else { Some((y, m, d)) };
            match fetch_periodic(&client, p.period, requested, MT_MARKDOWN).await {
                Ok((resolved, reply)) => {
                    if paths.contains(&resolved) {
                        // The plugin's "today" and the computed one overlap
                        // (tz_offset_minutes is off by a day): count it once.
                        duplicates += 1;
                        continue;
                    }
                    let content = if include_content {
                        let read =
                            NoteRead::from_reply(&reply, Format::Markdown, &resolved, 10_000)?;
                        read.content
                    } else {
                        None
                    };
                    lines.push(format!("{date}  {resolved}"));
                    paths.push(resolved.clone());
                    found.push(json!({
                        "date": date,
                        "resolved_by": if i == 0 { "plugin" } else { "date" },
                        "path": resolved,
                        "content": content,
                    }));
                }
                // No note for that period (the companion's first-hop
                // errorCode 40461, or a 404 after its redirect): skip it.
                // Anything else — companion plugin missing, period unknown
                // or disabled, 5xx — would repeat for every look-back, so
                // abort with that error.
                Err(err) if is_missing_periodic_note(&err) => {
                    skipped += 1;
                }
                Err(err) => return Err(err),
            }
        }
        let mut text = if lines.is_empty() {
            format!(
                "no {} notes found in the last {limit} periods",
                p.period.name()
            )
        } else {
            lines.join("\n")
        };
        if budget_exhausted {
            text.push_str(&format!(
                "\n(stopped after {attempted} of {limit} lookups: the call's {} ms time budget \
                 was spent; call again with a smaller limit)",
                budget.limit_ms()
            ));
        }
        if duplicates > 0 {
            text.push_str(&format!(
                "\n({duplicates} lookup(s) resolved to a note already listed: pass \
                 tz_offset_minutes so the dated look-backs start from Obsidian's local day)"
            ));
        }
        Ok(shaped(
            json!({
                "period": p.period.name(),
                "lookups": limit,
                "attempted": attempted,
                "found": found.len(),
                "skipped": skipped,
                "duplicates": duplicates,
                "budget_exhausted": budget_exhausted,
                "budget_ms": budget.limit_ms(),
                "tz_offset_minutes": offset_minutes,
                "notes": found,
            }),
            text,
        ))
    }

    async fn do_open(&self, p: OpenFileParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        require_writes(&cfg)?;
        let client = Client::authenticated(&cfg)?;
        let path = obsidian::vault_path(&p.filepath, false)?;
        let new_leaf = p.new_leaf == Some(true);
        let url = format!("/open/{}?newLeaf={new_leaf}", path.encoded);
        let reply = client
            .send(http::Method::POST, &url, &[], Bytes::new())
            .await?;
        Client::ok(reply, &url)?;
        Ok(shaped(
            json!({"path": path.display, "opened": true, "new_leaf": new_leaf}),
            format!("opened {} in Obsidian", path.display),
        ))
    }

    async fn do_list_commands(&self, p: ListCommandsParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        require_commands(&cfg)?;
        let client = Client::authenticated(&cfg)?;
        let limit = clamp(p.limit, 100, 1, 1000);
        let filter = p
            .filter
            .map(|f| f.trim().to_lowercase())
            .filter(|f| !f.is_empty());
        let url = "/commands/";
        let reply = client
            .send(http::Method::GET, url, &[], Bytes::new())
            .await?;
        let reply = Client::ok(reply, url)?;
        let body = reply.json().ok_or_else(|| Error::Malformed {
            path: url.to_owned(),
            detail: "expected {\"commands\": [{id, name}]}".to_owned(),
        })?;
        let all: Vec<(String, String)> = body
            .get("commands")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|c| {
                        let id = c.get("id").and_then(Value::as_str)?.to_owned();
                        let name = c
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        Some((id, name))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let filtered: Vec<&(String, String)> = all
            .iter()
            .filter(|(id, name)| {
                filter
                    .as_ref()
                    .map(|f| id.to_lowercase().contains(f) || name.to_lowercase().contains(f))
                    .unwrap_or(true)
            })
            .collect();
        let total = filtered.len();
        let kept: Vec<Value> = filtered
            .iter()
            .take(limit)
            .map(|(id, name)| json!({"id": id, "name": name}))
            .collect();
        let text = if kept.is_empty() {
            "no commands".to_owned()
        } else {
            filtered
                .iter()
                .take(limit)
                .map(|(id, name)| format!("{id}  {name}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(shaped(
            json!({
                "total": total,
                "returned": kept.len(),
                "truncated": total > limit,
                "filter": filter,
                "commands": kept,
            }),
            text,
        ))
    }

    async fn do_execute_command(&self, p: ExecuteCommandParams) -> Result<CallToolResult, Error> {
        let cfg = obsidian::config();
        require_commands(&cfg)?;
        require_writes(&cfg)?;
        let client = Client::authenticated(&cfg)?;
        let id = p.command_id.trim().to_owned();
        if id.is_empty() {
            return Err(Error::invalid("command_id must not be empty"));
        }
        if id.chars().count() > MAX_COMMAND_ID_CHARS {
            return Err(Error::invalid(format!(
                "command_id is longer than {MAX_COMMAND_ID_CHARS} characters"
            )));
        }
        let url = format!("/commands/{}/", obsidian::encode_segment(&id));
        let reply = client
            .send(http::Method::POST, &url, &[], Bytes::new())
            .await?;
        match Client::ok(reply, &url) {
            Ok(_) => Ok(shaped(
                json!({"command_id": id, "executed": true}),
                format!("executed {id}"),
            )),
            Err(Error::Api {
                status: 404,
                message,
                ..
            }) => Err(Error::invalid(format!(
                "no command with id '{id}' (HTTP 404: {message}); pick an id from list_commands"
            ))),
            Err(err) => Err(err),
        }
    }
}

/// Everything a PATCH needs, kept apart from the wire format so the request
/// can be rebuilt when the plugin version turns out to differ.
struct PatchPayload {
    target_type: TargetType,
    operation: Operation,
    target_json: Value,
    target_display: String,
    heading_path: Option<Vec<String>>,
    content: Option<String>,
    value: Option<Value>,
    scope: Option<Scope>,
    within: Option<i64>,
    create: bool,
    if_match: Option<String>,
}

/// Headers, body and format label of one PATCH request.
type PatchWire = (Vec<(&'static str, String)>, Bytes, &'static str);

/// Builds the headers, body and format label for one plugin major version:
/// the JSON instruction for >= 5, the deprecated header form below.
fn build_patch(major: u32, p: &PatchPayload, notes: &mut Vec<String>) -> Result<PatchWire, Error> {
    if major >= 5 {
        let mut instruction = json!({
            "targetType": p.target_type.name(),
            "target": p.target_json,
            "operation": p.operation.name(),
        });
        if let Some(scope) = p.scope {
            instruction["scope"] = json!(scope.name());
        }
        if let Some(content) = &p.content {
            instruction["content"] = Value::String(content.clone());
        }
        if let Some(value) = &p.value {
            instruction["value"] = value.clone();
        }
        if let Some(within) = p.within {
            instruction["within"] = json!(within);
        }
        if p.create {
            instruction["createTargetIfMissing"] = json!(true);
        }
        if let Some(token) = &p.if_match {
            instruction["ifMatch"] = Value::String(token.clone());
        }
        return Ok((
            vec![("Content-Type", MT_PATCH.to_owned())],
            Bytes::from(instruction.to_string()),
            "json-v2",
        ));
    }
    if p.operation == Operation::Delete {
        return Err(Error::invalid(
            "operation=delete needs plugin >= 5.0 (this vault reports an older plugin); use \
             replace with new content, or upgrade the plugin",
        ));
    }
    let target_header = match &p.heading_path {
        Some(elements) => obsidian::encode_segment(&elements.join("::")),
        None => obsidian::encode_segment(&p.target_display),
    };
    let mut headers = vec![
        ("Operation", p.operation.name().to_owned()),
        ("Target-Type", p.target_type.name().to_owned()),
        ("Target", target_header),
        (
            "Create-Target-If-Missing",
            if p.create { "true" } else { "false" }.to_owned(),
        ),
    ];
    let body = if let Some(content) = &p.content {
        headers.push(("Content-Type", MT_MARKDOWN.to_owned()));
        Bytes::from(content.clone())
    } else if let Some(value) = &p.value {
        headers.push(("Content-Type", "application/json".to_owned()));
        Bytes::from(value.to_string())
    } else {
        Bytes::new()
    };
    if p.scope.is_some() || p.within.is_some() || p.if_match.is_some() {
        let note = "scope/within/if_match are ignored by the legacy (plugin < 5) header format";
        if !notes.iter().any(|n| n == note) {
            notes.push(note.to_owned());
        }
    }
    Ok((headers, body, "legacy-v1"))
}

fn patch_format_name(major: Option<u32>) -> &'static str {
    match major {
        Some(m) if m < 5 => "legacy-v1",
        _ => "json-v2",
    }
}

fn display_dir(path: &VaultPath) -> String {
    if path.display.is_empty() {
        "/".to_owned()
    } else {
        format!("{}/", path.display)
    }
}

fn url_target(
    heading: Option<Vec<String>>,
    block: Option<String>,
    frontmatter_key: Option<String>,
) -> Result<Option<UrlTarget>, Error> {
    let given = heading.is_some() as u8 + block.is_some() as u8 + frontmatter_key.is_some() as u8;
    if given > 1 {
        return Err(Error::invalid(
            "give at most one of heading, block, or frontmatter_key",
        ));
    }
    if let Some(heading) = heading {
        obsidian::validate_heading_path(&heading)?;
        return Ok(Some(UrlTarget::Heading(heading)));
    }
    if let Some(block) = block {
        let id = block.trim().trim_start_matches('^').to_owned();
        obsidian::validate_target_text("block id", &id)?;
        return Ok(Some(UrlTarget::Block(id)));
    }
    if let Some(key) = frontmatter_key {
        let key = key.trim().to_owned();
        obsidian::validate_target_text("frontmatter key", &key)?;
        return Ok(Some(UrlTarget::Frontmatter(key)));
    }
    Ok(None)
}

/// Bounds a JsonLogic result value for output: long strings are cut, and
/// large non-string values are rendered as a cut string.
fn bound_value(value: Value, max_chars: usize) -> Value {
    match value {
        Value::String(s) => {
            let (kept, _) = obsidian::clamp_marked(&s, max_chars);
            Value::String(kept)
        }
        other => {
            let rendered = other.to_string();
            if rendered.chars().count() > max_chars {
                let (kept, _) = obsidian::clamp_marked(&rendered, max_chars);
                Value::String(kept)
            } else {
                other
            }
        }
    }
}

/// Resolves a periodic note through the companion plugin's 307 redirect
/// (followed exactly once, same origin, same Accept). Returns the resolved
/// vault path and the note reply. A first-hop answer that is neither a
/// redirect nor a 2xx goes through [`classify_periodic_error`]; a 404 on the
/// second hop means the note vanished between the hops.
async fn fetch_periodic(
    client: &Client,
    period: Period,
    date: Option<(i64, u32, u32)>,
    accept: &str,
) -> Result<(String, Reply), Error> {
    let first = match date {
        Some((y, m, d)) => format!("/periodic/{}/{y}/{m}/{d}/", period.name()),
        None => format!("/periodic/{}/", period.name()),
    };
    let headers = [("Accept", accept.to_owned())];
    let reply = client
        .send(http::Method::GET, &first, &headers, Bytes::new())
        .await?;
    if reply.is_redirect() {
        let location = reply.header("location").ok_or_else(|| Error::Malformed {
            path: first.clone(),
            detail: format!("HTTP {} without a Location header", reply.status),
        })?;
        let target = resolve_location(client.base_url(), &location)?;
        let resolved = obsidian::percent_decode(
            target
                .split('?')
                .next()
                .unwrap_or("")
                .trim_start_matches("/vault/"),
        );
        let second = client
            .send(http::Method::GET, &target, &headers, Bytes::new())
            .await?;
        let second = match Client::ok(second, &target) {
            Ok(reply) => reply,
            Err(Error::Api {
                status: 404,
                error_code,
                message,
                retry_after,
                ..
            }) => {
                return Err(Error::Api {
                    path: target,
                    status: 404,
                    error_code,
                    message: format!(
                        "the {} note for {} would be '{resolved}' but it does not exist \
                         ({message}); create it with put_content or append_content at that \
                         path",
                        period.name(),
                        describe_date(date)
                    ),
                    retry_after,
                })
            }
            Err(err) => return Err(err),
        };
        return Ok((resolved, second));
    }
    if reply.is_success() {
        // Plugin < 5 served /periodic/ natively and answered inline.
        let resolved = reply
            .header("content-location")
            .map(|loc| obsidian::percent_decode(loc.trim_start_matches("/vault/")))
            .unwrap_or_default();
        return Ok((resolved, reply));
    }
    Err(classify_periodic_error(reply, &first, period, date))
}

/// One error for a first-hop `/periodic/` answer that was neither a redirect
/// nor a 2xx. Three different conditions come back as 404, so the plugin's
/// `errorCode` decides — never the status alone:
///
/// | answer                            | meaning                                          |
/// |-----------------------------------|--------------------------------------------------|
/// | 404 errorCode 40461               | the period is set up but has no note for that date |
/// | 404 errorCode 40460               | Obsidian has no such period configured           |
/// | 404 errorCode 40400 / no envelope | the route is not served: the companion plugin is missing (the core plugin's generic 404) |
/// | 400 errorCode 40060               | the period exists but is not enabled             |
/// | anything else                     | reported exactly as upstream said it             |
///
/// The remediation for each code lives with the other hints in
/// `obsidian::api_message`, so the text here names the condition and quotes
/// the upstream message; the `errorCode` travels with the error so callers
/// (see [`is_missing_periodic_note`]) can branch on it.
fn classify_periodic_error(
    reply: Reply,
    path: &str,
    period: Period,
    date: Option<(i64, u32, u32)>,
) -> Error {
    let (status, error_code, upstream, retry_after) = match Client::ok(reply, path) {
        Err(Error::Api {
            status,
            error_code,
            message,
            retry_after,
            ..
        }) => (status, error_code, message, retry_after),
        Err(other) => return other,
        // Callers only get here for a non-2xx reply.
        Ok(reply) => {
            return Error::Malformed {
                path: path.to_owned(),
                detail: format!("HTTP {} reached the periodic error classifier", reply.status),
            }
        }
    };
    let name = period.name();
    let message = match (status, error_code) {
        (404, Some(PERIODIC_CODE_NO_NOTE)) => format!(
            "the {name} period is set up but has no note for {} yet. Upstream said: {upstream}",
            describe_date(date)
        ),
        (404, Some(PERIODIC_CODE_UNKNOWN_PERIOD)) => {
            format!("Obsidian has no {name} period configured. Upstream said: {upstream}")
        }
        (404, Some(40400) | None) => format!(
            "the /periodic/ route is not served: this is a generic 404 without the companion \
             plugin's errorCode (40460/40461). Upstream said: {upstream}"
        ),
        (400, Some(PERIODIC_CODE_NOT_ENABLED)) => format!(
            "the {name} period is not enabled in Obsidian's Daily Notes / Periodic Notes \
             plugin. Upstream said: {upstream}"
        ),
        _ => upstream,
    };
    Error::Api {
        path: path.to_owned(),
        status,
        error_code,
        message,
        retry_after,
    }
}

/// `2026-09-02` for a dated lookup, `the current period` for the undated route.
fn describe_date(date: Option<(i64, u32, u32)>) -> String {
    match date {
        Some((y, m, d)) => format!("{y:04}-{m:02}-{d:02}"),
        None => "the current period".to_owned(),
    }
}

/// Whether a periodic lookup failed only because that period has no note:
/// the companion plugin's 404 errorCode 40461 on the first hop, or a 404 on
/// the `/vault/` path it redirected to (the note vanished between the hops).
/// Every other failure (companion missing, period unknown or disabled, 5xx)
/// would repeat for every look-back, so `get_recent_periodic_notes` aborts.
fn is_missing_periodic_note(err: &Error) -> bool {
    match err {
        Error::Api {
            status: 404,
            error_code: Some(PERIODIC_CODE_NO_NOTE),
            ..
        } => true,
        Error::Api {
            status: 404, path, ..
        } => path.starts_with("/vault/"),
        _ => false,
    }
}

// --- protocol surface -------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ObsidianServer {
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
            "Obsidian vault access through the 'Local REST API with MCP' community plugin \
             running inside the user's Obsidian, as a sandboxed WebAssembly component on \
             Cosmonic Desktop. Call `check_auth` first (it reports whether the API key and \
             the plugin's plain-HTTP listener work; never retry a missing/invalid result). \
             Read with list_files_in_vault / list_files_in_dir / get_file_contents / \
             batch_get_file_contents / simple_search / complex_search / get_recent_changes / \
             list_tags / get_active_file / get_periodic_note / get_recent_periodic_notes; \
             write with append_content / put_content / patch_content / delete_file / \
             open_file (all refused when OBSIDIAN_READ_ONLY=true; delete needs \
             confirm=true); list_commands / execute_command only when \
             OBSIDIAN_ENABLE_COMMANDS=true.\n\n\
             This server publishes skills — playbooks describing when and how to use its \
             tools, the PATCH rules, and the error catalogue. Read `skill://index.json` for \
             the catalog, then `skill://obsidian-mcp/SKILL.md`.",
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
