//! The MCP server implementation: the thirteen tools of the official
//! `@modelcontextprotocol/server-filesystem` reference server, over a mounted
//! host folder. File-system logic lives in [`crate::fsops`]; this module is
//! the tool surface (argument schemas, gating, result rendering).
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.
//!
//! Tool bodies are written as `Result<CallToolResult, CallToolResult>`: the
//! `Err` arm is the already-rendered tool error, so `?` short-circuits with
//! the exact message the caller should see. That makes the `Err` variant as
//! large as a result — deliberate, and not a hot path.
#![allow(clippy::result_large_err)]

use std::fs;
use std::io;

use base64::Engine as _;
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

use crate::fsops::{self, Config, Edit, FsError, Missing, Validated, Walk};
use crate::skills;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so do not keep per-session
/// state on this struct. The only state is the mounted folder itself.
#[derive(Clone)]
pub struct TemplateServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadTextFileParams {
    /// Absolute guest path of the file (e.g. `/data/notes.md`); a relative
    /// path resolves against the first allowed directory.
    pub path: String,
    /// If provided, returns only the first N lines of the file.
    #[serde(default)]
    pub head: Option<u64>,
    /// If provided, returns only the last N lines of the file.
    #[serde(default)]
    pub tail: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PathParams {
    /// Absolute guest path (e.g. `/data/report.pdf`); a relative path
    /// resolves against the first allowed directory.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMultipleFilesParams {
    /// Array of file paths to read. Each path must be a string pointing to a
    /// valid file within allowed directories (at most 100 per call).
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileParams {
    /// Absolute guest path of the file to create or overwrite. Its parent
    /// directory must already exist (use `create_directory` first).
    pub path: String,
    /// The complete new file content (UTF-8 text).
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditOperation {
    /// Text to search for - must match exactly (a whitespace-trimmed
    /// line-by-line match is tried as a fallback).
    #[serde(rename = "oldText")]
    pub old_text: String,
    /// Text to replace with.
    #[serde(rename = "newText")]
    pub new_text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileParams {
    /// Absolute guest path of the text file to edit.
    pub path: String,
    /// Edits applied in order; each `oldText` must match the file as
    /// modified by the previous edits (at most 200 per call).
    pub edits: Vec<EditOperation>,
    /// Preview changes using git-style diff format without writing.
    #[serde(rename = "dryRun", default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortBy {
    Name,
    Size,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDirectoryWithSizesParams {
    /// Directory path to list.
    pub path: String,
    /// Sort entries by name or size (size sorts descending). Default: name.
    #[serde(rename = "sortBy", default)]
    pub sort_by: Option<SortBy>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DirectoryTreeParams {
    /// Starting directory.
    pub path: String,
    /// Glob patterns (relative to `path`) to leave out, e.g. `node_modules`,
    /// `**/.git/**`, `*.log`.
    #[serde(rename = "excludePatterns", default)]
    pub exclude_patterns: Vec<String>,
    /// Maximum recursion depth (1 = immediate children only). Defaults to
    /// the server's FS_MAX_TREE_DEPTH and cannot exceed it.
    #[serde(rename = "maxDepth", default)]
    pub max_depth: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveFileParams {
    /// Existing file or directory to move.
    pub source: String,
    /// New path. Must not exist; its parent directory must.
    pub destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchFilesParams {
    /// Starting directory.
    pub path: String,
    /// Glob matched against paths relative to `path`: `*.md` matches the top
    /// level only, `**/*.md` recurses, a bare name like `README.md` is found
    /// at any depth.
    pub pattern: String,
    /// Glob patterns to exclude, e.g. `node_modules`, `**/.git/**`.
    #[serde(rename = "excludePatterns", default)]
    pub exclude_patterns: Vec<String>,
    /// Maximum recursion depth. Defaults to the server's FS_MAX_TREE_DEPTH
    /// and cannot exceed it.
    #[serde(rename = "maxDepth", default)]
    pub max_depth: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoParams {}

/// A plain-text result.
fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

/// A result with a readable text block plus machine-readable
/// `structuredContent`.
fn structured_result(text: impl Into<String>, structured: Value) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text.into())]);
    result.structured_content = Some(structured);
    result
}

/// A tool-level error: the tool ran and failed, and the caller sees why.
fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

impl From<FsError> for CallToolResult {
    fn from(err: FsError) -> Self {
        tool_error(err.0)
    }
}

/// Loads the configuration or renders the actionable missing-config error.
fn config() -> Result<Config, CallToolResult> {
    Config::from_env().map_err(CallToolResult::from)
}

/// Refuses mutating tools when `FS_READ_ONLY` is set.
fn ensure_writable(cfg: &Config, tool: &str) -> Result<(), CallToolResult> {
    if cfg.read_only {
        Err(tool_error(format!(
            "This server is read-only (FS_READ_ONLY=true); {tool} is disabled."
        )))
    } else {
        Ok(())
    }
}

fn validate(cfg: &Config, path: &str, missing: Missing) -> Result<Validated, CallToolResult> {
    fsops::validate_path(cfg, path, missing).map_err(CallToolResult::from)
}

fn io_error(op: &str, path: &str, err: &io::Error) -> CallToolResult {
    tool_error(fsops::io_message(op, path, err))
}

/// Clamps a `head`/`tail` count; zero is rejected (the reference server
/// treats it as absent, which surprises callers).
fn line_count(name: &str, value: u64) -> Result<u64, CallToolResult> {
    if value == 0 {
        return Err(tool_error(format!("{name} must be a positive integer")));
    }
    Ok(value.min(fsops::MAX_HEAD_TAIL_LINES))
}

#[tool_router]
impl TemplateServer {
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
        name = "read_text_file",
        description = "Read the complete contents of a file from the file system as text. \
            Handles various text encodings and provides detailed error messages if the file \
            cannot be read. Use this tool when you need to examine the contents of a single \
            file. Use the 'head' parameter to read only the first N lines of a file, or the \
            'tail' parameter to read only the last N lines of a file. Operates on the file as \
            text regardless of extension. Files larger than FS_MAX_FILE_BYTES come back \
            truncated with a note. Only works within allowed directories.",
        annotations(
            title = "Read Text File",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.read_text_file", skip(self))]
    async fn read_text_file(
        &self,
        Parameters(params): Parameters<ReadTextFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.read_text_file_impl(params))
    }

    #[tool(
        name = "read_media_file",
        description = "Read a file and return it as a base64-encoded content block with its MIME \
            type. Image and audio files are returned as image/audio content; any other file \
            type is returned as an embedded resource. Files larger than FS_MAX_FILE_BYTES are \
            refused. Only works within allowed directories.",
        annotations(
            title = "Read Media File",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.read_media_file", skip(self))]
    async fn read_media_file(
        &self,
        Parameters(params): Parameters<PathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.read_media_file_impl(params))
    }

    #[tool(
        name = "read_multiple_files",
        description = "Read the contents of multiple files simultaneously. This is more efficient \
            than reading files one by one when you need to analyze or compare multiple files. \
            Each file's content is returned with its path as a reference. Failed reads for \
            individual files won't stop the entire operation. Only works within allowed \
            directories.",
        annotations(
            title = "Read Multiple Files",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.read_multiple_files", skip(self))]
    async fn read_multiple_files(
        &self,
        Parameters(params): Parameters<ReadMultipleFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.read_multiple_files_impl(params))
    }

    #[tool(
        name = "write_file",
        description = "Create a new file or completely overwrite an existing file with new \
            content. Use with caution as it will overwrite existing files without warning. \
            Handles text content with proper encoding; the write is atomic (no partial files). \
            The parent directory must exist. Only works within allowed directories.",
        annotations(
            title = "Write File",
            read_only_hint = false,
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.write_file", skip(self, params), fields(path = %params.path))]
    async fn write_file(
        &self,
        Parameters(params): Parameters<WriteFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.write_file_impl(params))
    }

    #[tool(
        name = "edit_file",
        description = "Make line-based edits to a text file. Each edit replaces exact line \
            sequences with new content. Returns a git-style diff showing the changes made. \
            Use dryRun: true to preview the diff without writing. Only works within allowed \
            directories.",
        annotations(
            title = "Edit File",
            read_only_hint = false,
            idempotent_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.edit_file", skip(self, params), fields(path = %params.path, dry_run = params.dry_run))]
    async fn edit_file(
        &self,
        Parameters(params): Parameters<EditFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.edit_file_impl(params))
    }

    #[tool(
        name = "create_directory",
        description = "Create a new directory or ensure a directory exists. Can create multiple \
            nested directories in one operation. If the directory already exists, this \
            operation will succeed silently. Perfect for setting up directory structures for \
            projects or ensuring required paths exist. Only works within allowed directories.",
        annotations(
            title = "Create Directory",
            read_only_hint = false,
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.create_directory", skip(self))]
    async fn create_directory(
        &self,
        Parameters(params): Parameters<PathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.create_directory_impl(params))
    }

    #[tool(
        name = "list_directory",
        description = "Get a detailed listing of all files and directories in a specified path. \
            Results clearly distinguish between files and directories with [FILE] and [DIR] \
            prefixes. This tool is essential for understanding directory structure and \
            finding specific files within a directory. Only works within allowed directories.",
        annotations(
            title = "List Directory",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.list_directory", skip(self))]
    async fn list_directory(
        &self,
        Parameters(params): Parameters<PathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.list_directory_impl(params))
    }

    #[tool(
        name = "list_directory_with_sizes",
        description = "Get a detailed listing of all files and directories in a specified path, \
            including sizes. Results clearly distinguish between files and directories with \
            [FILE] and [DIR] prefixes. This tool is useful for understanding directory \
            structure and finding specific files within a directory. Only works within \
            allowed directories.",
        annotations(
            title = "List Directory with Sizes",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.list_directory_with_sizes", skip(self))]
    async fn list_directory_with_sizes(
        &self,
        Parameters(params): Parameters<ListDirectoryWithSizesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.list_directory_with_sizes_impl(params))
    }

    #[tool(
        name = "directory_tree",
        description = "Get a recursive tree view of files and directories as a JSON structure. \
            Each entry includes 'name', 'type' (file/directory), and 'children' for \
            directories. Files have no children array, while directories always have a \
            children array (which may be empty). The output is formatted with 2-space \
            indentation for readability. Only works within allowed directories.",
        annotations(
            title = "Directory Tree",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.directory_tree", skip(self))]
    async fn directory_tree(
        &self,
        Parameters(params): Parameters<DirectoryTreeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.directory_tree_impl(params))
    }

    #[tool(
        name = "move_file",
        description = "Move or rename files and directories. Can move files between directories \
            and rename them in a single operation. If the destination exists, the operation \
            will fail. Works across different directories and can be used for simple renaming \
            within the same directory. Both source and destination must be within allowed \
            directories.",
        annotations(
            title = "Move File",
            read_only_hint = false,
            idempotent_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.move_file", skip(self))]
    async fn move_file(
        &self,
        Parameters(params): Parameters<MoveFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.move_file_impl(params))
    }

    #[tool(
        name = "search_files",
        description = "Recursively search for files and directories matching a pattern. The \
            patterns should be glob-style patterns that match paths relative to the working \
            directory. Use pattern like '*.ext' to match files in current directory, and \
            '**/*.ext' to match files in all subdirectories. Returns full paths to all \
            matching items. Great for finding files when you don't know their exact location. \
            Only searches within allowed directories.",
        annotations(title = "Search Files", read_only_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(name = "tool.search_files", skip(self))]
    async fn search_files(
        &self,
        Parameters(params): Parameters<SearchFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.search_files_impl(params))
    }

    #[tool(
        name = "get_file_info",
        description = "Retrieve detailed metadata about a file or directory. Returns comprehensive \
            information including size, creation time, last modified time, permissions, and \
            type. This tool is perfect for understanding file characteristics without reading \
            the actual content. Only works within allowed directories.",
        annotations(
            title = "Get File Info",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.get_file_info", skip(self))]
    async fn get_file_info(
        &self,
        Parameters(params): Parameters<PathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.get_file_info_impl(params))
    }

    #[tool(
        name = "list_allowed_directories",
        description = "Returns the list of directories that this server is allowed to access. \
            Subdirectories within these allowed directories are also accessible. Use this to \
            understand which directories and their nested paths are available before trying \
            to access files. Call it first: every other tool takes paths under these \
            directories.",
        annotations(
            title = "List Allowed Directories",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(name = "tool.list_allowed_directories", skip(self))]
    async fn list_allowed_directories(
        &self,
        Parameters(_params): Parameters<NoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.list_allowed_directories_impl())
    }
}

/// Tool bodies. Each returns a finished `CallToolResult`; `Err` values from
/// the helpers are already rendered tool errors, so `?` short-circuits with
/// the right message.
impl TemplateServer {
    fn read_text_file_impl(&self, params: ReadTextFileParams) -> CallToolResult {
        match self.read_text_file_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn read_text_file_inner(
        &self,
        params: ReadTextFileParams,
    ) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        if params.head.is_some() && params.tail.is_some() {
            return Err(tool_error(
                "Cannot specify both head and tail parameters simultaneously",
            ));
        }
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        let label = validated.requested.as_str();
        let text = if let Some(n) = params.tail {
            let n = line_count("tail", n)?;
            fsops::read_tail(&validated.real, n, cfg.max_file_bytes)
                .map_err(|err| io_error("read", label, &err))?
        } else if let Some(n) = params.head {
            let n = line_count("head", n)?;
            fsops::read_head(&validated.real, n, cfg.max_file_bytes)
                .map_err(|err| io_error("read", label, &err))?
        } else {
            let read = fsops::read_text_capped(&validated.real, cfg.max_file_bytes)
                .map_err(|err| io_error("read", label, &err))?;
            fsops::annotate_text(read, cfg.max_file_bytes)
        };
        Ok(text_result(text))
    }

    fn read_media_file_impl(&self, params: PathParams) -> CallToolResult {
        match self.read_media_file_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn read_media_file_inner(&self, params: PathParams) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        let label = validated.requested.as_str();
        let meta = fs::metadata(&validated.real).map_err(|err| io_error("stat", label, &err))?;
        if meta.is_dir() {
            return Err(tool_error(format!("{label} is a directory, not a file")));
        }
        if meta.len() > cfg.max_file_bytes {
            return Err(tool_error(format!(
                "File is {} bytes; read_media_file cap is FS_MAX_FILE_BYTES={}",
                meta.len(),
                cfg.max_file_bytes
            )));
        }
        let bytes = fs::read(&validated.real).map_err(|err| io_error("read", label, &err))?;
        let mime = fsops::mime_for(label);
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let uri = format!("file://{label}");
        let block = if mime.starts_with("image/") {
            ContentBlock::image(data, mime)
        } else if mime.starts_with("audio/") {
            ContentBlock::audio(data, mime)
        } else {
            ContentBlock::resource(ResourceContents::blob(data, uri.clone()).with_mime_type(mime))
        };
        let mut result = CallToolResult::success(vec![block]);
        result.structured_content = Some(json!({
            "uri": uri,
            "mimeType": mime,
            "size": bytes.len(),
            "encoding": "base64",
        }));
        Ok(result)
    }

    fn read_multiple_files_impl(&self, params: ReadMultipleFilesParams) -> CallToolResult {
        let cfg = match config() {
            Ok(cfg) => cfg,
            Err(err) => return err,
        };
        if params.paths.is_empty() {
            return tool_error("At least one file path must be provided");
        }
        let mut blocks = Vec::new();
        let mut failures = 0usize;
        let total = params.paths.len();
        for path in params.paths.iter().take(fsops::MAX_PATHS_PER_CALL) {
            let outcome = fsops::validate_path(&cfg, path, Missing::Deny).and_then(|validated| {
                fsops::read_text_capped(&validated.real, cfg.max_file_bytes)
                    .map(|read| fsops::annotate_text(read, cfg.max_file_bytes))
                    .map_err(|err| FsError(fsops::io_message("read", &validated.requested, &err)))
            });
            match outcome {
                Ok(content) => blocks.push(format!("{path}:\n{content}\n")),
                Err(err) => {
                    failures += 1;
                    blocks.push(format!("{path}: Error - {err}"));
                }
            }
        }
        let mut text = blocks.join("\n---\n");
        if total > fsops::MAX_PATHS_PER_CALL {
            text.push_str(&format!(
                "\n---\n[truncated: {total} paths requested, only the first {} were read]",
                fsops::MAX_PATHS_PER_CALL
            ));
        }
        if failures == blocks.len() {
            tool_error(text)
        } else {
            text_result(text)
        }
    }

    fn write_file_impl(&self, params: WriteFileParams) -> CallToolResult {
        match self.write_file_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn write_file_inner(&self, params: WriteFileParams) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        ensure_writable(&cfg, "write_file")?;
        if params.content.len() > fsops::MAX_WRITE_BYTES {
            return Err(tool_error(format!(
                "content is {} bytes; write_file accepts at most {} bytes",
                params.content.len(),
                fsops::MAX_WRITE_BYTES
            )));
        }
        let validated = validate(&cfg, &params.path, Missing::AllowLeaf)?;
        if validated.exists && validated.real.is_dir() {
            return Err(tool_error(format!(
                "{} is a directory, not a file",
                validated.requested
            )));
        }
        fsops::write_atomic(&validated.real, params.content.as_bytes())
            .map_err(|err| io_error("write", &validated.requested, &err))?;
        Ok(text_result(format!(
            "Successfully wrote to {}",
            params.path
        )))
    }

    fn edit_file_impl(&self, params: EditFileParams) -> CallToolResult {
        match self.edit_file_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn edit_file_inner(&self, params: EditFileParams) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        if !params.dry_run {
            ensure_writable(&cfg, "edit_file")?;
        }
        if params.edits.is_empty() {
            return Err(tool_error(
                "edits must contain at least one {oldText, newText} operation",
            ));
        }
        if params.edits.len() > fsops::MAX_EDITS_PER_CALL {
            return Err(tool_error(format!(
                "edit_file accepts at most {} edits per call (got {})",
                fsops::MAX_EDITS_PER_CALL,
                params.edits.len()
            )));
        }
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        let label = validated.requested.clone();
        let meta = fs::metadata(&validated.real).map_err(|err| io_error("stat", &label, &err))?;
        if meta.is_dir() {
            return Err(tool_error(format!("{label} is a directory, not a file")));
        }
        if meta.len() > cfg.max_file_bytes {
            return Err(tool_error(format!(
                "File is {} bytes; edit_file cap is FS_MAX_FILE_BYTES={}",
                meta.len(),
                cfg.max_file_bytes
            )));
        }
        let bytes = fs::read(&validated.real).map_err(|err| io_error("read", &label, &err))?;
        let original = String::from_utf8(bytes).map_err(|_| {
            tool_error(format!(
                "{label} is not valid UTF-8 text; edit_file only edits text files"
            ))
        })?;
        let original = fsops::normalize_line_endings(&original);
        let edits: Vec<Edit> = params
            .edits
            .into_iter()
            .map(|edit| Edit {
                old_text: edit.old_text,
                new_text: edit.new_text,
            })
            .collect();
        let modified = fsops::apply_edits(&original, &edits)?;
        let diff = fsops::fenced_diff(&original, &modified, &label);
        if !params.dry_run {
            fsops::write_atomic(&validated.real, modified.as_bytes())
                .map_err(|err| io_error("write", &label, &err))?;
        }
        Ok(text_result(diff))
    }

    fn create_directory_impl(&self, params: PathParams) -> CallToolResult {
        match self.create_directory_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn create_directory_inner(&self, params: PathParams) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        ensure_writable(&cfg, "create_directory")?;
        let validated = validate(&cfg, &params.path, Missing::AllowAncestors)?;
        if validated.exists && !validated.real.is_dir() {
            return Err(tool_error(format!(
                "{} already exists and is not a directory",
                validated.requested
            )));
        }
        fs::create_dir_all(&validated.real)
            .map_err(|err| io_error("mkdir", &validated.requested, &err))?;
        Ok(text_result(format!(
            "Successfully created directory {}",
            params.path
        )))
    }

    fn list_directory_impl(&self, params: PathParams) -> CallToolResult {
        match self.list_directory_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn list_directory_inner(&self, params: PathParams) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        let entries = fsops::list_entries(&validated.real)
            .map_err(|err| io_error("list", &validated.requested, &err))?;
        let truncated = entries.len() > cfg.max_results;
        let shown = &entries[..entries.len().min(cfg.max_results)];
        let mut lines: Vec<String> = shown
            .iter()
            .map(|entry| {
                format!(
                    "{} {}",
                    if entry.is_dir { "[DIR]" } else { "[FILE]" },
                    entry.name
                )
            })
            .collect();
        if truncated {
            lines.push(format!(
                "[truncated to FS_MAX_RESULTS={} entries]",
                cfg.max_results
            ));
        }
        let structured = json!({
            "path": validated.requested,
            "entries": shown.iter().map(|entry| json!({
                "name": entry.name,
                "type": if entry.is_dir { "directory" } else { "file" },
                "isSymlink": entry.is_symlink,
            })).collect::<Vec<_>>(),
            "truncated": truncated,
        });
        Ok(structured_result(lines.join("\n"), structured))
    }

    fn list_directory_with_sizes_impl(
        &self,
        params: ListDirectoryWithSizesParams,
    ) -> CallToolResult {
        match self.list_directory_with_sizes_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn list_directory_with_sizes_inner(
        &self,
        params: ListDirectoryWithSizesParams,
    ) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        let entries = fsops::list_entries(&validated.real)
            .map_err(|err| io_error("list", &validated.requested, &err))?;
        let mut detailed: Vec<(String, bool, u64)> = entries
            .iter()
            .map(|entry| {
                let size = if entry.is_dir {
                    0
                } else {
                    fs::metadata(&entry.path).map(|m| m.len()).unwrap_or(0)
                };
                (entry.name.clone(), entry.is_dir, size)
            })
            .collect();
        let total_files = detailed.iter().filter(|(_, is_dir, _)| !is_dir).count();
        let total_dirs = detailed.len() - total_files;
        let total_size: u64 = detailed.iter().map(|(_, _, size)| size).sum();
        if matches!(params.sort_by, Some(SortBy::Size)) {
            detailed.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        }
        let truncated = detailed.len() > cfg.max_results;
        let shown = &detailed[..detailed.len().min(cfg.max_results)];
        let mut lines: Vec<String> = shown
            .iter()
            .map(|(name, is_dir, size)| {
                let padded_name = pad_end(name, 30);
                let size_text = if *is_dir {
                    String::new()
                } else {
                    format!("{:>10}", fsops::format_size(*size))
                };
                format!(
                    "{} {padded_name} {size_text}",
                    if *is_dir { "[DIR]" } else { "[FILE]" }
                )
            })
            .collect();
        if truncated {
            lines.push(format!(
                "[truncated to FS_MAX_RESULTS={} entries]",
                cfg.max_results
            ));
        }
        lines.push(String::new());
        lines.push(format!(
            "Total: {total_files} files, {total_dirs} directories"
        ));
        lines.push(format!("Combined size: {}", fsops::format_size(total_size)));
        let structured = json!({
            "path": validated.requested,
            "entries": shown.iter().map(|(name, is_dir, size)| json!({
                "name": name,
                "type": if *is_dir { "directory" } else { "file" },
                "size": size,
            })).collect::<Vec<_>>(),
            "totalFiles": total_files,
            "totalDirectories": total_dirs,
            "combinedSize": total_size,
            "truncated": truncated,
        });
        Ok(structured_result(lines.join("\n"), structured))
    }

    fn directory_tree_impl(&self, params: DirectoryTreeParams) -> CallToolResult {
        match self.directory_tree_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn directory_tree_inner(
        &self,
        params: DirectoryTreeParams,
    ) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        if !validated.real.is_dir() {
            return Err(tool_error(format!(
                "{} is not a directory",
                validated.requested
            )));
        }
        let excludes = fsops::compile_globs(&params.exclude_patterns, true)?;
        let depth = cfg.clamp_depth(params.max_depth);
        let mut walk = Walk::new(&cfg, &excludes, depth);
        let nodes = walk
            .tree(&validated.real, "", 1)
            .map_err(|err| io_error("list", &validated.requested, &err))?;
        let tree: Vec<Value> = nodes.iter().map(fsops::TreeNode::to_json).collect();
        let text = serde_json::to_string_pretty(&tree).unwrap_or_else(|_| "[]".to_owned());
        let mut blocks = vec![ContentBlock::text(text)];
        if walk.truncated {
            blocks.push(ContentBlock::text(format!(
                "[truncated to FS_MAX_RESULTS={} entries; the final '…' node marks the cut. \
                 Narrow the path, add excludePatterns, or lower maxDepth.]",
                cfg.max_results
            )));
        }
        let mut result = CallToolResult::success(blocks);
        result.structured_content = Some(json!({
            "path": validated.requested,
            "entries": tree,
            "maxDepth": depth,
            "truncated": walk.truncated,
        }));
        Ok(result)
    }

    fn move_file_impl(&self, params: MoveFileParams) -> CallToolResult {
        match self.move_file_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn move_file_inner(&self, params: MoveFileParams) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        ensure_writable(&cfg, "move_file")?;
        let source = validate(&cfg, &params.source, Missing::Deny)?;
        let destination = validate(&cfg, &params.destination, Missing::AllowLeaf)?;
        if destination.real.starts_with(&source.real) && source.real.is_dir() {
            return Err(tool_error(format!(
                "Cannot move {} into itself ({})",
                source.requested, destination.requested
            )));
        }
        fsops::move_path(&source.real, &destination.real, &destination.requested)?;
        Ok(text_result(format!(
            "Successfully moved {} to {}",
            params.source, params.destination
        )))
    }

    fn search_files_impl(&self, params: SearchFilesParams) -> CallToolResult {
        match self.search_files_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn search_files_inner(
        &self,
        params: SearchFilesParams,
    ) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        if params.pattern.trim().is_empty() {
            return Err(tool_error("pattern must not be empty"));
        }
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        if !validated.real.is_dir() {
            return Err(tool_error(format!(
                "{} is not a directory",
                validated.requested
            )));
        }
        let pattern = fsops::compile_globs(std::slice::from_ref(&params.pattern), false)?;
        let excludes = fsops::compile_globs(&params.exclude_patterns, true)?;
        let depth = cfg.clamp_depth(params.max_depth);
        let mut walk = Walk::new(&cfg, &excludes, depth);
        let mut matches = Vec::new();
        walk.search(&validated.real, "", 1, &pattern, &mut matches)
            .map_err(|err| io_error("search", &validated.requested, &err))?;
        let mut text = if matches.is_empty() {
            "No matches found".to_owned()
        } else {
            matches.join("\n")
        };
        if walk.truncated {
            text.push_str(&format!(
                "\n[truncated to FS_MAX_RESULTS={} entries]",
                cfg.max_results
            ));
        }
        let structured = json!({
            "path": validated.requested,
            "pattern": params.pattern,
            "matches": matches,
            "truncated": walk.truncated,
        });
        Ok(structured_result(text, structured))
    }

    fn get_file_info_impl(&self, params: PathParams) -> CallToolResult {
        match self.get_file_info_inner(params) {
            Ok(result) | Err(result) => result,
        }
    }

    fn get_file_info_inner(&self, params: PathParams) -> Result<CallToolResult, CallToolResult> {
        let cfg = config()?;
        let validated = validate(&cfg, &params.path, Missing::Deny)?;
        let label = validated.requested.as_str();
        let meta = fs::metadata(&validated.real).map_err(|err| io_error("stat", label, &err))?;
        let is_symlink = fs::symlink_metadata(&validated.requested)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let created = fsops::timestamp(meta.created());
        let modified = fsops::timestamp(meta.modified());
        let accessed = fsops::timestamp(meta.accessed());
        let text = format!(
            "size: {}\ncreated: {created}\nmodified: {modified}\naccessed: {accessed}\n\
             isDirectory: {}\nisFile: {}\nisSymlink: {is_symlink}\npermissions: n/a",
            meta.len(),
            meta.is_dir(),
            meta.is_file(),
        );
        let structured = json!({
            "path": label,
            "size": meta.len(),
            "created": created,
            "modified": modified,
            "accessed": accessed,
            "isDirectory": meta.is_dir(),
            "isFile": meta.is_file(),
            "isSymlink": is_symlink,
            "permissions": "n/a",
        });
        Ok(structured_result(text, structured))
    }

    fn list_allowed_directories_impl(&self) -> CallToolResult {
        let cfg = match config() {
            Ok(cfg) => cfg,
            Err(err) => return err,
        };
        let mut lines = vec!["Allowed directories:".to_owned()];
        let mut directories = Vec::new();
        for dir in &cfg.allowed {
            let mounted = Config::is_mounted(dir);
            lines.push(if mounted {
                dir.clone()
            } else {
                format!("{dir} (NOT MOUNTED — check volumeMounts)")
            });
            directories.push(json!({ "path": dir, "mounted": mounted }));
        }
        if cfg.read_only {
            lines.push("(read-only: FS_READ_ONLY=true)".to_owned());
        }
        structured_result(
            lines.join("\n"),
            json!({
                "directories": directories,
                "readOnly": cfg.read_only,
                "maxFileBytes": cfg.max_file_bytes,
                "maxResults": cfg.max_results,
                "maxTreeDepth": cfg.max_tree_depth,
            }),
        )
    }
}

/// `String.prototype.padEnd` by character count.
fn pad_end(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TemplateServer {
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
            "Filesystem MCP server (a WebAssembly port of the official \
             @modelcontextprotocol/server-filesystem reference server) running sandboxed on \
             Cosmonic Desktop. The tools see only the host folders mounted into the workload, \
             under GUEST paths such as /data — call `list_allowed_directories` first and build \
             every path from what it returns; host paths like /home/... are always denied. \
             Read with read_text_file (head/tail for large files), read_media_file, \
             read_multiple_files, list_directory, list_directory_with_sizes, directory_tree, \
             search_files, get_file_info; write with write_file, edit_file (dryRun first), \
             create_directory, move_file — the writers are disabled when FS_READ_ONLY=true. \
             There is no delete tool.\n\n\
             This server publishes skills — playbooks describing when and how to use its \
             tools and what its errors mean. Read `skill://index.json` for the catalog, then \
             `skill://official-filesystem-mcp/SKILL.md` before non-trivial file work.",
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
