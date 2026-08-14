//! GitLab REST API v4 client over `wasi:http` via the bridge's outbound client,
//! bounded by the workload's `allowedHosts` egress allowlist (the one host is
//! `gitlab.com`).
//!
//! Every request carries `Accept: application/json` and a descriptive
//! `User-Agent`. Authentication is **optional**: when a `GITLAB_TOKEN` is set in
//! the environment (flattened from a Cosmonic secret reference) it is sent in
//! the `PRIVATE-TOKEN` header (GitLab's scheme), which raises the rate limit and
//! grants access to private projects. Without a token the client runs
//! unauthenticated, which works for public projects — a missing token is not an
//! error.
//!
//! GitLab addresses a project by a numeric ID or by its URL-encoded
//! `namespace/project` path (`gitlab-org/gitlab` → `gitlab-org%2Fgitlab`). The
//! `id` argument accepts either form; this client percent-encodes it (slashes
//! included) before placing it in the path.

use base64::Engine as _;
use serde_json::{json, Value};

use crate::bridge::outbound;

/// The API base. The only host this client reaches is `gitlab.com` (also the
/// workload's `allowedHosts`).
const BASE_URL: &str = "https://gitlab.com/api/v4";

/// Descriptive User-Agent sent on every request.
const USER_AGENT: &str = "gitlab-mcp (Cosmonic Desktop example)";

/// Requested representation.
const ACCEPT: &str = "application/json";

/// Cap on decoded file contents (~100 KB). Anything larger is truncated and
/// flagged, so a big file can't exhaust memory or an agent's context window.
const MAX_FILE_BYTES: usize = 100 * 1024;

/// A GitLab client error, surfaced to the model as a friendly tool-level error.
pub enum GitLabError {
    /// The resource does not exist (HTTP 404).
    NotFound(String),
    /// The request was unauthorized or forbidden (HTTP 401 / 403) — often a
    /// private project that needs a token.
    AuthRequired(String),
    /// The rate limit is exhausted (HTTP 429).
    RateLimited,
    /// The request failed to build, couldn't reach the host, or the API
    /// returned another non-2xx status.
    Request(String),
}

impl GitLabError {
    /// Renders the error as an MCP tool-level error result (not a protocol
    /// error — the request was valid, the upstream just said no).
    pub fn into_tool_result(self) -> rmcp::model::CallToolResult {
        use rmcp::model::{CallToolResult, ContentBlock};
        let text = match self {
            GitLabError::NotFound(what) => {
                format!("{what} was not found on GitLab (HTTP 404). Check the project id (numeric or 'namespace/project') and path.")
            }
            GitLabError::AuthRequired(what) => format!(
                "{what} could not be accessed (HTTP 401/403). It may be a private project. Add a \
                 `gitlab-token` secret (a GitLab personal access token exposed as the GITLAB_TOKEN \
                 environment variable — see this server's README) to reach private projects."
            ),
            GitLabError::RateLimited => String::from(
                "GitLab API rate limit exceeded (HTTP 429). Add a `gitlab-token` secret (a GitLab \
                 personal access token exposed as the GITLAB_TOKEN environment variable — see this \
                 server's README) to raise the limit.",
            ),
            GitLabError::Request(detail) => format!("GitLab request failed: {detail}"),
        };
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

/// The optional token, read fresh from the environment on each request so a
/// secret rotation takes effect without a restart. Absent is fine.
fn token() -> Option<String> {
    std::env::var("GITLAB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
}

/// Percent-encodes a value against the unreserved set `A-Za-z0-9-_.~`. Every
/// other byte — slashes included — becomes `%XX`. Used for query-parameter
/// values, and for a project `id` or file `path` that GitLab requires
/// URL-encoded (so `gitlab-org/gitlab` → `gitlab-org%2Fgitlab`).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// GETs a GitLab REST endpoint (an absolute path under `/api/v4`, query already
/// appended) and returns the decoded JSON body. Injects the standard headers
/// and, when present, the `PRIVATE-TOKEN`. Maps status codes to friendly
/// errors.
///
/// `what` names the resource for a 404/403 message (e.g. "project gitlab-org/gitlab").
async fn get(path: &str, what: &str) -> Result<Value, GitLabError> {
    let url = format!("{BASE_URL}{path}");

    let mut builder = http::Request::get(&url)
        .header("Accept", ACCEPT)
        .header("User-Agent", USER_AGENT);
    if let Some(tok) = token() {
        builder = builder.header("PRIVATE-TOKEN", tok);
    }
    let request = builder
        .body(bytes::Bytes::new())
        .map_err(|err| GitLabError::Request(format!("building request: {err}")))?;

    let response = outbound::fetch(request).await.map_err(|err| match err {
        // A host missing from allowedHosts (or DNS/TLS failure) surfaces
        // here; gitlab.com is the only host this tool ever needs.
        outbound::Error::Wasi(detail) => GitLabError::Request(format!(
            "couldn't reach gitlab.com — it may not be in this workload's egress \
             allowlist (allowedHosts). (details: {detail})"
        )),
        other => GitLabError::Request(other.to_string()),
    })?;

    let status = response.status();
    let body = response.into_body();

    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|err| GitLabError::Request(format!("decoding response: {err}")));
    }

    // Non-2xx: pull GitLab's JSON `message`/`error` for detail when present.
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("error"))
                .and_then(|m| match m {
                    Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                })
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());

    match status.as_u16() {
        404 => Err(GitLabError::NotFound(what.to_owned())),
        401 | 403 => Err(GitLabError::AuthRequired(what.to_owned())),
        429 => Err(GitLabError::RateLimited),
        code => Err(GitLabError::Request(format!("HTTP {code}: {message}"))),
    }
}

