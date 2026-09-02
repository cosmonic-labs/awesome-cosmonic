//! The MCP server: Jira Cloud tools over the REST API v3.
//!
//! Tool definitions and result rendering live here; the HTTP client, error
//! mapping and ADF handling live in [`crate::jira`]. Alongside the tools,
//! this server publishes **skills** — natural-language playbooks served over
//! the MCP resources primitive under `skill://` URIs (see [`crate::skills`]).

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

use crate::jira::{self, Client, Error, ErrorContext};
use crate::skills;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so no per-session state
/// lives here; everything durable is in Jira.
#[derive(Clone)]
pub struct JiraServer {
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Fields returned by `search_issues` when the caller names none.
const DEFAULT_SEARCH_FIELDS: &[&str] = &[
    "summary",
    "status",
    "assignee",
    "priority",
    "issuetype",
    "created",
    "updated",
];
/// Fields returned by `get_issue` when the caller names none: everything
/// navigable minus the three that make issues huge.
const DEFAULT_ISSUE_FIELDS: &[&str] = &["*navigable", "-comment", "-attachment", "-worklog"];
const MAX_JQL_CHARS: usize = 10_000;
const MAX_FIELDS: usize = 100;
const MAX_FIELD_NAME_CHARS: usize = 255;
const MAX_EXPAND_CHARS: usize = 256;
const MAX_TOKEN_CHARS: usize = 4096;
const MAX_SUMMARY_CHARS: usize = 255;
const MAX_RICH_TEXT_CHARS: usize = 32_767;
const MAX_LABELS: usize = 100;
const MAX_LABEL_CHARS: usize = 255;
const MAX_EXTRA_FIELDS_BYTES: usize = 65_536;
const MAX_QUERY_CHARS: usize = 512;
const MAX_VISIBILITY_CHARS: usize = 255;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchIssuesParams {
    /// JQL query. Must be bounded — contain at least one restriction such as
    /// `project = PROJ`, `assignee = currentUser()` or `updated >= -30d`
    /// (Jira rejects an ORDER BY-only query). Quote values with spaces.
    pub jql: String,
    /// Fields to return per issue. Default: summary, status, assignee,
    /// priority, issuetype, created, updated. Add `description` for the
    /// description rendered as text; `*all`, `*navigable` and `-field`
    /// (exclude) pass through to Jira. Keep it minimal — page size and rate
    /// budget both shrink with field count.
    pub fields: Option<Vec<String>>,
    /// Page size, clamped to 1..100 (default 25).
    pub max_results: Option<i64>,
    /// Cursor from a previous result's `next_page_token`; omit for page 1.
    pub next_page_token: Option<String>,
    /// Comma-separated expansions: `renderedFields`, `names`, `changelog`.
    pub expand: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CountIssuesParams {
    /// Bounded JQL query (same rules as `search_issues`).
    pub jql: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetIssueParams {
    /// Issue key such as `PROJ-123`, or the numeric issue id.
    pub issue_key: String,
    /// Fields to return. Default: every navigable field except comment,
    /// attachment and worklog. Use `["*all"]` for everything (large).
    pub fields: Option<Vec<String>>,
    /// Comma-separated expansions: `renderedFields` (HTML), `changelog`,
    /// `transitions`, `names`, `editmeta`.
    pub expand: Option<String>,
    /// Include the comment thread (rendered to text). Default false.
    pub include_comments: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateIssueParams {
    /// Project key (case-sensitive), e.g. `PROJ`.
    pub project_key: String,
    /// Issue type name (`Task`, `Bug`, `Story`, `Sub-task`…) or numeric id.
    /// Names are case-sensitive; `list_issue_types` shows what the project offers.
    pub issue_type: String,
    /// One-line summary, at most 255 characters.
    pub summary: String,
    /// Plain-text description; blank lines separate paragraphs. Converted to
    /// an Atlassian Document (ADF) for you.
    pub description: Option<String>,
    /// Priority name exactly as the project defines it (`High`, `Medium`…).
    pub priority: Option<String>,
    /// Labels (no spaces allowed by Jira).
    pub labels: Option<Vec<String>>,
    /// Assignee's accountId (from `search_users` or `get_myself`).
    pub assignee_account_id: Option<String>,
    /// Parent issue key — for sub-tasks, and for epic children in
    /// team-managed projects.
    pub parent_key: Option<String>,
    /// Additional raw `fields` entries merged last, e.g.
    /// `{"customfield_10016": 5, "components": [{"name": "API"}]}`.
    /// May not override `project` or `summary`.
    pub extra_fields: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateIssueParams {
    /// Issue key such as `PROJ-123`, or the numeric issue id.
    pub issue_key: String,
    /// New summary (at most 255 characters).
    pub summary: Option<String>,
    /// New description as plain text (replaces the whole description).
    pub description: Option<String>,
    /// New priority name.
    pub priority: Option<String>,
    /// Replace all labels with this list.
    pub labels: Option<Vec<String>>,
    /// Labels to add (kept alongside existing ones).
    pub add_labels: Option<Vec<String>>,
    /// Labels to remove.
    pub remove_labels: Option<Vec<String>>,
    /// New assignee accountId; an empty string unassigns.
    pub assignee_account_id: Option<String>,
    /// Additional raw `fields` entries merged last (custom fields etc.).
    /// May not set `project`.
    pub extra_fields: Option<Map<String, Value>>,
    /// Send Jira notifications for this edit (default true).
    pub notify_users: Option<bool>,
    /// Return the updated issue instead of just `ok` (default false).
    pub return_issue: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddCommentParams {
    /// Issue key such as `PROJ-123`, or the numeric issue id.
    pub issue_key: String,
    /// Comment text (plain text; blank lines separate paragraphs). Converted
    /// to ADF. At most 32767 characters.
    pub body: String,
    /// Restrict visibility: `role` or `group` (requires `visibility_value`).
    pub visibility_type: Option<String>,
    /// The role name or group name the comment is restricted to.
    pub visibility_value: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetCommentsParams {
    /// Issue key such as `PROJ-123`, or the numeric issue id.
    pub issue_key: String,
    /// Offset of the first comment (default 0).
    pub start_at: Option<i64>,
    /// Page size, clamped to 1..100 (default 50).
    pub max_results: Option<i64>,
    /// `created` (oldest first) or `-created` (newest first, the default).
    pub order_by: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTransitionsParams {
    /// Issue key such as `PROJ-123`, or the numeric issue id.
    pub issue_key: String,
    /// Also list each transition's screen fields and which are required
    /// (default false).
    pub include_fields: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TransitionIssueParams {
    /// Issue key such as `PROJ-123`, or the numeric issue id.
    pub issue_key: String,
    /// Transition id (`"31"`) or a transition / target-status name
    /// (`"Done"`, `"In Progress"`), matched case-insensitively against what
    /// `get_transitions` lists.
    pub transition: String,
    /// Optional plain-text comment added with the transition.
    pub comment: Option<String>,
    /// Screen fields the transition requires, e.g.
    /// `{"resolution": {"name": "Done"}}`.
    pub fields: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AssignIssueParams {
    /// Issue key such as `PROJ-123`, or the numeric issue id.
    pub issue_key: String,
    /// accountId to assign; omit or pass an empty string to unassign, or
    /// `"-1"` for the project's default assignee.
    pub account_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListProjectsParams {
    /// Filter by a substring of the project key or name.
    pub query: Option<String>,
    /// Offset of the first project (default 0).
    pub start_at: Option<i64>,
    /// Page size, clamped to 1..100 (default 50).
    pub max_results: Option<i64>,
    /// Restrict to a project type: `software`, `business` or `service_desk`.
    pub type_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetProjectParams {
    /// Project key (case-sensitive) or numeric id.
    pub project_key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListIssueTypesParams {
    /// Project key (case-sensitive) or numeric id.
    pub project_key: String,
    /// Page size, clamped to 1..200 (default 50).
    pub max_results: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetCreateFieldsParams {
    /// Project key (case-sensitive) or numeric id.
    pub project_key: String,
    /// Issue type id, or a name resolved via the project's issue types.
    pub issue_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchUsersParams {
    /// Display name or email prefix to match (at least one character).
    pub query: String,
    /// Page size, clamped to 1..50 (default 20).
    pub max_results: Option<i64>,
    /// Only users assignable to issues in this project key.
    pub assignable_to_project: Option<String>,
    /// Only users assignable to this issue key. Mutually exclusive with
    /// `assignable_to_project`.
    pub assignable_to_issue: Option<String>,
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

/// A tool-level error with a readable message and a structured `error`
/// object (the caller sees both).
fn structured_error(message: String, detail: Value) -> CallToolResult {
    let mut result = CallToolResult::error(vec![ContentBlock::text(message.clone())]);
    result.structured_content = Some(json!({ "error": detail, "message": message }));
    result
}

/// An input problem the tool detected before dialing Jira.
fn input_error(message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    structured_error(
        message.clone(),
        json!({ "kind": "invalid_input", "retryable": false, "hint": message }),
    )
}

/// A [`jira::Error`] phrased for the caller.
fn jira_error(err: &Error, ctx: &ErrorContext, site: Option<&str>) -> CallToolResult {
    let (message, detail) = jira::describe(err, ctx, site);
    structured_error(message, detail)
}

/// Builds the client or returns the actionable not-configured error.
fn client(operation: &str) -> Result<Client, CallToolResult> {
    Client::from_env().map_err(|err| {
        let site = jira::env_setting(jira::SITE_ENV);
        jira_error(&err, &ErrorContext::new(operation), site.as_deref())
    })
}

/// The `JIRA_READ_ONLY` gate for write tools.
fn refuse_if_read_only(tool: &str) -> Option<CallToolResult> {
    jira::read_only().then(|| {
        structured_error(
            format!(
                "{tool} refused: this server runs with {}=true (read-only mode). Read tools still \
                 work; to allow writes set {}=false in the workload's named config and redeploy.",
                jira::READ_ONLY_ENV,
                jira::READ_ONLY_ENV
            ),
            json!({ "kind": "read_only", "retryable": false, "tool": tool }),
        )
    })
}

/// Clamps an optional client integer into the upstream's documented range.
fn clamp(value: Option<i64>, min: i64, max: i64, default: i64) -> i64 {
    value.unwrap_or(default).clamp(min, max)
}

fn validate_issue_key(key: &str) -> Result<String, CallToolResult> {
    let key = key.trim().to_owned();
    if jira::is_issue_key(&key) {
        Ok(key)
    } else {
        Err(input_error(format!(
            "issue_key {:?} is not an issue key (PROJ-123) or numeric issue id",
            jira::bound_chars(key, 80)
        )))
    }
}

fn validate_project_key(key: &str) -> Result<String, CallToolResult> {
    let key = key.trim().to_owned();
    if jira::is_project_key(&key) || jira::is_numeric_id(&key) {
        Ok(key)
    } else {
        Err(input_error(format!(
            "project_key {:?} is not a project key (letters, digits, underscore; e.g. PROJ) or numeric id",
            jira::bound_chars(key, 80)
        )))
    }
}

fn validate_jql(jql: &str) -> Result<String, CallToolResult> {
    let jql = jql.trim();
    if jql.is_empty() {
        return Err(input_error(
            "jql must not be empty; give a bounded query such as `project = PROJ AND updated >= -7d`",
        ));
    }
    if jql.chars().count() > MAX_JQL_CHARS {
        return Err(input_error(format!(
            "jql is longer than {MAX_JQL_CHARS} characters"
        )));
    }
    if jira::has_control_chars(jql) {
        return Err(input_error("jql contains control characters"));
    }
    Ok(jql.replace(['\n', '\r', '\t'], " "))
}

fn validate_fields(
    fields: Option<Vec<String>>,
    default: &[&str],
) -> Result<Vec<String>, CallToolResult> {
    let Some(fields) = fields else {
        return Ok(default.iter().map(|f| (*f).to_owned()).collect());
    };
    let mut out = Vec::new();
    for field in fields {
        let field = field.trim().to_owned();
        if field.is_empty() {
            continue;
        }
        if field.chars().count() > MAX_FIELD_NAME_CHARS || jira::has_control_chars(&field) {
            return Err(input_error(format!(
                "field name {:?} is invalid",
                jira::bound_chars(field, 80)
            )));
        }
        if !out.contains(&field) {
            out.push(field);
        }
        if out.len() > MAX_FIELDS {
            return Err(input_error(format!(
                "at most {MAX_FIELDS} fields per request"
            )));
        }
    }
    if out.is_empty() {
        out = default.iter().map(|f| (*f).to_owned()).collect();
    }
    Ok(out)
}

fn validate_expand(expand: Option<String>) -> Result<Option<String>, CallToolResult> {
    let Some(expand) = expand else {
        return Ok(None);
    };
    let expand = expand.trim().to_owned();
    if expand.is_empty() {
        return Ok(None);
    }
    if expand.chars().count() > MAX_EXPAND_CHARS
        || !expand
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ',' | '.' | '_' | ' '))
    {
        return Err(input_error(
            "expand must be a comma-separated list such as renderedFields,changelog",
        ));
    }
    Ok(Some(expand.replace(' ', "")))
}

fn validate_summary(summary: &str) -> Result<String, CallToolResult> {
    let summary = summary.trim().replace(['\n', '\r'], " ");
    if summary.is_empty() {
        return Err(input_error("summary must not be empty"));
    }
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        return Err(input_error(format!(
            "summary is longer than {MAX_SUMMARY_CHARS} characters ({} given); move detail into the description",
            summary.chars().count()
        )));
    }
    Ok(summary)
}

fn validate_rich_text(name: &str, text: &str) -> Result<String, CallToolResult> {
    let count = text.chars().count();
    if count > MAX_RICH_TEXT_CHARS {
        return Err(input_error(format!(
            "{name} is longer than {MAX_RICH_TEXT_CHARS} characters ({count} given)"
        )));
    }
    if jira::has_control_chars(text) {
        return Err(input_error(format!("{name} contains control characters")));
    }
    Ok(text.to_owned())
}

fn validate_labels(name: &str, labels: Vec<String>) -> Result<Vec<String>, CallToolResult> {
    if labels.len() > MAX_LABELS {
        return Err(input_error(format!("{name}: at most {MAX_LABELS} labels")));
    }
    let mut out = Vec::new();
    for label in labels {
        let label = label.trim().to_owned();
        if label.is_empty() {
            continue;
        }
        if label.chars().any(char::is_whitespace) || label.chars().count() > MAX_LABEL_CHARS {
            return Err(input_error(format!(
                "{name}: label {:?} is invalid — Jira labels cannot contain spaces (use hyphens) and \
                 are at most {MAX_LABEL_CHARS} characters",
                jira::bound_chars(label, 80)
            )));
        }
        if !out.contains(&label) {
            out.push(label);
        }
    }
    Ok(out)
}

fn validate_account_id(name: &str, id: &str) -> Result<String, CallToolResult> {
    let id = id.trim().to_owned();
    if jira::is_account_id(&id) {
        Ok(id)
    } else {
        Err(input_error(format!(
            "{name} {:?} is not an Atlassian accountId; look it up with search_users or get_myself",
            jira::bound_chars(id, 80)
        )))
    }
}

fn validate_extra_fields(
    extra: Option<Map<String, Value>>,
    reserved: &[&str],
) -> Result<Map<String, Value>, CallToolResult> {
    let Some(extra) = extra else {
        return Ok(Map::new());
    };
    for key in reserved {
        if extra.contains_key(*key) {
            return Err(input_error(format!(
                "extra_fields may not set `{key}`; use the dedicated parameter"
            )));
        }
    }
    let size = serde_json::to_vec(&extra)
        .map(|v| v.len())
        .unwrap_or(usize::MAX);
    if size > MAX_EXTRA_FIELDS_BYTES {
        return Err(input_error(format!(
            "extra_fields is larger than {MAX_EXTRA_FIELDS_BYTES} bytes"
        )));
    }
    Ok(extra)
}

fn validate_query(name: &str, query: &str, allow_empty: bool) -> Result<String, CallToolResult> {
    let query = query.trim().to_owned();
    if query.is_empty() && !allow_empty {
        return Err(input_error(format!("{name} must not be empty")));
    }
    if query.chars().count() > MAX_QUERY_CHARS || jira::has_control_chars(&query) {
        return Err(input_error(format!(
            "{name} is invalid (at most {MAX_QUERY_CHARS} characters, no control characters)"
        )));
    }
    Ok(query)
}

/// Issue type reference for a create body: `{id}` for numeric input,
/// `{name}` otherwise.
fn issue_type_ref(issue_type: &str) -> Result<Value, CallToolResult> {
    let issue_type = issue_type.trim();
    if issue_type.is_empty()
        || issue_type.chars().count() > 255
        || jira::has_control_chars(issue_type)
    {
        return Err(input_error(
            "issue_type must be a type name such as Task or a numeric id",
        ));
    }
    Ok(if jira::is_numeric_id(issue_type) {
        json!({ "id": issue_type })
    } else {
        json!({ "name": issue_type })
    })
}

fn issue_not_found(key: &str) -> String {
    format!(
        "Issue {key} does not exist or you do not have permission to see it (Jira does not \
         distinguish the two). Check the key's project prefix (case-sensitive) or search with \
         `key = {key}`."
    )
}

fn project_not_found(key: &str) -> String {
    format!(
        "Project {key} was not found or is not browsable by this account. Keys are case-sensitive; \
         use list_projects (with `query`) to find the exact key."
    )
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl JiraServer {
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
        description = "Verify the Jira credentials and report status ok|missing|invalid|insufficient \
                       with the identity, the route in use, and remediation. Call this first; never \
                       retry a missing/invalid result."
    )]
    #[tracing::instrument(name = "tool.check_auth", skip(self))]
    async fn check_auth(&self) -> Result<CallToolResult, ErrorData> {
        let site = jira::env_setting(jira::SITE_ENV);
        let client = match Client::from_env() {
            Ok(client) => client,
            Err(err) => {
                let (message, detail) = jira::describe(
                    &err,
                    &ErrorContext::new("check credentials"),
                    site.as_deref(),
                );
                let status = match err {
                    Error::NotConfigured { .. } => "missing",
                    _ => "error",
                };
                let mut result = CallToolResult::error(vec![ContentBlock::text(message.clone())]);
                result.structured_content = Some(json!({
                    "status": status,
                    "auth": "basic",
                    "credential": { "ref": jira::SECRET_REF, "env": jira::TOKEN_ENV, "obtainUrl": jira::TOKEN_URL },
                    "error": detail,
                    "remediation": jira::remediation(site.as_deref()),
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
            "read_only": jira::read_only(),
            "projects_filter": config.projects_filter,
            "credential": { "ref": jira::SECRET_REF, "env": jira::TOKEN_ENV, "obtainUrl": jira::TOKEN_URL, "scopes": jira::SCOPES },
        });
        match client.get("/myself", &[]).await {
            Ok(me) => {
                let mut value = base;
                value["status"] = json!("ok");
                value["account"] = jira::simplify_user(&me);
                value["remediation"] = Value::Null;
                Ok(CallToolResult::structured(value))
            }
            Err(err) => {
                let ctx = ErrorContext::new("check credentials (GET /myself)");
                let (message, detail) = jira::describe(&err, &ctx, Some(&config.site));
                let status = match err.status() {
                    Some(401) => "invalid",
                    Some(403) => "insufficient",
                    _ => "error",
                };
                let mut value = base;
                value["status"] = json!(status);
                value["error"] = detail;
                value["remediation"] = json!(jira::remediation(Some(&config.site)));
                let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
                result.structured_content = Some(value);
                Ok(result)
            }
        }
    }

    #[tool(
        description = "Identity of the authenticated account: accountId (what currentUser() and \
                       assign_issue use), displayName, email (may be hidden), timeZone, active."
    )]
    #[tracing::instrument(name = "tool.get_myself", skip(self))]
    async fn get_myself(&self) -> Result<CallToolResult, ErrorData> {
        let client = match client("get the current user") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        match client.get("/myself", &[]).await {
            Ok(me) => {
                let mut value = jira::simplify_user(&me);
                if let Some(locale) = me.get("locale") {
                    value["locale"] = locale.clone();
                }
                value["site"] = json!(client.config.site);
                value["route"] = json!(client.config.route());
                Ok(CallToolResult::structured(value))
            }
            Err(err) => Ok(jira_error(
                &err,
                &ErrorContext::new("get the current user (GET /myself)"),
                Some(&client.config.site),
            )),
        }
    }

    #[tool(
        description = "Search issues with a bounded JQL query (POST /search/jql). Returns compact \
                       issues (ADF rendered to text), next_page_token for the next page, and is_last. \
                       No total — use count_issues."
    )]
    #[tracing::instrument(name = "tool.search_issues", skip(self))]
    async fn search_issues(
        &self,
        Parameters(params): Parameters<SearchIssuesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let jql = match validate_jql(&params.jql) {
            Ok(jql) => jql,
            Err(result) => return Ok(result),
        };
        let fields = match validate_fields(params.fields, DEFAULT_SEARCH_FIELDS) {
            Ok(fields) => fields,
            Err(result) => return Ok(result),
        };
        let expand = match validate_expand(params.expand) {
            Ok(expand) => expand,
            Err(result) => return Ok(result),
        };
        let token = params
            .next_page_token
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        if token
            .as_ref()
            .is_some_and(|t| t.chars().count() > MAX_TOKEN_CHARS || jira::has_control_chars(t))
        {
            return Ok(input_error(
                "next_page_token is not a token this server issued",
            ));
        }
        let max_results = clamp(params.max_results, 1, 100, 25);
        let client = match client("search issues") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let effective_jql = jira::apply_projects_filter(&jql, &client.config.projects_filter);
        let mut body = json!({
            "jql": effective_jql,
            "fields": fields,
            "maxResults": max_results,
        });
        if let Some(token) = &token {
            body["nextPageToken"] = json!(token);
        }
        if let Some(expand) = &expand {
            body["expand"] = json!(expand);
        }
        let ctx = ErrorContext::new("search issues (POST /search/jql)").bad_request(
            "Fix the JQL: Jira's message names the bad field or clause. The query must be bounded \
             (add `project = X` or `updated >= -30d`), ORDER BY may name at most 7 fields, and \
             field names must exist on this site.",
        );
        match client.post("/search/jql", &body).await {
            Ok(page) => {
                let issues: Vec<Value> = page
                    .get("issues")
                    .and_then(Value::as_array)
                    .map(|list| list.iter().take(1000).map(jira::simplify_issue).collect())
                    .unwrap_or_default();
                let next = page
                    .get("nextPageToken")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty());
                let is_last = page
                    .get("isLast")
                    .and_then(Value::as_bool)
                    .unwrap_or(next.is_none());
                let mut value = json!({
                    "jql": effective_jql,
                    "fields": fields,
                    "max_results": max_results,
                    "count": issues.len(),
                    "issues": issues,
                    "is_last": is_last,
                });
                if let Some(next) = next {
                    value["next_page_token"] = json!(next);
                }
                if let Some(names) = page.get("names") {
                    value["names"] = names.clone();
                }
                Ok(CallToolResult::structured(value))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Approximate number of issues matching a bounded JQL query (the search \
                       endpoint returns no total)."
    )]
    #[tracing::instrument(name = "tool.count_issues", skip(self))]
    async fn count_issues(
        &self,
        Parameters(params): Parameters<CountIssuesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let jql = match validate_jql(&params.jql) {
            Ok(jql) => jql,
            Err(result) => return Ok(result),
        };
        let client = match client("count issues") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let effective_jql = jira::apply_projects_filter(&jql, &client.config.projects_filter);
        let ctx = ErrorContext::new("count issues (POST /search/approximate-count)").bad_request(
            "Fix the JQL: Jira's message names the bad field or clause; the query must be bounded.",
        );
        match client
            .post(
                "/search/approximate-count",
                &json!({ "jql": effective_jql }),
            )
            .await
        {
            Ok(value) => Ok(CallToolResult::structured(json!({
                "jql": effective_jql,
                "count": value.get("count").cloned().unwrap_or(Value::Null),
                "approximate": true,
            }))),
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Fetch one issue by key or id. Description (and comments when requested) are \
                       rendered from ADF to text; users and statuses are compacted."
    )]
    #[tracing::instrument(name = "tool.get_issue", skip(self))]
    async fn get_issue(
        &self,
        Parameters(params): Parameters<GetIssueParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = match validate_issue_key(&params.issue_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let mut fields = match validate_fields(params.fields, DEFAULT_ISSUE_FIELDS) {
            Ok(fields) => fields,
            Err(result) => return Ok(result),
        };
        if params.include_comments.unwrap_or(false) {
            fields.retain(|f| f != "-comment");
            if !fields.iter().any(|f| f == "comment" || f == "*all") {
                fields.push("comment".to_owned());
            }
        }
        let expand = match validate_expand(params.expand) {
            Ok(expand) => expand,
            Err(result) => return Ok(result),
        };
        let client = match client("get an issue") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let mut query = vec![("fields", fields.join(","))];
        if let Some(expand) = expand {
            query.push(("expand", expand));
        }
        let ctx = ErrorContext::new(format!("get issue {key} (GET /issue/{key})"))
            .not_found(issue_not_found(&key));
        match client.get(&format!("/issue/{key}"), &query).await {
            Ok(issue) => {
                let mut value = jira::simplify_issue(&issue);
                value["url"] = json!(client
                    .config
                    .browse_url(issue.get("key").and_then(Value::as_str).unwrap_or(&key)));
                Ok(CallToolResult::structured(value))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Create an issue (gated by JIRA_READ_ONLY). Plain-text description becomes \
                       an ADF document. Returns id, key and URL. Use get_create_fields first when \
                       unsure which fields the project requires."
    )]
    #[tracing::instrument(name = "tool.create_issue", skip(self))]
    async fn create_issue(
        &self,
        Parameters(params): Parameters<CreateIssueParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = refuse_if_read_only("create_issue") {
            return Ok(refusal);
        }
        let project = match validate_project_key(&params.project_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let issue_type = match issue_type_ref(&params.issue_type) {
            Ok(value) => value,
            Err(result) => return Ok(result),
        };
        let summary = match validate_summary(&params.summary) {
            Ok(summary) => summary,
            Err(result) => return Ok(result),
        };
        let mut fields = Map::new();
        fields.insert(
            "project".to_owned(),
            if jira::is_numeric_id(&project) {
                json!({ "id": project })
            } else {
                json!({ "key": project })
            },
        );
        fields.insert("issuetype".to_owned(), issue_type);
        fields.insert("summary".to_owned(), json!(summary));
        if let Some(description) = params
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            match validate_rich_text("description", description) {
                Ok(text) => {
                    fields.insert("description".to_owned(), jira::text_to_adf(&text));
                }
                Err(result) => return Ok(result),
            }
        }
        if let Some(priority) = params
            .priority
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            if priority.chars().count() > 255 || jira::has_control_chars(priority) {
                return Ok(input_error("priority must be a priority name such as High"));
            }
            fields.insert("priority".to_owned(), json!({ "name": priority }));
        }
        if let Some(labels) = params.labels {
            match validate_labels("labels", labels) {
                Ok(labels) => {
                    fields.insert("labels".to_owned(), json!(labels));
                }
                Err(result) => return Ok(result),
            }
        }
        if let Some(assignee) = params
            .assignee_account_id
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            match validate_account_id("assignee_account_id", assignee) {
                Ok(id) => {
                    fields.insert("assignee".to_owned(), json!({ "id": id }));
                }
                Err(result) => return Ok(result),
            }
        }
        if let Some(parent) = params
            .parent_key
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            match validate_issue_key(parent) {
                Ok(parent) => {
                    fields.insert(
                        "parent".to_owned(),
                        if jira::is_numeric_id(&parent) {
                            json!({ "id": parent })
                        } else {
                            json!({ "key": parent })
                        },
                    );
                }
                Err(_) => {
                    return Ok(input_error(
                        "parent_key must be an issue key such as PROJ-10",
                    ));
                }
            }
        }
        match validate_extra_fields(params.extra_fields, &["project", "summary"]) {
            Ok(extra) => fields.extend(extra),
            Err(result) => return Ok(result),
        }
        let client = match client("create an issue") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let ctx = ErrorContext::new(format!("create an issue in {project} (POST /issue)"))
            .bad_request(
                "Jira's per-field errors say which field is missing, not on the create screen, or \
                 has an invalid value. Call get_create_fields for the exact required fields, allowed \
                 values (priorities, components) and custom field ids; project keys and issue type \
                 names are case-sensitive; labels cannot contain spaces.",
            )
            .not_found(project_not_found(&project));
        match client.post("/issue", &json!({ "fields": fields })).await {
            Ok(created) => {
                let key = created
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Ok(CallToolResult::structured(json!({
                    "ok": true,
                    "id": created.get("id").cloned().unwrap_or(Value::Null),
                    "key": key,
                    "url": client.config.browse_url(&key),
                })))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Edit an issue (gated by JIRA_READ_ONLY): summary, description (plain text → \
                       ADF), priority, labels (replace/add/remove), assignee, custom fields. At \
                       least one change is required."
    )]
    #[tracing::instrument(name = "tool.update_issue", skip(self))]
    async fn update_issue(
        &self,
        Parameters(params): Parameters<UpdateIssueParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = refuse_if_read_only("update_issue") {
            return Ok(refusal);
        }
        let key = match validate_issue_key(&params.issue_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let mut fields = Map::new();
        let mut update = Map::new();
        if let Some(summary) = params.summary.as_deref() {
            match validate_summary(summary) {
                Ok(summary) => {
                    fields.insert("summary".to_owned(), json!(summary));
                }
                Err(result) => return Ok(result),
            }
        }
        if let Some(description) = params.description.as_deref() {
            match validate_rich_text("description", description) {
                Ok(text) => {
                    fields.insert("description".to_owned(), jira::text_to_adf(&text));
                }
                Err(result) => return Ok(result),
            }
        }
        if let Some(priority) = params.priority.as_deref().map(str::trim) {
            if priority.is_empty()
                || priority.chars().count() > 255
                || jira::has_control_chars(priority)
            {
                return Ok(input_error("priority must be a priority name such as High"));
            }
            fields.insert("priority".to_owned(), json!({ "name": priority }));
        }
        if let Some(labels) = params.labels {
            match validate_labels("labels", labels) {
                Ok(labels) => {
                    fields.insert("labels".to_owned(), json!(labels));
                }
                Err(result) => return Ok(result),
            }
        }
        let mut label_ops = Vec::new();
        if let Some(add) = params.add_labels {
            match validate_labels("add_labels", add) {
                Ok(add) => label_ops.extend(add.into_iter().map(|l| json!({ "add": l }))),
                Err(result) => return Ok(result),
            }
        }
        if let Some(remove) = params.remove_labels {
            match validate_labels("remove_labels", remove) {
                Ok(remove) => label_ops.extend(remove.into_iter().map(|l| json!({ "remove": l }))),
                Err(result) => return Ok(result),
            }
        }
        if !label_ops.is_empty() {
            if fields.contains_key("labels") {
                return Ok(input_error(
                    "use either labels (replace) or add_labels/remove_labels, not both",
                ));
            }
            update.insert("labels".to_owned(), Value::Array(label_ops));
        }
        if let Some(assignee) = params.assignee_account_id.as_deref() {
            if assignee.trim().is_empty() {
                fields.insert("assignee".to_owned(), Value::Null);
            } else {
                match validate_account_id("assignee_account_id", assignee) {
                    Ok(id) => {
                        fields.insert("assignee".to_owned(), json!({ "id": id }));
                    }
                    Err(result) => return Ok(result),
                }
            }
        }
        match validate_extra_fields(params.extra_fields, &["project"]) {
            Ok(extra) => fields.extend(extra),
            Err(result) => return Ok(result),
        }
        if fields.is_empty() && update.is_empty() {
            return Ok(input_error(
                "nothing to update: pass at least one of summary, description, priority, labels, \
                 add_labels, remove_labels, assignee_account_id or extra_fields",
            ));
        }
        let client = match client("update an issue") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let return_issue = params.return_issue.unwrap_or(false);
        let query = vec![
            (
                "notifyUsers",
                params.notify_users.unwrap_or(true).to_string(),
            ),
            ("returnIssue", return_issue.to_string()),
        ];
        let mut body = json!({ "fields": fields });
        if !update.is_empty() {
            body["update"] = Value::Object(update);
        }
        let ctx = ErrorContext::new(format!("update issue {key} (PUT /issue/{key})"))
            .not_found(issue_not_found(&key))
            .bad_request(
                "Jira's per-field errors name the field that is not on the edit screen, unknown, \
                 or has an invalid value. Check names/ids with get_issue (expand=editmeta) or \
                 get_create_fields; use customfield_NNNNN ids for custom fields.",
            );
        match client.put(&format!("/issue/{key}"), &query, &body).await {
            Ok(Value::Null) => Ok(CallToolResult::structured(json!({
                "ok": true,
                "issue_key": key,
                "url": client.config.browse_url(&key),
            }))),
            Ok(issue) => {
                let mut value = jira::simplify_issue(&issue);
                value["ok"] = json!(true);
                value["url"] = json!(client.config.browse_url(&key));
                Ok(CallToolResult::structured(value))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Add a comment to an issue (gated by JIRA_READ_ONLY). Plain text becomes ADF; \
                       optionally restrict visibility to a role or group."
    )]
    #[tracing::instrument(name = "tool.add_comment", skip(self))]
    async fn add_comment(
        &self,
        Parameters(params): Parameters<AddCommentParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = refuse_if_read_only("add_comment") {
            return Ok(refusal);
        }
        let key = match validate_issue_key(&params.issue_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        if params.body.trim().is_empty() {
            return Ok(input_error("body must not be empty"));
        }
        let text = match validate_rich_text("body", &params.body) {
            Ok(text) => text,
            Err(result) => return Ok(result),
        };
        let mut body = json!({ "body": jira::text_to_adf(&text) });
        match (
            params.visibility_type.as_deref().map(str::trim),
            params.visibility_value.as_deref().map(str::trim),
        ) {
            (None, None) | (Some(""), _) | (None, Some("")) => {}
            (Some(kind), Some(value)) if !value.is_empty() => {
                if !matches!(kind, "role" | "group") {
                    return Ok(input_error("visibility_type must be `role` or `group`"));
                }
                if value.chars().count() > MAX_VISIBILITY_CHARS || jira::has_control_chars(value) {
                    return Ok(input_error("visibility_value is invalid"));
                }
                body["visibility"] = json!({ "type": kind, "value": value });
            }
            _ => {
                return Ok(input_error(
                    "visibility_type and visibility_value must be given together",
                ));
            }
        }
        let client = match client("add a comment") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let ctx = ErrorContext::new(format!(
            "add a comment to {key} (POST /issue/{key}/comment)"
        ))
        .not_found(issue_not_found(&key))
        .bad_request("Check the visibility role/group name; the body is already sent as ADF.");
        match client.post(&format!("/issue/{key}/comment"), &body).await {
            Ok(created) => {
                let id = created
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Ok(CallToolResult::structured(json!({
                    "ok": true,
                    "issue_key": key,
                    "id": id,
                    "author": created.get("author").map(jira::simplify_user).unwrap_or(Value::Null),
                    "created": created.get("created").cloned().unwrap_or(Value::Null),
                    "url": format!("{}?focusedCommentId={id}", client.config.browse_url(&key)),
                })))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Page through an issue's comments (bodies rendered from ADF to text) with \
                       author, created, updated and visibility."
    )]
    #[tracing::instrument(name = "tool.get_comments", skip(self))]
    async fn get_comments(
        &self,
        Parameters(params): Parameters<GetCommentsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = match validate_issue_key(&params.issue_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let start_at = clamp(params.start_at, 0, i64::MAX / 2, 0);
        let max_results = clamp(params.max_results, 1, 100, 50);
        let order_by = match params.order_by.as_deref().map(str::trim) {
            None | Some("") | Some("-created") => "-created",
            Some("created") | Some("+created") => "created",
            Some(_) => return Ok(input_error("order_by must be `created` or `-created`")),
        };
        let client = match client("get comments") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let query = vec![
            ("startAt", start_at.to_string()),
            ("maxResults", max_results.to_string()),
            ("orderBy", order_by.to_owned()),
        ];
        let ctx = ErrorContext::new(format!("get comments of {key} (GET /issue/{key}/comment)"))
            .not_found(issue_not_found(&key));
        match client.get(&format!("/issue/{key}/comment"), &query).await {
            Ok(page) => {
                let mut value = jira::simplify_comment_page(&page);
                let total = page.get("total").and_then(Value::as_i64).unwrap_or(0);
                let returned = value
                    .get("comments")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0) as i64;
                value["issue_key"] = json!(key);
                value["has_more"] = json!(start_at.saturating_add(returned) < total);
                value["order_by"] = json!(order_by);
                Ok(CallToolResult::structured(value))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "List the workflow transitions currently available on an issue (id, name, \
                       target status; optionally the screen fields each requires). Call before \
                       transition_issue."
    )]
    #[tracing::instrument(name = "tool.get_transitions", skip(self))]
    async fn get_transitions(
        &self,
        Parameters(params): Parameters<GetTransitionsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = match validate_issue_key(&params.issue_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let client = match client("get transitions") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        match fetch_transitions(&client, &key, params.include_fields.unwrap_or(false)).await {
            Ok(transitions) => Ok(CallToolResult::structured(json!({
                "issue_key": key,
                "count": transitions.len(),
                "transitions": transitions,
            }))),
            Err(result) => Ok(result),
        }
    }

    #[tool(
        description = "Move an issue through a workflow transition by id or by transition/target \
                       status name (gated by JIRA_READ_ONLY), with an optional comment and screen \
                       fields such as resolution."
    )]
    #[tracing::instrument(name = "tool.transition_issue", skip(self))]
    async fn transition_issue(
        &self,
        Parameters(params): Parameters<TransitionIssueParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = refuse_if_read_only("transition_issue") {
            return Ok(refusal);
        }
        let key = match validate_issue_key(&params.issue_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let wanted = params.transition.trim().to_owned();
        if wanted.is_empty() || wanted.chars().count() > 255 || jira::has_control_chars(&wanted) {
            return Ok(input_error(
                "transition must be a transition id such as \"31\" or a name such as \"Done\"",
            ));
        }
        let comment = match params.comment.as_deref().filter(|c| !c.trim().is_empty()) {
            Some(comment) => match validate_rich_text("comment", comment) {
                Ok(text) => Some(text),
                Err(result) => return Ok(result),
            },
            None => None,
        };
        let screen_fields = match validate_extra_fields(params.fields, &[]) {
            Ok(fields) => fields,
            Err(result) => return Ok(result),
        };
        let client = match client("transition an issue") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        // Resolve the id or name against what Jira offers right now.
        let available = match fetch_transitions(&client, &key, false).await {
            Ok(list) => list,
            Err(result) => return Ok(result),
        };
        let matched = available.iter().find(|t| {
            let id = t.get("id").and_then(Value::as_str).unwrap_or("");
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            let to = t
                .get("to")
                .and_then(|to| to.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            id == wanted || name.eq_ignore_ascii_case(&wanted) || to.eq_ignore_ascii_case(&wanted)
        });
        let Some(matched) = matched else {
            let names: Vec<String> = available
                .iter()
                .map(|t| {
                    format!(
                        "{} \"{}\" → {}",
                        t.get("id").and_then(Value::as_str).unwrap_or("?"),
                        t.get("name").and_then(Value::as_str).unwrap_or("?"),
                        t.get("to")
                            .and_then(|to| to.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                    )
                })
                .collect();
            return Ok(structured_error(
                format!(
                    "No transition {wanted:?} is available on {key} for this account. Available: {}. \
                     Names match the transition name or its target status, case-insensitively; only \
                     transitions the account can perform right now are listed.",
                    if names.is_empty() { "(none)".to_owned() } else { names.join("; ") }
                ),
                json!({ "kind": "no_such_transition", "retryable": false, "available": available }),
            ));
        };
        let transition_id = matched
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut body = json!({ "transition": { "id": transition_id } });
        if !screen_fields.is_empty() {
            body["fields"] = Value::Object(screen_fields);
        }
        if let Some(comment) = comment {
            body["update"] =
                json!({ "comment": [{ "add": { "body": jira::text_to_adf(&comment) } }] });
        }
        let ctx = ErrorContext::new(format!("transition {key} (POST /issue/{key}/transitions)"))
            .not_found(issue_not_found(&key))
            .bad_request(
                "The transition is not valid right now or its screen requires fields (e.g. \
                 resolution). Call get_transitions with include_fields=true and pass the required \
                 fields in `fields`.",
            );
        match client
            .post(&format!("/issue/{key}/transitions"), &body)
            .await
        {
            Ok(_) => Ok(CallToolResult::structured(json!({
                "ok": true,
                "issue_key": key,
                "transition": matched,
                "url": client.config.browse_url(&key),
            }))),
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Assign an issue to an accountId, unassign it (no account_id), or reset it \
                       to the project default (\"-1\"). Gated by JIRA_READ_ONLY."
    )]
    #[tracing::instrument(name = "tool.assign_issue", skip(self))]
    async fn assign_issue(
        &self,
        Parameters(params): Parameters<AssignIssueParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(refusal) = refuse_if_read_only("assign_issue") {
            return Ok(refusal);
        }
        let key = match validate_issue_key(&params.issue_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let (account, action) = match params.account_id.as_deref().map(str::trim) {
            None | Some("") => (Value::Null, "unassigned"),
            Some("-1") => (json!("-1"), "assigned to the project default"),
            Some(id) => match validate_account_id("account_id", id) {
                Ok(id) => (json!(id), "assigned"),
                Err(result) => return Ok(result),
            },
        };
        let client = match client("assign an issue") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let ctx = ErrorContext::new(format!("assign {key} (PUT /issue/{key}/assignee)"))
            .not_found(issue_not_found(&key))
            .bad_request(
                "The accountId is unknown or the user cannot be assigned issues in this project. \
                 Use search_users with assignable_to_issue to find assignable accounts.",
            );
        match client
            .put(
                &format!("/issue/{key}/assignee"),
                &[],
                &json!({ "accountId": account }),
            )
            .await
        {
            Ok(_) => Ok(CallToolResult::structured(json!({
                "ok": true,
                "issue_key": key,
                "account_id": account,
                "result": action,
                "url": client.config.browse_url(&key),
            }))),
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "List projects the account can browse (key, id, name, type, style, lead), \
                       paginated, optionally filtered by a key/name substring or project type."
    )]
    #[tracing::instrument(name = "tool.list_projects", skip(self))]
    async fn list_projects(
        &self,
        Parameters(params): Parameters<ListProjectsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let query_text = match validate_query("query", params.query.as_deref().unwrap_or(""), true)
        {
            Ok(text) => text,
            Err(result) => return Ok(result),
        };
        let start_at = clamp(params.start_at, 0, i64::MAX / 2, 0);
        let max_results = clamp(params.max_results, 1, 100, 50);
        let type_key = match params.type_key.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(kind @ ("software" | "business" | "service_desk")) => Some(kind.to_owned()),
            Some(_) => {
                return Ok(input_error(
                    "type_key must be `software`, `business` or `service_desk`",
                ));
            }
        };
        let client = match client("list projects") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let mut query = vec![
            ("startAt", start_at.to_string()),
            ("maxResults", max_results.to_string()),
            ("orderBy", "key".to_owned()),
            ("expand", "lead,description".to_owned()),
        ];
        if !query_text.is_empty() {
            query.push(("query", query_text.clone()));
        }
        if let Some(kind) = &type_key {
            query.push(("typeKey", kind.clone()));
        }
        for key in &client.config.projects_filter {
            query.push(("keys", key.clone()));
        }
        let ctx = ErrorContext::new("list projects (GET /project/search)");
        match client.get("/project/search", &query).await {
            Ok(page) => {
                let filter = &client.config.projects_filter;
                let projects: Vec<Value> = page
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .take(1000)
                            .filter(|p| {
                                filter.is_empty()
                                    || p.get("key")
                                        .and_then(Value::as_str)
                                        .is_some_and(|k| filter.iter().any(|f| f == k))
                            })
                            .map(|p| jira::simplify_project(p, &client.config.site))
                            .collect()
                    })
                    .unwrap_or_default();
                let total = page.get("total").cloned().unwrap_or(Value::Null);
                let is_last = page.get("isLast").and_then(Value::as_bool).unwrap_or(true);
                Ok(CallToolResult::structured(json!({
                    "start_at": start_at,
                    "max_results": max_results,
                    "count": projects.len(),
                    "total": total,
                    "is_last": is_last,
                    "next_start_at": if is_last { Value::Null } else { json!(start_at.saturating_add(max_results)) },
                    "projects_filter": filter,
                    "projects": projects,
                })))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Project details: issue types, lead, components, versions, style \
                       (classic = company-managed, next-gen = team-managed)."
    )]
    #[tracing::instrument(name = "tool.get_project", skip(self))]
    async fn get_project(
        &self,
        Parameters(params): Parameters<GetProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = match validate_project_key(&params.project_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let client = match client("get a project") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let ctx = ErrorContext::new(format!("get project {key} (GET /project/{key})"))
            .not_found(project_not_found(&key));
        let query = vec![("expand", "issueTypes,lead,description".to_owned())];
        match client.get(&format!("/project/{key}"), &query).await {
            Ok(project) => Ok(CallToolResult::structured(jira::simplify_project(
                &project,
                &client.config.site,
            ))),
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Issue types available for creating issues in a project (id, name, subtask \
                       flag, hierarchy level) from the per-project create metadata."
    )]
    #[tracing::instrument(name = "tool.list_issue_types", skip(self))]
    async fn list_issue_types(
        &self,
        Parameters(params): Parameters<ListIssueTypesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = match validate_project_key(&params.project_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let max_results = clamp(params.max_results, 1, 200, 50);
        let client = match client("list issue types") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        match fetch_issue_types(&client, &key, max_results).await {
            Ok((types, total)) => Ok(CallToolResult::structured(json!({
                "project_key": key,
                "count": types.len(),
                "total": total,
                "issue_types": types,
            }))),
            Err(result) => Ok(result),
        }
    }

    #[tool(
        description = "Fields on the create screen for a project + issue type: id, name, required, \
                       type, allowed values. Call before create_issue when Jira complains about a \
                       field."
    )]
    #[tracing::instrument(name = "tool.get_create_fields", skip(self))]
    async fn get_create_fields(
        &self,
        Parameters(params): Parameters<GetCreateFieldsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = match validate_project_key(&params.project_key) {
            Ok(key) => key,
            Err(result) => return Ok(result),
        };
        let wanted = params.issue_type.trim().to_owned();
        if wanted.is_empty() || wanted.chars().count() > 255 || jira::has_control_chars(&wanted) {
            return Ok(input_error(
                "issue_type must be a type name such as Task or a numeric id",
            ));
        }
        let client = match client("get create fields") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let (type_id, type_name) = if jira::is_numeric_id(&wanted) {
            (wanted.clone(), Value::Null)
        } else {
            let (types, _) = match fetch_issue_types(&client, &key, 200).await {
                Ok(found) => found,
                Err(result) => return Ok(result),
            };
            let found = types.iter().find(|t| {
                t.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(&wanted))
            });
            match found.and_then(|t| t.get("id").and_then(Value::as_str)) {
                Some(id) => (
                    id.to_owned(),
                    found
                        .and_then(|t| t.get("name"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
                None => {
                    let names: Vec<&str> = types
                        .iter()
                        .filter_map(|t| t.get("name").and_then(Value::as_str))
                        .collect();
                    return Ok(structured_error(
                        format!(
                            "Issue type {wanted:?} is not available in project {key}. Available: {}.",
                            if names.is_empty() { "(none)".to_owned() } else { names.join(", ") }
                        ),
                        json!({ "kind": "no_such_issue_type", "retryable": false, "available": types }),
                    ));
                }
            }
        };
        let ctx = ErrorContext::new(format!(
            "get create fields for {key}/{type_id} (GET /issue/createmeta/{key}/issuetypes/{type_id})"
        ))
        .not_found(format!(
            "{} Or issue type id {type_id} is not usable in that project — see list_issue_types.",
            project_not_found(&key)
        ));
        let query = vec![
            ("startAt", "0".to_owned()),
            ("maxResults", "200".to_owned()),
        ];
        match client
            .get(
                &format!("/issue/createmeta/{key}/issuetypes/{type_id}"),
                &query,
            )
            .await
        {
            Ok(page) => {
                let fields: Vec<Value> = page
                    .get("fields")
                    .or_else(|| page.get("values"))
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .take(500)
                            .map(jira::simplify_create_field)
                            .collect()
                    })
                    .unwrap_or_default();
                let required: Vec<Value> = fields
                    .iter()
                    .filter(|f| f.get("required").and_then(Value::as_bool).unwrap_or(false))
                    .filter_map(|f| f.get("id").cloned())
                    .collect();
                Ok(CallToolResult::structured(json!({
                    "project_key": key,
                    "issue_type": { "id": type_id, "name": type_name },
                    "required": required,
                    "count": fields.len(),
                    "fields": fields,
                })))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }

    #[tool(
        description = "Find users by display name or email prefix to obtain accountIds; optionally \
                       only users assignable to a project or issue. Empty result (not 403) when the \
                       account lacks Browse users and groups."
    )]
    #[tracing::instrument(name = "tool.search_users", skip(self))]
    async fn search_users(
        &self,
        Parameters(params): Parameters<SearchUsersParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = match validate_query("query", &params.query, false) {
            Ok(text) => text,
            Err(result) => return Ok(result),
        };
        let max_results = clamp(params.max_results, 1, 50, 20);
        let project = params
            .assignable_to_project
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let issue = params
            .assignable_to_issue
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty());
        if project.is_some() && issue.is_some() {
            return Ok(input_error(
                "pass at most one of assignable_to_project and assignable_to_issue",
            ));
        }
        let mut query = vec![
            ("query", text.clone()),
            ("maxResults", max_results.to_string()),
        ];
        let mut scope = json!("all");
        if let Some(project) = project {
            match validate_project_key(project) {
                Ok(key) => {
                    scope = json!({ "assignable_to_project": key });
                    query.push(("project", key));
                }
                Err(result) => return Ok(result),
            }
        }
        if let Some(issue) = issue {
            match validate_issue_key(issue) {
                Ok(key) => {
                    scope = json!({ "assignable_to_issue": key });
                    query.push(("issueKey", key));
                }
                Err(result) => return Ok(result),
            }
        }
        let path = if scope.is_string() {
            "/user/search"
        } else {
            "/user/assignable/search"
        };
        let client = match client("search users") {
            Ok(client) => client,
            Err(result) => return Ok(result),
        };
        let ctx = ErrorContext::new(format!("search users (GET {path})")).not_found(
            "The project or issue named in the assignable scope was not found or is not browsable.",
        );
        match client.get(path, &query).await {
            Ok(list) => {
                let users: Vec<Value> = list
                    .as_array()
                    .map(|list| list.iter().take(200).map(jira::simplify_user).collect())
                    .unwrap_or_default();
                Ok(CallToolResult::structured(json!({
                    "query": text,
                    "scope": scope,
                    "count": users.len(),
                    "users": users,
                    "note": if users.is_empty() {
                        "No match. Jira returns an empty list (not 403) when the account lacks the \
                         'Browse users and groups' permission, and hides inactive users and users \
                         whose privacy settings block the match."
                    } else {
                        ""
                    },
                })))
            }
            Err(err) => Ok(jira_error(&err, &ctx, Some(&client.config.site))),
        }
    }
}

/// GET the transitions of an issue, simplified. Shared by `get_transitions`
/// and `transition_issue` (name resolution).
async fn fetch_transitions(
    client: &Client,
    key: &str,
    include_fields: bool,
) -> Result<Vec<Value>, CallToolResult> {
    let mut query = Vec::new();
    if include_fields {
        query.push(("expand", "transitions.fields".to_owned()));
    }
    let ctx = ErrorContext::new(format!(
        "get transitions of {key} (GET /issue/{key}/transitions)"
    ))
    .not_found(issue_not_found(key));
    match client
        .get(&format!("/issue/{key}/transitions"), &query)
        .await
    {
        Ok(page) => Ok(page
            .get("transitions")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .take(200)
                    .map(jira::simplify_transition)
                    .collect()
            })
            .unwrap_or_default()),
        Err(err) => Err(jira_error(&err, &ctx, Some(&client.config.site))),
    }
}

/// GET the create-metadata issue types of a project, simplified, plus the
/// reported total.
async fn fetch_issue_types(
    client: &Client,
    key: &str,
    max_results: i64,
) -> Result<(Vec<Value>, Value), CallToolResult> {
    let query = vec![
        ("startAt", "0".to_owned()),
        ("maxResults", max_results.to_string()),
    ];
    let ctx = ErrorContext::new(format!(
        "list issue types of {key} (GET /issue/createmeta/{key}/issuetypes)"
    ))
    .not_found(format!(
        "{} The account also needs Create Issues permission in the project.",
        project_not_found(key)
    ));
    match client
        .get(&format!("/issue/createmeta/{key}/issuetypes"), &query)
        .await
    {
        Ok(page) => {
            let types: Vec<Value> = page
                .get("issueTypes")
                .or_else(|| page.get("values"))
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .take(500)
                        .map(|t| {
                            json!({
                                "id": t.get("id").cloned().unwrap_or(Value::Null),
                                "name": t.get("name").cloned().unwrap_or(Value::Null),
                                "description": t.get("description").and_then(Value::as_str).map(|d| jira::bound_chars(d.to_owned(), 500)).unwrap_or_default(),
                                "subtask": t.get("subtask").and_then(Value::as_bool).unwrap_or(false),
                                "hierarchyLevel": t.get("hierarchyLevel").cloned().unwrap_or(Value::Null),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok((types, page.get("total").cloned().unwrap_or(Value::Null)))
        }
        Err(err) => Err(jira_error(&err, &ctx, Some(&client.config.site))),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for JiraServer {
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
            "Jira Cloud MCP server (REST API v3, Basic auth with an Atlassian API token) running \
             as a sandboxed WebAssembly component on Cosmonic Desktop. Start with `check_auth` \
             (or `get_myself`) to validate credentials and learn the caller's accountId. Search \
             with `search_issues` (bounded JQL, cursor pagination via next_page_token) and \
             `count_issues`; read with `get_issue`, `get_comments`, `get_transitions`, \
             `list_projects`, `get_project`, `list_issue_types`, `get_create_fields`, \
             `search_users`. Writes — `create_issue`, `update_issue`, `add_comment`, \
             `transition_issue`, `assign_issue` — take plain text and send ADF; they are refused \
             when JIRA_READ_ONLY=true. Rich text comes back rendered to text.\n\n\
             This server publishes skills — playbooks describing when and how to use its tools, \
             the error catalogue, and Jira's quirks (ADF, transitions, scoped tokens, rate \
             limits). Read `skill://index.json` for the catalog, then \
             `skill://atlassian-jira-mcp/SKILL.md`.",
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
