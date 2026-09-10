//! GitHub REST API client over `wasi:http` via the bridge's outbound client,
//! bounded by the workload's `allowedHosts` egress allowlist (the one host is
//! `api.github.com`).
//!
//! Every request carries the GitHub-recommended headers (`Accept:
//! application/vnd.github+json`, a descriptive `User-Agent`, and
//! `X-GitHub-Api-Version: 2022-11-28`). Authentication is **optional**: when a
//! `GITHUB_TOKEN` is set in the environment (flattened from a Cosmonic secret
//! reference) it is sent as a bearer token, which raises the rate limit and
//! grants access to private repositories. Without a token the client runs
//! unauthenticated, which works for public data at 60 requests/hour — a missing
//! token is not an error.

use base64::Engine as _;
use serde_json::{json, Value};

use crate::bridge::outbound;

/// The only host this client reaches (also the workload's `allowedHosts`).
const BASE_URL: &str = "https://api.github.com";

/// Descriptive User-Agent — GitHub requires one on every request.
const USER_AGENT: &str = "github-mcp (Cosmonic Desktop example)";

/// Media type requesting the v3 JSON representation.
const ACCEPT: &str = "application/vnd.github+json";

/// Pinned REST API version (GitHub's date-based scheme).
const API_VERSION: &str = "2022-11-28";

/// Cap on decoded file contents (~100 KB). Anything larger is truncated and
/// flagged, so a big file can't exhaust memory or an agent's context window.
const MAX_FILE_BYTES: usize = 100 * 1024;

/// A GitHub client error, surfaced to the model as a friendly tool-level error.
pub enum GitHubError {
    /// The resource does not exist (HTTP 404).
    NotFound(String),
    /// The (unauthenticated) rate limit is exhausted (HTTP 403 / 429 with the
    /// rate-limit signal).
    RateLimited,
    /// The request failed to build, couldn't reach the host, or the API
    /// returned another non-2xx status.
    Request(String),
}

impl GitHubError {
    /// Renders the error as an MCP tool-level error result (not a protocol
    /// error — the request was valid, the upstream just said no).
    pub fn into_tool_result(self) -> rmcp::model::CallToolResult {
        use rmcp::model::{CallToolResult, ContentBlock};
        let text = match self {
            GitHubError::NotFound(what) => {
                format!("{what} was not found on GitHub (HTTP 404). Check the owner, repo, and path.")
            }
            GitHubError::RateLimited => String::from(
                "GitHub API rate limit exceeded. Unauthenticated requests are capped at 60/hour. \
                 Add a `github-token` secret (a GitHub personal access token exposed as the \
                 GITHUB_TOKEN environment variable — see this server's README) to raise the limit \
                 and enable private repositories.",
            ),
            GitHubError::Request(detail) => format!("GitHub request failed: {detail}"),
        };
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

/// The optional token, read fresh from the environment on each request so a
/// secret rotation takes effect without a restart. Absent is fine.
fn token() -> Option<String> {
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.trim().is_empty())
}

/// Percent-encodes a query-parameter value (unreserved set `A-Za-z0-9-_.~`).
fn encode(value: &str) -> String {
    encode_with(value, false)
}

/// Percent-encodes a URL path, preserving `/` segment separators.
fn encode_path(value: &str) -> String {
    encode_with(value, true)
}

fn encode_with(value: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// GETs a GitHub REST endpoint (an absolute path, query already appended) and
/// returns the decoded JSON body. Injects the standard headers and, when
/// present, the bearer token. Maps status codes to friendly errors.
///
/// `what` names the resource for a 404 message (e.g. "repository owner/repo").
async fn get(path: &str, what: &str) -> Result<Value, GitHubError> {
    let url = format!("{BASE_URL}{path}");

    let mut builder = http::Request::get(&url)
        .header("Accept", ACCEPT)
        .header("User-Agent", USER_AGENT)
        .header("X-GitHub-Api-Version", API_VERSION);
    if let Some(tok) = token() {
        builder = builder.header("Authorization", format!("Bearer {tok}"));
    }
    let request = builder
        .body(bytes::Bytes::new())
        .map_err(|err| GitHubError::Request(format!("building request: {err}")))?;

    let response = outbound::fetch(request)
        .await
        .map_err(|err| match err {
            // A host missing from allowedHosts (or DNS/TLS failure) surfaces
            // here; api.github.com is the only host this tool ever needs.
            outbound::Error::Wasi(detail) => GitHubError::Request(format!(
                "couldn't reach api.github.com — it may not be in this workload's egress \
                 allowlist (allowedHosts). (details: {detail})"
            )),
            other => GitHubError::Request(other.to_string()),
        })?;

    let status = response.status();
    let rate_remaining_zero = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "0")
        .unwrap_or(false);
    let body = response.into_body();

    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|err| GitHubError::Request(format!("decoding response: {err}")));
    }

    // Non-2xx: pull GitHub's JSON `message` for detail when present.
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());

    match status.as_u16() {
        404 => Err(GitHubError::NotFound(what.to_owned())),
        403 | 429
            if rate_remaining_zero
                || message.to_ascii_lowercase().contains("rate limit") =>
        {
            Err(GitHubError::RateLimited)
        }
        code => Err(GitHubError::Request(format!("HTTP {code}: {message}"))),
    }
}