/// `search_projects` — free-text project search, capped at 10 results, ordered
/// by star count.
pub async fn search_projects(query: &str, per_page: u32) -> Result<Value, GitLabError> {
    let per_page = per_page.clamp(1, 10);
    let path = format!(
        "/projects?search={}&per_page={per_page}&order_by=star_count",
        encode(query)
    );
    let body = get(&path, "search").await?;

    let items: Vec<Value> = body
        .as_array()
        .map(|arr| arr.iter().map(project_summary).collect())
        .unwrap_or_default();

    Ok(json!({
        "query": query,
        "count": items.len(),
        "items": items,
    }))
}

/// `get_project` — key metadata for one project (numeric ID or
/// `namespace/project` path).
pub async fn get_project(id: &str) -> Result<Value, GitLabError> {
    let path = format!("/projects/{}", encode(id));
    let what = format!("project {id}");
    let p = get(&path, &what).await?;

    Ok(json!({
        "name": p.get("name"),
        "path_with_namespace": p.get("path_with_namespace"),
        "description": p.get("description"),
        "star_count": p.get("star_count"),
        "forks_count": p.get("forks_count"),
        "default_branch": p.get("default_branch"),
        "visibility": p.get("visibility"),
        "web_url": p.get("web_url"),
    }))
}

/// `list_issues` — opened/closed/all issues for a project.
pub async fn list_issues(id: &str, state: &str) -> Result<Value, GitLabError> {
    // GitLab issue state values are `opened`/`closed`/`all`; map the GitHub-ish
    // `open` alias to `opened`.
    let state = match state.trim().to_ascii_lowercase().as_str() {
        "closed" => "closed",
        "all" => "all",
        _ => "opened",
    };
    let path = format!(
        "/projects/{}/issues?state={state}&per_page=20",
        encode(id)
    );
    let what = format!("project {id}");
    let body = get(&path, &what).await?;

    let issues: Vec<Value> = body
        .as_array()
        .map(|arr| arr.iter().map(issue_summary).collect())
        .unwrap_or_default();

    Ok(json!({
        "id": id,
        "state": state,
        "count": issues.len(),
        "issues": issues,
    }))
}

/// `get_file_contents` — a file's decoded text (base64 from the API).
///
/// GitLab's files endpoint requires a `ref`. When the caller omits it, read the
/// project's `default_branch` first (mirrors GitHub defaulting to the default
/// branch) and use that.
pub async fn get_file_contents(
    id: &str,
    path: &str,
    git_ref: Option<&str>,
) -> Result<Value, GitLabError> {
    let git_ref = git_ref.map(str::trim).filter(|r| !r.is_empty());

    // `ref` is required by the files API; default to the project's default
    // branch when the caller didn't pin one.
    let git_ref: String = match git_ref {
        Some(r) => r.to_owned(),
        None => {
            let project_path = format!("/projects/{}", encode(id));
            let what = format!("project {id}");
            let project = get(&project_path, &what).await?;
            project
                .get("default_branch")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    GitLabError::Request(format!(
                        "project {id} has no default branch; specify 'ref' explicitly"
                    ))
                })?
        }
    };

    let file_path = path.trim_start_matches('/');
    let url_path = format!(
        "/projects/{}/repository/files/{}?ref={}",
        encode(id),
        encode(file_path),
        encode(&git_ref)
    );
    let what = format!("{path} in project {id} (ref {git_ref})");
    let body = get(&url_path, &what).await?;

    // GitLab returns the file as an object with base64 `content` (its default
    // encoding is "base64").
    let encoding = body.get("encoding").and_then(Value::as_str).unwrap_or("");
    if encoding != "base64" {
        return Ok(json!({
            "path": path,
            "ref": git_ref,
            "size": body.get("size"),
            "truncated": true,
            "content": "",
            "note": format!(
                "File was returned with encoding '{encoding}' rather than base64; \
                 fetch it from the raw file endpoint instead."
            ),
        }));
    }

    let raw = body.get("content").and_then(Value::as_str).unwrap_or("");
    // The base64 may carry whitespace/newlines; strip it, then decode with the
    // standard alphabet.
    let cleaned: String = raw.split_whitespace().collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|err| GitLabError::Request(format!("decoding file content: {err}")))?;

    let total = decoded.len();
    let truncated = total > MAX_FILE_BYTES;
    let slice = if truncated {
        &decoded[..MAX_FILE_BYTES]
    } else {
        &decoded[..]
    };
    let content = String::from_utf8_lossy(slice).into_owned();

    Ok(json!({
        "path": body.get("file_path").cloned().unwrap_or_else(|| json!(path)),
        "ref": git_ref,
        "size": total,
        "truncated": truncated,
        "content": content,
    }))
}

/// Projects a project object to the summary fields the tools return.
fn project_summary(p: &Value) -> Value {
    json!({
        "path_with_namespace": p.get("path_with_namespace"),
        "name": p.get("name"),
        "description": p.get("description"),
        "star_count": p.get("star_count"),
        "web_url": p.get("web_url"),
    })
}

/// Projects an issue object to the summary fields `list_issues` returns.
fn issue_summary(i: &Value) -> Value {
    let labels: Vec<Value> = i
        .get("labels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    json!({
        "iid": i.get("iid"),
        "title": i.get("title"),
        "state": i.get("state"),
        "author": i
            .get("author")
            .and_then(|u| u.get("username"))
            .cloned()
            .unwrap_or(Value::Null),
        "labels": labels,
        "web_url": i.get("web_url"),
    })
}
