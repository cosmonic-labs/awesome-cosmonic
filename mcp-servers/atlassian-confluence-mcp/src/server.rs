//! The MCP server implementation: Confluence Cloud tools.
//!
//! Every tool resolves the client from the environment, validates and
//! percent-encodes its inputs, calls the REST API through
//! [`crate::confluence::Client`], and renders a compact structured result.
//! Failures the caller can act on are `CallToolResult::error` with a
//! structured `error` detail; JSON-RPC `invalid_params` is reserved for
//! requests the server cannot route at all.
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
use serde_json::{json, Value};

use crate::body;
use crate::confluence::{self, Client, Error, ErrorContext};
use crate::skills;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so do not keep per-session
/// state on this struct.
#[derive(Clone)]
pub struct TemplateServer {
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// v2 collection endpoints accept 1..=250 per page.
const V2_MAX_LIMIT: u32 = 250;
/// v1 search is clamped to 100 by this server (the endpoint accepts more, but
/// excerpts make large pages heavy).
const SEARCH_MAX_LIMIT: u32 = 100;
const DEFAULT_LIMIT: u32 = 25;
const ATTACHMENTS_DEFAULT_LIMIT: u32 = 50;
/// `get_page` body size window (characters).
const MIN_PAGE_CHARS: usize = 1_000;
const MAX_PAGE_CHARS: usize = 200_000;
const DEFAULT_PAGE_CHARS: usize = 60_000;
/// Rendered comment bodies are cut here.
const COMMENT_BODY_CHARS: usize = 4_000;
const MAX_TITLE_CHARS: usize = 255;
const MAX_VERSION_MESSAGE_CHARS: usize = 255;
const MAX_LABELS: usize = 20;
const MAX_LABEL_CHARS: usize = 255;
const MAX_COMMENT_CHARS: usize = 100_000;
const MAX_CQL_CHARS: usize = 10_000;
const MAX_CURSOR_CHARS: usize = 4_096;
const MAX_SPACE_KEYS: usize = 50;

const PAGE_SORTS: &[&str] = &[
    "id",
    "-id",
    "title",
    "-title",
    "created-date",
    "-created-date",
    "modified-date",
    "-modified-date",
];
const CHILD_SORTS: &[&str] = &[
    "id",
    "-id",
    "child-position",
    "-child-position",
    "created-date",
    "-created-date",
    "modified-date",
    "-modified-date",
    "title",
    "-title",
];
const COMMENT_SORTS: &[&str] = &[
    "created-date",
    "-created-date",
    "modified-date",
    "-modified-date",
];
const SPACE_TYPES: &[&str] = &["global", "collaboration", "knowledge_base", "personal"];
const SPACE_STATUSES: &[&str] = &["current", "archived"];
const LABEL_PREFIXES: &[&str] = &["global", "my", "team", "system"];
const BODY_FORMATS: &[&str] = &["markdown", "storage", "wiki"];
const PAGE_FORMATS: &[&str] = &["text", "storage", "atlas_doc_format", "view"];
const COMMENT_KINDS: &[&str] = &["footer", "inline"];
/// Characters Confluence refuses in label names (besides whitespace).
const LABEL_FORBIDDEN: &[char] = &[
    ':', ';', ',', '.', '?', '&', '[', ']', '(', ')', '#', '^', '*', '@', '!', '\'', '"', '<', '>',
    '/', '\\', '|', '{', '}', '~', '=', '+', '%', '$',
];

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// CQL query, e.g. `text ~ "kubernetes" AND type = page AND space = "ENG"
    /// ORDER BY lastmodified DESC`. Quote values; escape embedded quotes
    /// with a backslash. Either `cql` or `text` is required.
    #[serde(default)]
    pub cql: Option<String>,
    /// Free-text shortcut: becomes `text ~ "<text>" AND type = page ORDER BY
    /// lastmodified DESC`. Ignored when `cql` is given.
    #[serde(default)]
    pub text: Option<String>,
    /// Restrict to one space key (appends `AND space = "KEY"`).
    #[serde(default)]
    pub space_key: Option<String>,
    /// Results per page, 1..100 (default 25; larger values are clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from a previous result's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListPagesParams {
    /// Numeric space id, or a space key (resolved via the spaces API).
    /// Required when CONFLUENCE_SPACES_FILTER is set.
    #[serde(default)]
    pub space_id: Option<String>,
    /// Exact page title to look up (titles are unique per space).
    #[serde(default)]
    pub title: Option<String>,
    /// Page status filter: current (default), archived, trashed, deleted,
    /// draft, or a comma-separated combination.
    #[serde(default)]
    pub status: Option<String>,
    /// Sort: id | -id | title | -title | created-date | -created-date |
    /// modified-date | -modified-date.
    #[serde(default)]
    pub sort: Option<String>,
    /// Results per page, 1..250 (default 25; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from a previous result's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPageParams {
    /// Numeric page id, or a Confluence page URL (`.../pages/<id>/Title`).
    pub page_id: String,
    /// Body format: text (default; storage rendered to readable text),
    /// storage (raw XHTML, for editing), atlas_doc_format (ADF rendered to
    /// text, with the raw ADF JSON alongside), view (rendered HTML).
    #[serde(default)]
    pub format: Option<String>,
    /// Maximum body characters to return, 1000..200000 (default 60000;
    /// clamped). Longer bodies are cut with a marker and the full length.
    #[serde(default)]
    pub max_chars: Option<i64>,
    /// Read a specific historical version number instead of the latest.
    #[serde(default)]
    pub version: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPageChildrenParams {
    /// Numeric page id, or a Confluence page URL.
    pub page_id: String,
    /// Sort: id | -id | child-position | -child-position | created-date |
    /// -created-date | modified-date | -modified-date | title | -title.
    #[serde(default)]
    pub sort: Option<String>,
    /// Results per page, 1..250 (default 25; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from a previous result's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSpacesParams {
    /// Space keys to fetch (up to 50). Omit to list all visible spaces.
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Space type filter: global | collaboration | knowledge_base | personal.
    #[serde(default, rename = "type")]
    pub space_type: Option<String>,
    /// Status filter: current (default) | archived.
    #[serde(default)]
    pub status: Option<String>,
    /// Results per page, 1..250 (default 25; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from a previous result's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSpaceParams {
    /// Numeric space id or space key (e.g. `ENG`).
    pub space: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreatePageParams {
    /// Numeric space id, or a space key (resolved via the spaces API).
    pub space_id: String,
    /// Page title, 1..255 characters; must be unique within the space.
    pub title: String,
    /// Page body in the `body_format` representation (at most 4 MiB after
    /// conversion).
    pub body: String,
    /// markdown (default; converted to storage format, fenced code becomes
    /// code macros) | storage (raw XHTML, must be well-formed) | wiki
    /// (Confluence wiki markup).
    #[serde(default)]
    pub body_format: Option<String>,
    /// Parent page id or URL. Omitted = space root (not under the homepage;
    /// pass the space's homepageId to nest under home).
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatePageParams {
    /// Numeric page id, or a Confluence page URL.
    pub page_id: String,
    /// New title (1..255 chars). Omitted = keep the current title.
    #[serde(default)]
    pub title: Option<String>,
    /// New body, replacing the whole current body. Omitted = keep the
    /// current storage body. At least one of `title`/`body` is required.
    #[serde(default)]
    pub body: Option<String>,
    /// markdown (default) | storage | wiki — how to interpret `body`.
    #[serde(default)]
    pub body_format: Option<String>,
    /// Version comment shown in page history (up to 255 chars).
    #[serde(default)]
    pub version_message: Option<String>,
    /// Mark the edit as minor (no watcher notifications). Default false.
    #[serde(default)]
    pub minor_edit: Option<bool>,
    /// The version number you last read. When given and the live version
    /// differs, the update is refused without writing.
    #[serde(default)]
    pub expected_version: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeletePageParams {
    /// Numeric page id, or a Confluence page URL.
    pub page_id: String,
    /// Must be literally `true`. The page moves to the space trash
    /// (restorable in the Confluence UI); it is never purged.
    #[serde(default)]
    pub confirm: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetCommentsParams {
    /// Numeric page id, or a Confluence page URL.
    pub page_id: String,
    /// footer (default; page-level comments) | inline (anchored to text).
    #[serde(default)]
    pub kind: Option<String>,
    /// Sort: created-date | -created-date | modified-date | -modified-date.
    #[serde(default)]
    pub sort: Option<String>,
    /// Results per page, 1..250 (default 25; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from a previous result's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddCommentParams {
    /// Page to comment on (id or URL). Required unless
    /// `reply_to_comment_id` is given.
    #[serde(default)]
    pub page_id: Option<String>,
    /// Footer comment id to reply to. When set, the comment becomes a reply
    /// and `page_id` is not sent.
    #[serde(default)]
    pub reply_to_comment_id: Option<String>,
    /// Comment body, 1..100000 characters, in `body_format`.
    pub body: String,
    /// markdown (default) | storage | wiki.
    #[serde(default)]
    pub body_format: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetLabelsParams {
    /// Numeric page id, or a Confluence page URL.
    pub page_id: String,
    /// Label prefix filter: global | my | team | system.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Results per page, 1..250 (default 25; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from a previous result's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddLabelParams {
    /// Numeric page id, or a Confluence page URL.
    pub page_id: String,
    /// 1..20 label names. Each is lowercased; no spaces or punctuation such
    /// as `: ; , . ? & ( ) [ ] # ^ * @ !` (use hyphens: `release-notes`).
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAttachmentsParams {
    /// Numeric page id, or a Confluence page URL.
    pub page_id: String,
    /// MIME type filter, e.g. `application/pdf`.
    #[serde(default)]
    pub media_type: Option<String>,
    /// Exact filename filter.
    #[serde(default)]
    pub filename: Option<String>,
    /// Results per page, 1..250 (default 50; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from a previous result's `next_cursor`.
    #[serde(default)]
    pub cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A tool-level error with a structured `error` detail.
fn tool_error(message: String, detail: Value) -> CallToolResult {
    let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
    result.structured_content = Some(json!({ "error": detail }));
    result
}

/// A tool-level error for input the server refused before dialing.
fn refuse(kind: &str, message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    tool_error(
        message.clone(),
        json!({ "kind": kind, "retryable": false, "message": message }),
    )
}

/// Phrases a client error for the caller.
fn confluence_error(err: &Error, ctx: &ErrorContext, site: Option<&str>) -> CallToolResult {
    let (message, detail) = confluence::describe(err, ctx, site);
    tool_error(message, detail)
}

/// Resolves the client or produces the actionable not-configured error.
fn client(operation: &str) -> Result<Client, CallToolResult> {
    Client::from_env().map_err(|err| {
        confluence_error(
            &err,
            &ErrorContext::new(operation),
            confluence::env_setting(confluence::SITE_ENV).as_deref(),
        )
    })
}

/// `Some(refusal)` when `CONFLUENCE_READ_ONLY` gates the write tools.
fn write_gate(tool: &str) -> Option<CallToolResult> {
    confluence::read_only().then(|| {
        refuse(
            "read_only",
            format!(
                "{tool} is disabled: write tools are disabled ({}=true). Nothing was sent to \
                 Confluence. Change the named config in deploy/workload.yaml and re-apply to \
                 enable writes.",
                confluence::READ_ONLY_ENV
            ),
        )
    })
}

fn page_id(raw: &str) -> Result<String, CallToolResult> {
    confluence::parse_page_id(raw).map_err(|message| refuse("bad_page_id", message))
}

fn limit(value: Option<i64>, default: u32, max: u32) -> Result<u32, CallToolResult> {
    confluence::clamp_limit(value, default, max).map_err(|message| refuse("bad_limit", message))
}

fn cursor(value: Option<String>) -> Result<Option<String>, CallToolResult> {
    match value {
        None => Ok(None),
        Some(cursor) => {
            let cursor = cursor.trim().to_owned();
            if cursor.is_empty() {
                Ok(None)
            } else if cursor.chars().count() > MAX_CURSOR_CHARS {
                Err(refuse(
                    "bad_cursor",
                    format!(
                        "cursor is longer than {MAX_CURSOR_CHARS} characters; pass the \
                         next_cursor value from a previous result verbatim"
                    ),
                ))
            } else {
                Ok(Some(cursor))
            }
        }
    }
}

/// Validates an enumerated option against `allowed` (case-insensitive) and
/// returns its canonical spelling.
fn choice(
    name: &str,
    value: Option<String>,
    allowed: &[&str],
    default: Option<&str>,
) -> Result<Option<String>, CallToolResult> {
    let Some(value) = value
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
    else {
        return Ok(default.map(str::to_owned));
    };
    match allowed
        .iter()
        .find(|option| option.eq_ignore_ascii_case(&value))
    {
        Some(option) => Ok(Some((*option).to_owned())),
        None => Err(refuse(
            "bad_option",
            format!(
                "{name} must be one of {}; got {:?}",
                allowed.join(" | "),
                value.chars().take(40).collect::<String>()
            ),
        )),
    }
}

/// `{ "results": [...], "count": n, "next_cursor": ..., "limit": n }`.
fn page_result(
    results: Vec<Value>,
    raw: &Value,
    limit: u32,
    mut extra: serde_json::Map<String, Value>,
) -> Value {
    let next = confluence::next_cursor(raw);
    let mut value = json!({
        "count": results.len(),
        "limit": limit,
        "results": results,
        "next_cursor": next,
    });
    if let Value::Object(map) = &mut value {
        map.append(&mut extra);
    }
    value
}

fn results_array(raw: &Value) -> Vec<Value> {
    raw.get("results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Validates a title for create/update. Control characters XML forbids are
/// dropped first (titles travel inside the storage-format document too).
fn title_param(title: &str) -> Result<String, CallToolResult> {
    let title = body::strip_xml_illegal(title);
    let title = title.trim();
    let count = title.chars().count();
    if count == 0 {
        return Err(refuse("bad_title", "title must not be empty"));
    }
    if count > MAX_TITLE_CHARS {
        return Err(refuse(
            "bad_title",
            format!("title is {count} characters; Confluence allows at most {MAX_TITLE_CHARS}"),
        ));
    }
    Ok(title.to_owned())
}

/// Converts a caller-supplied body into `(representation, value)` for a
/// `body` object, enforcing the size cap on both sides of the conversion.
/// Characters XML 1.0 forbids (NUL, ESC, form feed, …) are dropped for every
/// format: storage is XML, and Confluence rejects them in wiki markup too.
fn convert_body(body: &str, format: Option<String>) -> Result<(String, String), CallToolResult> {
    let format = choice("body_format", format, BODY_FORMATS, Some("markdown"))?
        .unwrap_or_else(|| "markdown".to_owned());
    let body = body::strip_xml_illegal(body);
    let body = body.as_ref();
    if body.len() > confluence::MAX_BODY_BYTES {
        return Err(refuse(
            "body_too_large",
            format!(
                "body is {} bytes; the limit is {} bytes (Confluence rejects requests over 5 MB). \
                 Split the content across pages or attach it as a file.",
                body.len(),
                confluence::MAX_BODY_BYTES
            ),
        ));
    }
    let (representation, value) = match format.as_str() {
        "markdown" => ("storage", body::markdown_to_storage(body)),
        "storage" => ("storage", body.to_owned()),
        _ => ("wiki", body.to_owned()),
    };
    if value.len() > confluence::MAX_BODY_BYTES {
        return Err(refuse(
            "body_too_large",
            format!(
                "body is {} bytes after conversion to storage format; the limit is {} bytes. \
                 Split the content across pages.",
                value.len(),
                confluence::MAX_BODY_BYTES
            ),
        ));
    }
    Ok((representation.to_owned(), value))
}

/// A space resolved from an id or key.
struct SpaceRef {
    id: String,
    key: String,
    space: Value,
}

/// Resolves a numeric space id or a space key to a space object, and
/// enforces `CONFLUENCE_SPACES_FILTER`.
async fn resolve_space(
    client: &Client,
    raw: &str,
    operation: &str,
) -> Result<SpaceRef, CallToolResult> {
    let value = raw.trim();
    let site = Some(client.config.site.as_str());
    let (url, ctx) = if confluence::is_numeric_id(value) {
        (
            client.v2_url(
                &format!("/spaces/{value}"),
                &[("description-format", "plain".to_owned())],
            ),
            ErrorContext::new(format!("{operation} (GET /spaces/{value})")).not_found(format!(
                "No space with id {value} is visible to this account; list_spaces shows the numeric \
                 ids and keys the account can see."
            )),
        )
    } else if confluence::is_space_key(value) {
        (
            client.v2_url(
                "/spaces",
                &[
                    ("keys", value.to_owned()),
                    ("limit", "1".to_owned()),
                    ("description-format", "plain".to_owned()),
                ],
            ),
            ErrorContext::new(format!("{operation} (GET /spaces?keys={value})")),
        )
    } else {
        return Err(refuse(
            "bad_space",
            format!(
                "space {:?} is neither a numeric space id nor a space key (letters, digits, _ ~ : . -)",
                value.chars().take(60).collect::<String>()
            ),
        ));
    };
    let reply = client
        .get(url)
        .await
        .map_err(|err| confluence_error(&err, &ctx, site))?;
    let space = if confluence::is_numeric_id(value) {
        reply
    } else {
        match results_array(&reply).into_iter().next() {
            Some(space) => space,
            None => {
                return Err(refuse(
                    "space_not_found",
                    format!(
                        "no space with key {value:?} is visible to this account (keys are \
                         case-sensitive; list_spaces shows what the account can see)"
                    ),
                ));
            }
        }
    };
    let id = space
        .get("id")
        .and_then(|i| match i {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let key = space
        .get("key")
        .and_then(|k| k.as_str())
        .unwrap_or_default()
        .to_owned();
    if id.is_empty() {
        return Err(refuse(
            "decode",
            "Confluence returned a space without an id; cannot continue",
        ));
    }
    if !client.config.space_allowed(&key) {
        return Err(refuse(
            "space_filtered",
            format!(
                "space {key} (id {id}) is outside {}={}; this deployment is scoped to those spaces",
                confluence::SPACES_FILTER_ENV,
                client.config.spaces_filter.join(",")
            ),
        ));
    }
    Ok(SpaceRef { id, key, space })
}

fn extra(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

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
        description = "Verify the Confluence credentials and report status ok|missing|invalid|insufficient \
                       with the identity, the route in use, the write gates, and remediation. Call \
                       this first; never retry a missing/invalid result."
    )]
    #[tracing::instrument(name = "tool.check_auth", skip(self))]
    async fn check_auth(&self) -> Result<CallToolResult, ErrorData> {
        let site = confluence::env_setting(confluence::SITE_ENV);
        let client = match Client::from_env() {
            Ok(client) => client,
            Err(err) => {
                let (message, detail) = confluence::describe(
                    &err,
                    &ErrorContext::new("check credentials"),
                    site.as_deref(),
                );
                let status = match err {
                    Error::NotConfigured { .. } => "missing",
                    _ => "error",
                };
                let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
                result.structured_content = Some(json!({
                    "status": status,
                    "auth": "basic",
                    "credential": { "ref": confluence::SECRET_REF, "env": confluence::TOKEN_ENV, "obtainUrl": confluence::TOKEN_URL },
                    "error": detail,
                    "remediation": confluence::remediation(site.as_deref()),
                }));
                return Ok(result);
            }
        };
        let config = &client.config;
        let base = json!({
            "auth": "basic",
            "site": config.site,
            "base_url": config.base_url,
            "route": config.route(),
            "email": config.email,
            "read_only": confluence::read_only(),
            "allow_delete": confluence::allow_delete(),
            "spaces_filter": config.spaces_filter,
            "credential": { "ref": confluence::SECRET_REF, "env": confluence::TOKEN_ENV, "obtainUrl": confluence::TOKEN_URL, "scopes": confluence::SCOPES },
        });
        match client.get(client.v1_url("/user/current", &[])).await {
            Ok(me) => {
                let mut value = base;
                value["status"] = json!("ok");
                value["account"] = confluence::simplify_user(&me);
                value["remediation"] = Value::Null;
                Ok(CallToolResult::structured(value))
            }
            Err(err) => {
                let ctx = ErrorContext::new("check credentials (GET /wiki/rest/api/user/current)");
                let (message, detail) = confluence::describe(&err, &ctx, Some(&config.site));
                let status = match err.status() {
                    Some(401) => "invalid",
                    Some(403) => "insufficient",
                    _ => "error",
                };
                let mut value = base;
                value["status"] = json!(status);
                value["error"] = detail;
                value["remediation"] = json!(confluence::remediation(Some(&config.site)));
                let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
                result.structured_content = Some(value);
                Ok(result)
            }
        }
    }

    #[tool(
        description = "Identity of the authenticated account (GET /wiki/rest/api/user/current): \
                       accountId, email, displayName, publicName, accountType, timeZone. Cheap \
                       connectivity check."
    )]
    #[tracing::instrument(name = "tool.get_current_user", skip(self))]
    async fn get_current_user(&self) -> Result<CallToolResult, ErrorData> {
        let client = match client("get the current user") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        match client.get(client.v1_url("/user/current", &[])).await {
            Ok(me) => {
                let mut value = confluence::simplify_user(&me);
                value["site"] = json!(client.config.site);
                value["route"] = json!(client.config.route());
                Ok(CallToolResult::structured(value))
            }
            Err(err) => Ok(confluence_error(
                &err,
                &ErrorContext::new("get the current user (GET /wiki/rest/api/user/current)"),
                Some(&client.config.site),
            )),
        }
    }

    #[tool(
        description = "CQL search across pages, blog posts, attachments and comments \
                       (GET /wiki/rest/api/search). Pass `cql` or a free-text `text`. Returns \
                       id, type, title, spaceKey, url, lastModified, excerpt, totalSize and \
                       next_cursor. Limit 1..100."
    )]
    #[tracing::instrument(name = "tool.search", skip(self))]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("search") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let limit = match limit(params.limit, DEFAULT_LIMIT, SEARCH_MAX_LIMIT) {
            Ok(limit) => limit,
            Err(result) => return Ok(result),
        };
        let cursor = match cursor(params.cursor) {
            Ok(cursor) => cursor,
            Err(result) => return Ok(result),
        };
        let mut cql = match (
            params
                .cql
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty()),
            params
                .text
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty()),
        ) {
            (Some(cql), _) => cql,
            (None, Some(text)) => format!(
                "text ~ {} AND type = page ORDER BY lastmodified DESC",
                confluence::cql_quote(&text)
            ),
            (None, None) => {
                return Ok(refuse(
                    "bad_query",
                    "pass `cql` (e.g. text ~ \"kubernetes\" AND type = page) or `text`",
                ));
            }
        };
        if cql.chars().count() > MAX_CQL_CHARS {
            return Ok(refuse(
                "bad_query",
                format!("cql is longer than {MAX_CQL_CHARS} characters"),
            ));
        }
        // Space scoping: an explicit key (which must pass the filter), or the
        // deployment filter itself. ORDER BY must stay at the end, so the
        // clause is inserted before it.
        let space_clause = match params
            .space_key
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
        {
            Some(key) => {
                if !confluence::is_space_key(&key) {
                    return Ok(refuse(
                        "bad_space",
                        format!(
                            "space_key {:?} is not a space key",
                            key.chars().take(60).collect::<String>()
                        ),
                    ));
                }
                if !client.config.space_allowed(&key) {
                    return Ok(refuse(
                        "space_filtered",
                        format!(
                            "space {key} is outside {}={}",
                            confluence::SPACES_FILTER_ENV,
                            client.config.spaces_filter.join(",")
                        ),
                    ));
                }
                Some(format!("space = {}", confluence::cql_quote(&key)))
            }
            None if !client.config.spaces_filter.is_empty() => Some(format!(
                "space in ({})",
                client
                    .config
                    .spaces_filter
                    .iter()
                    .map(|k| confluence::cql_quote(k))
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            None => None,
        };
        if let Some(clause) = space_clause {
            let upper = cql.to_ascii_uppercase();
            cql = match upper.rfind(" ORDER BY ") {
                Some(index) if cql.is_char_boundary(index) => {
                    format!("({}) AND {clause}{}", cql[..index].trim(), &cql[index..])
                }
                _ => format!("({cql}) AND {clause}"),
            };
        }
        let mut query: Vec<(&str, String)> = vec![
            ("cql", cql.clone()),
            ("limit", limit.to_string()),
            ("excerpt", "highlight".to_owned()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let url = client.v1_url("/search", &query);
        let ctx = ErrorContext::new("search (GET /wiki/rest/api/search)").bad_request(
            "Invalid CQL: quote values (space = \"ENG\"), use text ~ / title ~ / label = / type = / \
             lastmodified > / ancestor =, and avoid user.* fields. The `text` parameter builds a \
             safe query for you.",
        );
        match client.get(url).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(|r| confluence::simplify_search_result(r, &client.config))
                    .collect();
                let echoed = raw.get("cqlQuery").cloned().unwrap_or_else(|| json!(cql));
                Ok(CallToolResult::structured(page_result(
                    results,
                    &raw,
                    limit,
                    extra(&[
                        ("cql", echoed),
                        (
                            "totalSize",
                            raw.get("totalSize").cloned().unwrap_or(Value::Null),
                        ),
                    ]),
                )))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "List pages, optionally by space and exact title (the way to turn a title \
                       into a numeric page id). GET /wiki/api/v2/pages. Returns id, title, \
                       status, spaceId, parentId, version, url and next_cursor. Limit 1..250."
    )]
    #[tracing::instrument(name = "tool.list_pages", skip(self))]
    async fn list_pages(
        &self,
        Parameters(params): Parameters<ListPagesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("list pages") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let limit = match limit(params.limit, DEFAULT_LIMIT, V2_MAX_LIMIT) {
            Ok(limit) => limit,
            Err(result) => return Ok(result),
        };
        let cursor = match cursor(params.cursor) {
            Ok(cursor) => cursor,
            Err(result) => return Ok(result),
        };
        let sort = match choice("sort", params.sort, PAGE_SORTS, None) {
            Ok(sort) => sort,
            Err(result) => return Ok(result),
        };
        let status = match params
            .status
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
        {
            None => "current".to_owned(),
            Some(status) => {
                let valid = status.split(',').all(|part| {
                    matches!(
                        part.trim(),
                        "current" | "archived" | "trashed" | "deleted" | "draft"
                    )
                });
                if !valid {
                    return Ok(refuse(
                        "bad_option",
                        "status must be a comma-separated subset of current | archived | trashed | \
                         deleted | draft",
                    ));
                }
                status
            }
        };
        let space = match params
            .space_id
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
        {
            Some(raw) => {
                // A numeric id with no filter needs no lookup; anything else
                // resolves (and enforces the filter) through the spaces API.
                if confluence::is_numeric_id(&raw) && client.config.spaces_filter.is_empty() {
                    Some((raw, None))
                } else {
                    match resolve_space(&client, &raw, "list pages").await {
                        Ok(space) => Some((space.id, Some(space.key))),
                        Err(result) => return Ok(result),
                    }
                }
            }
            None if !client.config.spaces_filter.is_empty() => {
                return Ok(refuse(
                    "space_required",
                    format!(
                        "{} is set ({}); pass space_id (one of those keys or its numeric id)",
                        confluence::SPACES_FILTER_ENV,
                        client.config.spaces_filter.join(",")
                    ),
                ));
            }
            None => None,
        };
        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string()), ("status", status)];
        if let Some((id, _)) = &space {
            query.push(("space-id", id.clone()));
        }
        if let Some(title) = params
            .title
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
        {
            if title.chars().count() > MAX_TITLE_CHARS {
                return Ok(refuse(
                    "bad_title",
                    format!("title is longer than {MAX_TITLE_CHARS} characters"),
                ));
            }
            query.push(("title", title));
        }
        if let Some(sort) = sort {
            query.push(("sort", sort));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let url = client.v2_url("/pages", &query);
        let ctx = ErrorContext::new("list pages (GET /wiki/api/v2/pages)");
        match client.get(url).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(|p| confluence::simplify_page(p, &client.config))
                    .collect();
                let mut more = serde_json::Map::new();
                if let Some((id, key)) = space {
                    more.insert("spaceId".to_owned(), json!(id));
                    if let Some(key) = key {
                        more.insert("spaceKey".to_owned(), json!(key));
                    }
                }
                Ok(CallToolResult::structured(page_result(
                    results, &raw, limit, more,
                )))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Read one page (GET /wiki/api/v2/pages/{id}): metadata, version, labels, \
                       urls and the body — as readable text (default), raw storage XHTML for \
                       editing, ADF, or rendered view HTML. Accepts a page URL. Bodies over \
                       max_chars are truncated with a marker."
    )]
    #[tracing::instrument(name = "tool.get_page", skip(self))]
    async fn get_page(
        &self,
        Parameters(params): Parameters<GetPageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("get page") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        let format = match choice("format", params.format, PAGE_FORMATS, Some("text")) {
            Ok(format) => format.unwrap_or_else(|| "text".to_owned()),
            Err(result) => return Ok(result),
        };
        let max_chars = match params.max_chars {
            None => DEFAULT_PAGE_CHARS,
            Some(n) if n < 0 => {
                return Ok(refuse(
                    "bad_limit",
                    format!(
                        "max_chars must be positive ({MIN_PAGE_CHARS}..{MAX_PAGE_CHARS}), got {n}"
                    ),
                ));
            }
            Some(n) => usize::try_from(n)
                .unwrap_or(usize::MAX)
                .clamp(MIN_PAGE_CHARS, MAX_PAGE_CHARS),
        };
        let body_format = match format.as_str() {
            "text" | "storage" => "storage",
            "atlas_doc_format" => "atlas_doc_format",
            _ => "view",
        };
        let mut query: Vec<(&str, String)> = vec![
            ("body-format", body_format.to_owned()),
            ("include-labels", "true".to_owned()),
            ("include-version", "true".to_owned()),
        ];
        match params.version {
            None => {}
            Some(n) if n >= 1 => query.push(("version", n.to_string())),
            Some(n) => {
                return Ok(refuse(
                    "bad_version",
                    format!("version must be a positive version number, got {n}"),
                ));
            }
        }
        let url = client.v2_url(&format!("/pages/{id}"), &query);
        let ctx = ErrorContext::new(format!("get page {id} (GET /wiki/api/v2/pages/{id})"))
            .not_found(format!(
                "No page with id {id} is visible to this account: it does not exist, is trashed \
                 or a draft, the version does not exist, or the account cannot view it. Verify the \
                 id via search or list_pages."
            ));
        let raw = match client.get(url).await {
            Ok(raw) => raw,
            Err(err) => return Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        };
        let mut value = confluence::simplify_page(&raw, &client.config);
        let body_value = raw
            .get("body")
            .and_then(|b| b.get(body_format))
            .and_then(|f| f.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let rendered = match format.as_str() {
            "text" => body::storage_to_text(body_value),
            "atlas_doc_format" => match serde_json::from_str::<Value>(body_value) {
                Ok(doc) => {
                    value["adf"] = doc.clone();
                    body::adf_to_text(&doc)
                }
                Err(_) => body_value.to_owned(),
            },
            _ => body_value.to_owned(),
        };
        let total_chars = rendered.chars().count();
        let (text, truncated) = body::truncate_chars(&rendered, max_chars);
        value["format"] = json!(format);
        value["body"] = json!(text);
        value["body_chars"] = json!(total_chars);
        value["body_truncated"] = json!(truncated);
        if truncated {
            value["body_note"] = json!(format!(
                "body cut at {max_chars} of {total_chars} characters; raise max_chars (up to \
                 {MAX_PAGE_CHARS}) or read format=storage for the exact source"
            ));
        }
        if let Some(adf) = value.get_mut("adf") {
            // Keep the raw ADF bounded too: it is for reference, not a dump.
            if adf.to_string().len() > max_chars.saturating_mul(4) {
                *adf = json!({ "omitted": "ADF JSON larger than 4×max_chars; raise max_chars" });
            }
        }
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        description = "Direct child pages of a page (GET /wiki/api/v2/pages/{id}/children): id, \
                       title, status, spaceId, childPosition and next_cursor. Limit 1..250."
    )]
    #[tracing::instrument(name = "tool.get_page_children", skip(self))]
    async fn get_page_children(
        &self,
        Parameters(params): Parameters<GetPageChildrenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("get page children") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        let limit = match limit(params.limit, DEFAULT_LIMIT, V2_MAX_LIMIT) {
            Ok(limit) => limit,
            Err(result) => return Ok(result),
        };
        let cursor = match cursor(params.cursor) {
            Ok(cursor) => cursor,
            Err(result) => return Ok(result),
        };
        let sort = match choice("sort", params.sort, CHILD_SORTS, None) {
            Ok(sort) => sort,
            Err(result) => return Ok(result),
        };
        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(sort) = sort {
            query.push(("sort", sort));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let url = client.v2_url(&format!("/pages/{id}/children"), &query);
        let ctx = ErrorContext::new(format!("get children of page {id}")).not_found(format!(
            "No page with id {id} is visible to this account; verify the id via search or list_pages."
        ));
        match client.get(url).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(|p| confluence::simplify_page(p, &client.config))
                    .collect();
                Ok(CallToolResult::structured(page_result(
                    results,
                    &raw,
                    limit,
                    extra(&[("parentId", json!(id))]),
                )))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "List spaces (GET /wiki/api/v2/spaces): id, key, name, type, status, \
                       homepageId, url and next_cursor. Use it to turn a space key into the \
                       numeric spaceId that create_page and list_pages take. Limit 1..250."
    )]
    #[tracing::instrument(name = "tool.list_spaces", skip(self))]
    async fn list_spaces(
        &self,
        Parameters(params): Parameters<ListSpacesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("list spaces") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let limit = match limit(params.limit, DEFAULT_LIMIT, V2_MAX_LIMIT) {
            Ok(limit) => limit,
            Err(result) => return Ok(result),
        };
        let cursor = match cursor(params.cursor) {
            Ok(cursor) => cursor,
            Err(result) => return Ok(result),
        };
        let space_type = match choice("type", params.space_type, SPACE_TYPES, None) {
            Ok(value) => value,
            Err(result) => return Ok(result),
        };
        let status = match choice("status", params.status, SPACE_STATUSES, Some("current")) {
            Ok(value) => value,
            Err(result) => return Ok(result),
        };
        let mut keys: Vec<String> = Vec::new();
        for key in params.keys.unwrap_or_default() {
            let key = key.trim().to_owned();
            if key.is_empty() {
                continue;
            }
            if !confluence::is_space_key(&key) {
                return Ok(refuse(
                    "bad_space",
                    format!(
                        "key {:?} is not a space key",
                        key.chars().take(60).collect::<String>()
                    ),
                ));
            }
            if keys.len() >= MAX_SPACE_KEYS {
                return Ok(refuse(
                    "bad_space",
                    format!("at most {MAX_SPACE_KEYS} keys per call"),
                ));
            }
            keys.push(key);
        }
        let filtered_out: Vec<String> = keys
            .iter()
            .filter(|key| !client.config.space_allowed(key))
            .cloned()
            .collect();
        if !filtered_out.is_empty() {
            return Ok(refuse(
                "space_filtered",
                format!(
                    "keys {} are outside {}={}",
                    filtered_out.join(","),
                    confluence::SPACES_FILTER_ENV,
                    client.config.spaces_filter.join(",")
                ),
            ));
        }
        if keys.is_empty() {
            keys = client.config.spaces_filter.clone();
        }
        let mut query: Vec<(&str, String)> = vec![
            ("limit", limit.to_string()),
            ("description-format", "plain".to_owned()),
        ];
        if !keys.is_empty() {
            query.push(("keys", keys.join(",")));
        }
        if let Some(space_type) = space_type {
            query.push(("type", space_type));
        }
        if let Some(status) = status {
            query.push(("status", status));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let url = client.v2_url("/spaces", &query);
        let ctx = ErrorContext::new("list spaces (GET /wiki/api/v2/spaces)");
        match client.get(url).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(|s| confluence::simplify_space(s, &client.config))
                    .collect();
                Ok(CallToolResult::structured(page_result(
                    results,
                    &raw,
                    limit,
                    extra(&[("keys", json!(keys))]),
                )))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "One space by numeric id or key (GET /wiki/api/v2/spaces/{id}), with its \
                       plain description and homepageId."
    )]
    #[tracing::instrument(name = "tool.get_space", skip(self))]
    async fn get_space(
        &self,
        Parameters(params): Parameters<GetSpaceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("get space") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        match resolve_space(&client, &params.space, "get space").await {
            Ok(space) => Ok(CallToolResult::structured(confluence::simplify_space(
                &space.space,
                &client.config,
            ))),
            Err(result) => Ok(result),
        }
    }

    #[tool(
        description = "Create a page (POST /wiki/api/v2/pages) in a space, optionally under a \
                       parent, from markdown (default), storage XHTML or wiki markup. Returns id, \
                       version and url. Gated by CONFLUENCE_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.create_page", skip(self))]
    async fn create_page(
        &self,
        Parameters(params): Parameters<CreatePageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = write_gate("create_page") {
            return Ok(refusal);
        }
        let client = match client("create page") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let title = match title_param(&params.title) {
            Ok(title) => title,
            Err(result) => return Ok(result),
        };
        let (representation, value) = match convert_body(&params.body, params.body_format) {
            Ok(converted) => converted,
            Err(result) => return Ok(result),
        };
        let parent = match params
            .parent_id
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
        {
            Some(raw) => match page_id(&raw) {
                Ok(id) => Some(id),
                Err(result) => return Ok(result),
            },
            None => None,
        };
        let space = match resolve_space(&client, &params.space_id, "create page").await {
            Ok(space) => space,
            Err(result) => return Ok(result),
        };
        let mut payload = json!({
            "spaceId": space.id,
            "status": "current",
            "title": title,
            "body": { "representation": representation, "value": value },
        });
        if let Some(parent) = &parent {
            payload["parentId"] = json!(parent);
        }
        let url = client.v2_url("/pages", &[]);
        let ctx = ErrorContext::new(format!(
            "create page {:?} in space {} (POST /wiki/api/v2/pages)",
            title, space.key
        ))
        .not_found(format!(
            "Space {} (id {}) or parent page {} is not visible, or the account lacks 'add page' \
             permission there (Confluence folds that into 404).",
            space.key,
            space.id,
            parent.as_deref().unwrap_or("(none)")
        ));
        match client.post(url, &payload).await {
            Ok(created) => {
                let mut result = confluence::simplify_page(&created, &client.config);
                result["created"] = json!(true);
                result["spaceKey"] = json!(space.key);
                result["representation"] = json!(representation);
                Ok(CallToolResult::structured(result))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Update a page's title and/or body (PUT /wiki/api/v2/pages/{id}). Reads \
                       the live version first and sends version+1; refuses when expected_version \
                       is given and differs. The body replaces the whole page. Gated by \
                       CONFLUENCE_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.update_page", skip(self))]
    async fn update_page(
        &self,
        Parameters(params): Parameters<UpdatePageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = write_gate("update_page") {
            return Ok(refusal);
        }
        let client = match client("update page") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        let title = match params
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(title) => match title_param(title) {
                Ok(title) => Some(title),
                Err(result) => return Ok(result),
            },
            None => None,
        };
        let new_body = match params.body {
            Some(body) => match convert_body(&body, params.body_format) {
                Ok(converted) => Some(converted),
                Err(result) => return Ok(result),
            },
            None => None,
        };
        if title.is_none() && new_body.is_none() {
            return Ok(refuse(
                "nothing_to_update",
                "pass a new title, a new body, or both",
            ));
        }
        let version_message = params
            .version_message
            .map(|m| {
                m.trim()
                    .chars()
                    .filter(|ch| body::xml_allowed(*ch))
                    .take(MAX_VERSION_MESSAGE_CHARS)
                    .collect::<String>()
            })
            .filter(|m| !m.is_empty());
        let minor_edit = params.minor_edit.unwrap_or(false);

        // 1. Read the live page: current version, title, status and body.
        let read_url = client.v2_url(
            &format!("/pages/{id}"),
            &[("body-format", "storage".to_owned())],
        );
        let read_ctx =
            ErrorContext::new(format!("read page {id} before updating it")).not_found(format!(
                "No page with id {id} is visible to this account; verify the id via search or \
                 list_pages. Nothing was written."
            ));
        let current = match client.get(read_url).await {
            Ok(page) => page,
            Err(err) => return Ok(confluence_error(&err, &read_ctx, Some(&client.config.site))),
        };
        let live_version = current
            .get("version")
            .and_then(|v| v.get("number"))
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        if live_version < 1 {
            return Ok(refuse(
                "decode",
                format!("page {id} came back without a version number; nothing was written"),
            ));
        }
        if let Some(expected) = params.expected_version {
            if expected != live_version {
                return Ok(tool_error(
                    format!(
                        "expected_version mismatch: live version is {live_version}, you expected \
                         {expected}. The page changed since you read it; no PUT was sent. Read it \
                         again with get_page, reconcile, then update_page with expected_version={live_version}."
                    ),
                    json!({
                        "kind": "version_mismatch",
                        "retryable": false,
                        "live_version": live_version,
                        "expected_version": expected,
                        "page_id": id,
                    }),
                ));
            }
        }
        let current_title = current
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_owned();
        let status = current
            .get("status")
            .and_then(|s| s.as_str())
            .filter(|s| matches!(*s, "current" | "draft"))
            .unwrap_or("current")
            .to_owned();
        let (representation, value) = match new_body {
            Some(converted) => converted,
            None => (
                "storage".to_owned(),
                current
                    .get("body")
                    .and_then(|b| b.get("storage"))
                    .and_then(|s| s.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            ),
        };
        let next_version = live_version.saturating_add(1);
        let mut version = json!({ "number": next_version, "minorEdit": minor_edit });
        if let Some(message) = &version_message {
            version["message"] = json!(message);
        }
        let payload = json!({
            "id": id,
            "status": status,
            "title": title.clone().unwrap_or(current_title),
            "body": { "representation": representation, "value": value },
            "version": version,
        });

        // 2. Write.
        let url = client.v2_url(&format!("/pages/{id}"), &[]);
        let ctx = ErrorContext::new(format!("update page {id} (PUT /wiki/api/v2/pages/{id})"))
            .not_found(format!(
                "Page {id} disappeared between read and write, or the account lacks edit \
                 permission on it (Confluence folds that into 404)."
            ));
        match client.put(url, &payload).await {
            Ok(updated) => {
                let mut result = confluence::simplify_page(&updated, &client.config);
                result["updated"] = json!(true);
                result["previous_version"] = json!(live_version);
                result["representation"] = json!(representation);
                Ok(CallToolResult::structured(result))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Move a page to the space trash (DELETE /wiki/api/v2/pages/{id}); \
                       restorable in the UI, never purged. Requires confirm=true and the \
                       CONFLUENCE_ALLOW_DELETE=true named config (and not CONFLUENCE_READ_ONLY)."
    )]
    #[tracing::instrument(name = "tool.delete_page", skip(self))]
    async fn delete_page(
        &self,
        Parameters(params): Parameters<DeletePageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = write_gate("delete_page") {
            return Ok(refusal);
        }
        if !confluence::allow_delete() {
            return Ok(refuse(
                "delete_disabled",
                format!(
                    "delete_page is disabled (set {}=true in deploy/workload.yaml and re-apply). \
                     Nothing was sent to Confluence.",
                    confluence::ALLOW_DELETE_ENV
                ),
            ));
        }
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        if params.confirm != Some(true) {
            return Ok(refuse(
                "confirm_required",
                format!(
                    "delete_page moves page {id} to the trash; pass confirm=true to proceed. \
                     Nothing was sent to Confluence."
                ),
            ));
        }
        let client = match client("delete page") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let url = client.v2_url(&format!("/pages/{id}"), &[]);
        let ctx = ErrorContext::new(format!("delete page {id} (DELETE /wiki/api/v2/pages/{id})"))
            .not_found(format!(
                "No page with id {id} is visible to this account, it is already trashed, or the \
                 account lacks 'delete page' permission (Confluence folds that into 404)."
            ));
        match client.delete(url).await {
            Ok(reply) => Ok(CallToolResult::structured(json!({
                "deleted": true,
                "id": id,
                "status": reply.status,
                "note": "moved to the space trash; restore from Space settings → Content tools → Trash",
            }))),
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Comments on a page — footer (page-level, default) or inline — rendered to \
                       text with id, parentCommentId, version author/date, status and next_cursor \
                       (GET /wiki/api/v2/pages/{id}/footer-comments | inline-comments). Limit 1..250."
    )]
    #[tracing::instrument(name = "tool.get_comments", skip(self))]
    async fn get_comments(
        &self,
        Parameters(params): Parameters<GetCommentsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("get comments") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        let kind = match choice("kind", params.kind, COMMENT_KINDS, Some("footer")) {
            Ok(kind) => kind.unwrap_or_else(|| "footer".to_owned()),
            Err(result) => return Ok(result),
        };
        let limit = match limit(params.limit, DEFAULT_LIMIT, V2_MAX_LIMIT) {
            Ok(limit) => limit,
            Err(result) => return Ok(result),
        };
        let cursor = match cursor(params.cursor) {
            Ok(cursor) => cursor,
            Err(result) => return Ok(result),
        };
        let sort = match choice("sort", params.sort, COMMENT_SORTS, None) {
            Ok(sort) => sort,
            Err(result) => return Ok(result),
        };
        let mut query: Vec<(&str, String)> = vec![
            ("body-format", "storage".to_owned()),
            ("limit", limit.to_string()),
        ];
        if let Some(sort) = sort {
            query.push(("sort", sort));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let path = format!("/pages/{id}/{kind}-comments");
        let url = client.v2_url(&path, &query);
        let ctx = ErrorContext::new(format!("get {kind} comments of page {id}")).not_found(format!(
            "No page with id {id} is visible to this account; verify the id via search or list_pages."
        ));
        match client.get(url).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(|c| confluence::simplify_comment(c, &client.config, COMMENT_BODY_CHARS))
                    .collect();
                Ok(CallToolResult::structured(page_result(
                    results,
                    &raw,
                    limit,
                    extra(&[("pageId", json!(id)), ("kind", json!(kind))]),
                )))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Add a footer comment to a page, or reply to an existing footer comment \
                       (POST /wiki/api/v2/footer-comments). Body from markdown (default), storage \
                       or wiki. Returns the comment id, pageId, parentCommentId and version. \
                       Gated by CONFLUENCE_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.add_comment", skip(self))]
    async fn add_comment(
        &self,
        Parameters(params): Parameters<AddCommentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = write_gate("add_comment") {
            return Ok(refusal);
        }
        let client = match client("add comment") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let body_chars = params.body.chars().count();
        if params.body.trim().is_empty() || body_chars > MAX_COMMENT_CHARS {
            return Ok(refuse(
                "bad_body",
                format!("body must be 1..{MAX_COMMENT_CHARS} characters (got {body_chars})"),
            ));
        }
        let (representation, value) = match convert_body(&params.body, params.body_format) {
            Ok(converted) => converted,
            Err(result) => return Ok(result),
        };
        let reply_to = params
            .reply_to_comment_id
            .map(|r| r.trim().to_owned())
            .filter(|r| !r.is_empty());
        let mut payload = json!({
            "body": { "representation": representation, "value": value },
        });
        let target = match reply_to {
            Some(parent) => {
                if !confluence::is_numeric_id(&parent) {
                    return Ok(refuse(
                        "bad_comment_id",
                        format!(
                            "reply_to_comment_id {:?} is not a numeric comment id (get_comments \
                             lists them)",
                            parent.chars().take(40).collect::<String>()
                        ),
                    ));
                }
                payload["parentCommentId"] = json!(parent);
                format!("reply to comment {parent}")
            }
            None => {
                let Some(raw) = params
                    .page_id
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty())
                else {
                    return Ok(refuse(
                        "bad_page_id",
                        "pass page_id (the page to comment on) or reply_to_comment_id",
                    ));
                };
                let id = match page_id(&raw) {
                    Ok(id) => id,
                    Err(result) => return Ok(result),
                };
                payload["pageId"] = json!(id);
                format!("comment on page {id}")
            }
        };
        let url = client.v2_url("/footer-comments", &[]);
        let ctx = ErrorContext::new(format!("{target} (POST /wiki/api/v2/footer-comments)"))
            .not_found(
                "The page or parent comment is not visible to this account, or it lacks 'add \
                 comment' permission in that space (Confluence folds that into 404)."
                    .to_owned(),
            );
        match client.post(url, &payload).await {
            Ok(created) => {
                let mut result =
                    confluence::simplify_comment(&created, &client.config, COMMENT_BODY_CHARS);
                result["created"] = json!(true);
                Ok(CallToolResult::structured(result))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Labels on a page (GET /wiki/api/v2/pages/{id}/labels): id, name, prefix \
                       and next_cursor. Limit 1..250."
    )]
    #[tracing::instrument(name = "tool.get_labels", skip(self))]
    async fn get_labels(
        &self,
        Parameters(params): Parameters<GetLabelsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("get labels") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        let limit = match limit(params.limit, DEFAULT_LIMIT, V2_MAX_LIMIT) {
            Ok(limit) => limit,
            Err(result) => return Ok(result),
        };
        let cursor = match cursor(params.cursor) {
            Ok(cursor) => cursor,
            Err(result) => return Ok(result),
        };
        let prefix = match choice("prefix", params.prefix, LABEL_PREFIXES, None) {
            Ok(prefix) => prefix,
            Err(result) => return Ok(result),
        };
        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(prefix) = prefix {
            query.push(("prefix", prefix));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let url = client.v2_url(&format!("/pages/{id}/labels"), &query);
        let ctx = ErrorContext::new(format!("get labels of page {id}")).not_found(format!(
            "No page with id {id} is visible to this account; verify the id via search or list_pages."
        ));
        match client.get(url).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(confluence::simplify_label)
                    .collect();
                Ok(CallToolResult::structured(page_result(
                    results,
                    &raw,
                    limit,
                    extra(&[("pageId", json!(id))]),
                )))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Add one or more global labels to a page (POST /wiki/rest/api/content/{id}/label); \
                       existing labels are kept. Labels are lowercased single tokens without \
                       spaces or punctuation. Returns the resulting label list. Gated by \
                       CONFLUENCE_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.add_label", skip(self))]
    async fn add_label(
        &self,
        Parameters(params): Parameters<AddLabelParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = write_gate("add_label") {
            return Ok(refusal);
        }
        let client = match client("add label") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        if params.labels.is_empty() || params.labels.len() > MAX_LABELS {
            return Ok(refuse(
                "bad_labels",
                format!(
                    "labels must contain 1..{MAX_LABELS} names (got {})",
                    params.labels.len()
                ),
            ));
        }
        let mut names: Vec<String> = Vec::new();
        for raw in &params.labels {
            let name = raw.trim().to_lowercase();
            let count = name.chars().count();
            if count == 0 || count > MAX_LABEL_CHARS {
                return Ok(refuse(
                    "bad_labels",
                    format!("each label must be 1..{MAX_LABEL_CHARS} characters"),
                ));
            }
            if name.chars().any(char::is_whitespace) {
                return Ok(refuse(
                    "bad_labels",
                    format!(
                        "labels must not contain spaces ({name:?}); Confluence labels are single \
                         tokens — use hyphens or underscores (release-notes)"
                    ),
                ));
            }
            if name.chars().any(char::is_control) {
                return Ok(refuse(
                    "bad_labels",
                    format!("label {name:?} contains a control character"),
                ));
            }
            if let Some(bad) = name.chars().find(|c| LABEL_FORBIDDEN.contains(c)) {
                return Ok(refuse(
                    "bad_labels",
                    format!(
                        "label {name:?} contains {bad:?}, which Confluence does not allow in label \
                         names (no : ; , . ? & [ ] ( ) # ^ * @ ! quotes or slashes)"
                    ),
                ));
            }
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let payload = json!(names
            .iter()
            .map(|name| json!({ "prefix": "global", "name": name }))
            .collect::<Vec<_>>());
        let url = client.v1_url(&format!("/content/{id}/label"), &[]);
        let ctx = ErrorContext::new(format!(
            "add labels to page {id} (POST /wiki/rest/api/content/{id}/label)"
        ))
        .not_found(format!(
            "No content with id {id} is visible to this account, or it lacks edit permission on it."
        ))
        .bad_request(
            "Confluence rejected a label name; labels are lowercase single tokens without \
             punctuation such as : ; , . ? & ( ) [ ] # ^ * @ !"
                .to_owned(),
        );
        match client.post(url, &payload).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(confluence::simplify_label)
                    .collect();
                Ok(CallToolResult::structured(json!({
                    "pageId": id,
                    "added": names,
                    "count": results.len(),
                    "labels": results,
                })))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Attachments on a page (GET /wiki/api/v2/pages/{id}/attachments): id, \
                       filename, mediaType, fileSize, version, comment, url and an absolute \
                       download_url (fetch it with the same Basic credentials), plus next_cursor. \
                       Limit 1..250 (default 50)."
    )]
    #[tracing::instrument(name = "tool.list_attachments", skip(self))]
    async fn list_attachments(
        &self,
        Parameters(params): Parameters<ListAttachmentsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = match client("list attachments") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let id = match page_id(&params.page_id) {
            Ok(id) => id,
            Err(result) => return Ok(result),
        };
        let limit = match limit(params.limit, ATTACHMENTS_DEFAULT_LIMIT, V2_MAX_LIMIT) {
            Ok(limit) => limit,
            Err(result) => return Ok(result),
        };
        let cursor = match cursor(params.cursor) {
            Ok(cursor) => cursor,
            Err(result) => return Ok(result),
        };
        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(media_type) = params
            .media_type
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty())
        {
            if media_type.chars().count() > 255 {
                return Ok(refuse(
                    "bad_option",
                    "media_type is longer than 255 characters",
                ));
            }
            query.push(("mediaType", media_type));
        }
        if let Some(filename) = params
            .filename
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty())
        {
            if filename.chars().count() > 255 {
                return Ok(refuse(
                    "bad_option",
                    "filename is longer than 255 characters",
                ));
            }
            query.push(("filename", filename));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let url = client.v2_url(&format!("/pages/{id}/attachments"), &query);
        let ctx = ErrorContext::new(format!("list attachments of page {id}")).not_found(format!(
            "No page with id {id} is visible to this account; verify the id via search or list_pages."
        ));
        match client.get(url).await {
            Ok(raw) => {
                let results: Vec<Value> = results_array(&raw)
                    .iter()
                    .map(|a| confluence::simplify_attachment(a, &client.config))
                    .collect();
                Ok(CallToolResult::structured(page_result(
                    results,
                    &raw,
                    limit,
                    extra(&[("pageId", json!(id))]),
                )))
            }
            Err(err) => Ok(confluence_error(&err, &ctx, Some(&client.config.site))),
        }
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
            "Confluence Cloud MCP server (REST API v2 + v1 search) running as a sandboxed \
             WebAssembly component on Cosmonic Desktop. Start with `check_auth` to validate the \
             API token; then `list_spaces` (key → numeric spaceId), `search` (CQL) or \
             `list_pages` (space + exact title → page id), and `get_page` to read. Writes \
             (`create_page`, `update_page`, `add_comment`, `add_label`) take markdown by default \
             and are gated by CONFLUENCE_READ_ONLY; `delete_page` additionally needs \
             CONFLUENCE_ALLOW_DELETE=true and confirm=true and only trashes. Everything in v2 \
             is addressed by numeric ids, pagination is cursor-based (pass next_cursor \
             verbatim), and update_page is optimistic-locked.\n\n\
             This server publishes skills — playbooks describing when and how to use its \
             tools, the error catalogue, CQL syntax and body-format rules. Read \
             `skill://index.json` for the catalog, then `skill://atlassian-confluence-mcp/SKILL.md`.",
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