/// `search_repositories` — free-text repository search, capped at 10 results.
pub async fn search_repositories(query: &str, per_page: u32) -> Result<Value, GitHubError> {
    let per_page = per_page.clamp(1, 10);
    let path = format!(
        "/search/repositories?q={}&per_page={per_page}",
        encode(query)
    );
    let body = get(&path, "search").await?;

    let items: Vec<Value> = body
        .get("items")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(repo_summary).collect())
        .unwrap_or_default();

    Ok(json!({
        "query": query,
        "total_count": body.get("total_count").cloned().unwrap_or(json!(items.len())),
        "count": items.len(),
        "items": items,
    }))
}

/// `get_repository` — key metadata for one repository.
pub async fn get_repository(owner: &str, repo: &str) -> Result<Value, GitHubError> {
    let path = format!("/repos/{}/{}", encode_path(owner), encode_path(repo));
    let what = format!("repository {owner}/{repo}");
    let r = get(&path, &what).await?;

    Ok(json!({
        "full_name": r.get("full_name"),
        "description": r.get("description"),
        "stars": r.get("stargazers_count"),
        "forks": r.get("forks_count"),
        "language": r.get("language"),
        "open_issues": r.get("open_issues_count"),
        "license": r
            .get("license")
            .and_then(|l| l.get("spdx_id").or_else(|| l.get("name")))
            .cloned()
            .unwrap_or(Value::Null),
        "default_branch": r.get("default_branch"),
        "html_url": r.get("html_url"),
    }))
}

/// `list_issues` — open/closed/all issues, pull requests filtered out (GitHub
/// returns PRs from this endpoint; drop any item with a `pull_request` field).
pub async fn list_issues(owner: &str, repo: &str, state: &str) -> Result<Value, GitHubError> {
    let state = match state.trim().to_ascii_lowercase().as_str() {
        "closed" => "closed",
        "all" => "all",
        _ => "open",
    };
    let path = format!(
        "/repos/{}/{}/issues?state={state}&per_page=20",
        encode_path(owner),
        encode_path(repo)
    );
    let what = format!("repository {owner}/{repo}");
    let body = get(&path, &what).await?;

    let issues: Vec<Value> = body
        .as_array()
        .map(|arr| {
            arr.iter()
                // Drop pull requests — the issues endpoint returns them too.
                .filter(|item| item.get("pull_request").is_none())
                .map(issue_summary)
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "owner": owner,
        "repo": repo,
        "state": state,
        "count": issues.len(),
        "issues": issues,
    }))
}

/// `get_file_contents` — a file's decoded text (base64 from the API), or a
/// directory listing when the path is a directory.
pub async fn get_file_contents(
    owner: &str,
    repo: &str,
    path: &str,
    git_ref: Option<&str>,
) -> Result<Value, GitHubError> {
    let mut url_path = format!(
        "/repos/{}/{}/contents/{}",
        encode_path(owner),
        encode_path(repo),
        encode_path(path.trim_start_matches('/'))
    );
    if let Some(r) = git_ref.map(str::trim).filter(|r| !r.is_empty()) {
        url_path.push_str(&format!("?ref={}", encode(r)));
    }
    let what = format!("{path} in {owner}/{repo}");
    let body = get(&url_path, &what).await?;

    // A directory comes back as a JSON array of entries.
    if let Some(entries) = body.as_array() {
        let listing: Vec<Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "name": e.get("name"),
                    "path": e.get("path"),
                    "type": e.get("type"),
                    "size": e.get("size"),
                })
            })
            .collect();
        return Ok(json!({
            "path": path,
            "type": "dir",
            "count": listing.len(),
            "entries": listing,
        }));
    }

    // A file comes back as an object with base64 `content`.
    let encoding = body.get("encoding").and_then(Value::as_str).unwrap_or("");
    if encoding != "base64" {
        // Files over 1 MB return encoding "none" with empty content; the API
        // directs callers to the blob/raw endpoints for those.
        return Ok(json!({
            "path": path,
            "size": body.get("size"),
            "truncated": true,
            "content": "",
            "note": "File is too large for the contents API (returned without inline base64). \
                     Fetch it from its download_url or the git blobs API instead.",
        }));
    }

    let raw = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    // GitHub wraps the base64 in newlines; the standard alphabet, whitespace
    // stripped, decodes it.
    let cleaned: String = raw.split_whitespace().collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|err| GitHubError::Request(format!("decoding file content: {err}")))?;

    let total = decoded.len();
    let truncated = total > MAX_FILE_BYTES;
    let slice = if truncated { &decoded[..MAX_FILE_BYTES] } else { &decoded[..] };
    let content = String::from_utf8_lossy(slice).into_owned();

    Ok(json!({
        "path": path,
        "size": total,
        "truncated": truncated,
        "content": content,
    }))
}

/// Projects a repository object to the summary fields the tools return.
fn repo_summary(r: &Value) -> Value {
    json!({
        "name": r.get("name"),
        "full_name": r.get("full_name"),
        "description": r.get("description"),
        "stars": r.get("stargazers_count"),
        "language": r.get("language"),
        "html_url": r.get("html_url"),
    })
}

/// Projects an issue object to the summary fields `list_issues` returns.
fn issue_summary(i: &Value) -> Value {
    let labels: Vec<Value> = i
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| match l {
                    // Labels are objects; a name-only string form is tolerated.
                    Value::Object(_) => l.get("name").cloned(),
                    Value::String(_) => Some(l.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "number": i.get("number"),
        "title": i.get("title"),
        "state": i.get("state"),
        "user": i.get("user").and_then(|u| u.get("login")).cloned().unwrap_or(Value::Null),
        "labels": labels,
        "html_url": i.get("html_url"),
    })
}
