//! The MCP server: tool definitions and result rendering for the AWS client
//! in [`crate::aws`].
//!
//! Every tool returns `Ok(CallToolResult)`: shaped results carry
//! `structuredContent` plus a readable text fallback, and every failure —
//! argument validation, the local write gate, a missing secret, an AWS error
//! — is a tool-level error (`isError: true`) whose text says what to do and
//! whose `structuredContent` carries `{error, retryable, http_status,
//! message}`. `Err(ErrorData)` is reserved for requests rmcp cannot route or
//! parse.
//!
//! Alongside the tools, this server publishes **skills** — natural-language
//! playbooks served over the MCP resources primitive under `skill://` URIs.
//! See [`crate::skills`]; the handlers at the bottom of this file are the
//! protocol surface for them.

use std::collections::BTreeMap;

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

use crate::aws::{self, Config, Error};
use crate::sigv4::{self, time};
use crate::skills;

/// The MCP server for this component. One instance is created per request —
/// the transport is stateless (2026-07-28 spec), so do not keep per-session
/// state on this struct.
#[derive(Clone)]
pub struct TemplateServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegionParams {
    /// Region override for this call (e.g. "eu-west-1"). Defaults to
    /// AWS_REGION. STS works in any region.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListBucketsParams {
    /// Only buckets whose name starts with this prefix.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Buckets per page, 1..10000 (default 100; clamped).
    #[serde(default)]
    pub max_buckets: Option<i64>,
    /// `next_continuation_token` from the previous page.
    #[serde(default)]
    pub continuation_token: Option<String>,
    /// Only buckets in this region. Must equal the endpoint region (the
    /// `region` argument / AWS_REGION) or S3 rejects the call.
    #[serde(default)]
    pub bucket_region: Option<String>,
    /// Endpoint region for this call. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListObjectsParams {
    /// Bucket name (exact, from s3_list_buckets).
    pub bucket: String,
    /// Only keys starting with this prefix (e.g. "logs/2024/").
    #[serde(default)]
    pub prefix: Option<String>,
    /// Usually "/": collapses keys below the next delimiter into
    /// common_prefixes ("folders"), which count against max_keys.
    #[serde(default)]
    pub delimiter: Option<String>,
    /// Keys per page, 1..1000 (default 100; clamped).
    #[serde(default)]
    pub max_keys: Option<i64>,
    /// `next_continuation_token` from the previous page.
    #[serde(default)]
    pub continuation_token: Option<String>,
    /// Start listing after this key (alternative to a continuation token).
    #[serde(default)]
    pub start_after: Option<String>,
    /// The bucket's region (see s3_list_buckets). Defaults to AWS_REGION;
    /// a wrong region fails with a 301 naming the right one.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetObjectParams {
    /// Bucket name.
    pub bucket: String,
    /// Object key, verbatim (spaces, unicode, '+', '?' are encoded for you;
    /// '/' keeps its meaning). Never pre-encode it.
    pub key: String,
    /// Bytes to read from the start of the object, 1..1048576 (default
    /// 65536; clamped). A 206 partial read sets `truncated: true`.
    #[serde(default)]
    pub max_bytes: Option<i64>,
    /// Specific version of a versioned object.
    #[serde(default)]
    pub version_id: Option<String>,
    /// The bucket's region. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PutObjectParams {
    /// Bucket name.
    pub bucket: String,
    /// Object key to create or overwrite.
    pub key: String,
    /// UTF-8 text content, at most 1 MiB.
    pub body: String,
    /// Content-Type to store (default "text/plain; charset=utf-8").
    #[serde(default)]
    pub content_type: Option<String>,
    /// true = only create, never overwrite (If-None-Match: *; an existing
    /// key fails with 412 PreconditionFailed).
    #[serde(default)]
    pub if_none_match: Option<bool>,
    /// The bucket's region. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeInstancesParams {
    /// Specific instance ids (i-xxxxxxxxxxxxxxxxx), at most 100. When
    /// given, max_results is not sent (EC2 rejects the combination).
    #[serde(default)]
    pub instance_ids: Option<Vec<String>>,
    /// EC2 filters as {"filter-name": ["value", ...]} (a single string is
    /// accepted too), e.g. {"instance-state-name": ["running"],
    /// "tag:Name": ["web*"], "vpc-id": ["vpc-0abc"]}. At most 20 filters
    /// with 20 values each; '*' and '?' wildcards work in values.
    #[serde(default)]
    pub filters: Option<BTreeMap<String, Value>>,
    /// Instances per page, 5..1000 (default 100; clamped; EC2's minimum is 5).
    #[serde(default)]
    pub max_results: Option<i64>,
    /// `next_token` from the previous page.
    #[serde(default)]
    pub next_token: Option<String>,
    /// Region to list. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFunctionsParams {
    /// Functions per page, 1..50 (default 50; clamped — Lambda never returns
    /// more than 50 per page).
    #[serde(default)]
    pub max_items: Option<i64>,
    /// `next_marker` from the previous page.
    #[serde(default)]
    pub marker: Option<String>,
    /// true also lists every published version of each function.
    #[serde(default)]
    pub include_versions: Option<bool>,
    /// Region to list. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

/// How a Lambda function is invoked.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub enum InvocationType {
    /// Synchronous: wait for the result (default).
    RequestResponse,
    /// Asynchronous: 202 with no payload; Lambda may retry the function.
    Event,
    /// Permission check only (204); never runs the function and is never
    /// gated by AWS_ALLOW_WRITES.
    DryRun,
}

impl InvocationType {
    fn as_str(self) -> &'static str {
        match self {
            InvocationType::RequestResponse => "RequestResponse",
            InvocationType::Event => "Event",
            InvocationType::DryRun => "DryRun",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InvokeParams {
    /// Function name, name:alias, name:version, partial ARN or full ARN.
    pub function_name: String,
    /// JSON event payload (default {}); serialized size at most 1 MiB.
    #[serde(default)]
    pub payload: Option<Value>,
    /// RequestResponse (default), Event, or DryRun.
    #[serde(default)]
    pub invocation_type: Option<InvocationType>,
    /// Version number or alias to invoke (alternative to name:alias).
    #[serde(default)]
    pub qualifier: Option<String>,
    /// Include the last 4 KB of execution log (default true; synchronous
    /// invocations only).
    #[serde(default)]
    pub log_tail: Option<bool>,
    /// The function's region. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

/// CloudWatch Logs log group storage class.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub enum LogGroupClass {
    #[serde(rename = "STANDARD")]
    Standard,
    #[serde(rename = "INFREQUENT_ACCESS")]
    InfrequentAccess,
    #[serde(rename = "DELIVERY")]
    Delivery,
}

impl LogGroupClass {
    fn as_str(self) -> &'static str {
        match self {
            LogGroupClass::Standard => "STANDARD",
            LogGroupClass::InfrequentAccess => "INFREQUENT_ACCESS",
            LogGroupClass::Delivery => "DELIVERY",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeLogGroupsParams {
    /// Only log groups whose name starts with this prefix (e.g.
    /// "/aws/lambda/"). Mutually exclusive with pattern.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Case-sensitive substring match on the name. Mutually exclusive with
    /// prefix; results then carry only name/arn/creation time.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Groups per page, 1..50 (default 50; clamped).
    #[serde(default)]
    pub limit: Option<i64>,
    /// `next_token` from the previous page.
    #[serde(default)]
    pub next_token: Option<String>,
    /// Restrict to one storage class.
    #[serde(default)]
    pub log_group_class: Option<LogGroupClass>,
    /// Region to search. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FilterLogEventsParams {
    /// Log group name (e.g. "/aws/lambda/my-fn") or its ARN.
    pub log_group: String,
    /// CloudWatch filter pattern, at most 1024 chars: terms, "quoted
    /// phrases", ?A ?B (any of), -exclude, or JSON {$.level = "error"}.
    /// Omit to match every event.
    #[serde(default)]
    pub filter_pattern: Option<String>,
    /// Window start: epoch milliseconds (numbers below 1e11 are taken as
    /// seconds) or an RFC 3339 string like "2024-05-01T12:00:00Z".
    #[serde(default)]
    pub start_time: Option<Value>,
    /// Window end, same formats as start_time.
    #[serde(default)]
    pub end_time: Option<Value>,
    /// Convenience: start_time = now minus this many minutes, 1..10080
    /// (ignored when start_time is given).
    #[serde(default)]
    pub last_minutes: Option<i64>,
    /// Events per page, 1..10000 (default 100; clamped). Pages may hold
    /// fewer — even zero — events while next_token is present.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Only streams whose name starts with this. Mutually exclusive with
    /// log_stream_names.
    #[serde(default)]
    pub log_stream_name_prefix: Option<String>,
    /// Exact stream names, at most 100. Mutually exclusive with
    /// log_stream_name_prefix.
    #[serde(default)]
    pub log_stream_names: Option<Vec<String>>,
    /// `next_token` from the previous page (expires after 24 h).
    #[serde(default)]
    pub next_token: Option<String>,
    /// true returns newest events first (needs start_time on or after
    /// 2024-01-01). Default false = oldest first.
    #[serde(default)]
    pub newest_first: Option<bool>,
    /// Region of the log group. Defaults to AWS_REGION.
    #[serde(default)]
    pub region: Option<String>,
}

/// Longest value accepted for prefixes, delimiters and filter values.
const MAX_PREFIX_BYTES: usize = 1024;
/// Delimiters are a character or two.
const MAX_DELIMITER_BYTES: usize = 16;
/// A cached clock correction at least this large is called out by
/// `check_auth` (AWS tolerates 15 minutes; anything cached at all means a
/// request was already rejected once).
const CLOCK_SKEW_NOTE_SECS: i64 = 60;

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

    /// Validate the configured credentials with STS GetCallerIdentity and
    /// report the write policy.
    #[tool(
        description = "Check the AWS credentials this server is configured with: calls STS \
                       GetCallerIdentity (no IAM permission needed) and reports status \
                       (ok/missing/invalid), the account, ARN and user id, whether the keys \
                       are long-term or temporary, the default region and whether writes are \
                       enabled. Call this first in a session; never retry a missing/invalid \
                       result — relay the remediation."
    )]
    #[tracing::instrument(name = "tool.check_auth", skip(self))]
    async fn check_auth(&self) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, None) {
            Ok(region) => region,
            Err(err) => return Ok(auth_report(&cfg, "error", None, err.message(), true)),
        };
        if !cfg.configured() {
            let err = cfg.credentials().err().unwrap_or(Error::MissingCredential {
                env: aws::ACCESS_KEY_ENV,
                reference: aws::ACCESS_KEY_REF,
            });
            return Ok(auth_report(&cfg, "missing", None, err.message(), true));
        }
        match aws::get_caller_identity(&cfg, &region).await {
            Ok(identity) => Ok(auth_report(
                &cfg,
                "ok",
                Some(identity),
                String::new(),
                false,
            )),
            Err(err) if err.is_credential_rejection() => {
                Ok(auth_report(&cfg, "invalid", None, err.message(), true))
            }
            Err(err) => Ok(auth_report(&cfg, "error", None, err.message(), true)),
        }
    }

    /// The raw identity call with a region override.
    #[tool(
        description = "STS GetCallerIdentity: the account id, ARN and user id of the \
                       configured principal, plus whether the credentials are long-term \
                       (AKIA…) or temporary (ASIA… + session token). Needs no IAM permission, \
                       so it isolates credential/region/clock problems from authorization \
                       problems."
    )]
    #[tracing::instrument(name = "tool.sts_get_caller_identity", skip(self))]
    async fn sts_get_caller_identity(
        &self,
        Parameters(params): Parameters<RegionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        match aws::get_caller_identity(&cfg, &region).await {
            Ok(mut identity) => {
                identity["region"] = Value::String(region.clone());
                identity["credential_type"] = Value::String(cfg.credential_type().to_owned());
                let text = format!(
                    "account {} — {} (user id {}), {} credentials, region {region}",
                    identity["account"].as_str().unwrap_or("?"),
                    identity["arn"].as_str().unwrap_or("?"),
                    identity["user_id"].as_str().unwrap_or("?"),
                    cfg.credential_type(),
                );
                Ok(shaped(text, identity))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// S3 ListBuckets.
    #[tool(
        description = "List the S3 buckets the account owns, with each bucket's region \
                       (call this before listing objects in a bucket in another region). \
                       Paginated with continuation_token; max_buckets is clamped to 1..10000."
    )]
    #[tracing::instrument(name = "tool.s3_list_buckets", skip(self))]
    async fn s3_list_buckets(
        &self,
        Parameters(params): Parameters<ListBucketsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let prefix = match opt_text("prefix", params.prefix, MAX_PREFIX_BYTES) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let continuation_token = match opt_text(
            "continuation_token",
            params.continuation_token,
            aws::MAX_TOKEN_LEN,
        ) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let bucket_region = match params.bucket_region.as_deref().map(str::trim) {
            Some(r) if !r.is_empty() => {
                if !aws::is_valid_region(r) {
                    return Ok(refuse(format!(
                        "bucket_region {:?} is not a region code (e.g. us-west-2)",
                        aws::cut(r, 40)
                    )));
                }
                Some(r.to_owned())
            }
            _ => None,
        };
        let opts = aws::ListBucketsOpts {
            prefix,
            max_buckets: clamp(params.max_buckets, 100, 1, 10_000),
            continuation_token,
            bucket_region,
        };
        match aws::list_buckets(&cfg, &region, opts).await {
            Ok(value) => {
                let names: Vec<String> = value["buckets"]
                    .as_array()
                    .map(|b| {
                        b.iter()
                            .take(20)
                            .map(|x| {
                                format!(
                                    "{} ({})",
                                    x["name"].as_str().unwrap_or("?"),
                                    x["region"].as_str().unwrap_or("region unknown")
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let more = if value["next_continuation_token"].is_string() {
                    " — more pages: pass next_continuation_token"
                } else {
                    ""
                };
                let text = format!(
                    "{} bucket(s): {}{more}",
                    value["count"],
                    if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    }
                );
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// S3 ListObjectsV2.
    #[tool(
        description = "List objects in an S3 bucket under a prefix, optionally collapsing \
                       'folders' with delimiter \"/\" into common_prefixes. Paginated with \
                       continuation_token (max_keys clamped to 1..1000). Pass the bucket's \
                       region; a wrong one fails with the right region named."
    )]
    #[tracing::instrument(name = "tool.s3_list_objects", skip(self))]
    async fn s3_list_objects(
        &self,
        Parameters(params): Parameters<ListObjectsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let bucket = params.bucket.trim().to_owned();
        if let Err(msg) = aws::validate_bucket(&bucket) {
            return Ok(refuse(msg));
        }
        let prefix = match opt_text("prefix", params.prefix, MAX_PREFIX_BYTES) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let delimiter = match opt_text("delimiter", params.delimiter, MAX_DELIMITER_BYTES) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let continuation_token = match opt_text(
            "continuation_token",
            params.continuation_token,
            aws::MAX_TOKEN_LEN,
        ) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let start_after = match opt_text("start_after", params.start_after, aws::MAX_KEY_BYTES) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let opts = aws::ListObjectsOpts {
            bucket,
            prefix,
            delimiter,
            max_keys: clamp(params.max_keys, 100, 1, 1000),
            continuation_token,
            start_after,
        };
        match aws::list_objects(&cfg, &region, opts).await {
            Ok(value) => {
                let objects = value["objects"].as_array().map(Vec::len).unwrap_or(0);
                let prefixes = value["common_prefixes"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0);
                let first: Vec<String> = value["objects"]
                    .as_array()
                    .map(|o| {
                        o.iter()
                            .take(10)
                            .map(|x| {
                                format!(
                                    "{} ({} B)",
                                    x["key"].as_str().unwrap_or("?"),
                                    x["size"].as_u64().unwrap_or(0)
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let more = if value["is_truncated"].as_bool().unwrap_or(false) {
                    " — truncated: pass next_continuation_token for the next page"
                } else {
                    ""
                };
                let text = format!(
                    "{objects} object(s) and {prefixes} common prefix(es) in {}{}{}{more}",
                    value["bucket"].as_str().unwrap_or("?"),
                    value["prefix"]
                        .as_str()
                        .map(|p| format!(" under {p:?}"))
                        .unwrap_or_default(),
                    if first.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", first.join(", "))
                    }
                );
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// S3 GetObject (text, byte-capped).
    #[tool(
        description = "Read a text object from S3 (up to max_bytes, default 64 KiB, max 1 MiB, \
                       via an HTTP Range request) with its metadata. Partial reads report \
                       truncated=true and the full content_length; non-UTF-8 objects return \
                       metadata only (binary=true). There is no binary download."
    )]
    #[tracing::instrument(name = "tool.s3_get_object", skip(self))]
    async fn s3_get_object(
        &self,
        Parameters(params): Parameters<GetObjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let bucket = params.bucket.trim().to_owned();
        if let Err(msg) = aws::validate_bucket(&bucket) {
            return Ok(refuse(msg));
        }
        if let Err(msg) = aws::validate_key(&params.key) {
            return Ok(refuse(msg));
        }
        let version_id = match opt_text("version_id", params.version_id, 1024) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let max_bytes = u64::from(clamp(
            params.max_bytes,
            aws::DEFAULT_GET_BYTES as u32,
            1,
            aws::MAX_GET_BYTES as u32,
        ));
        let opts = aws::GetObjectOpts {
            bucket,
            key: params.key,
            max_bytes,
            version_id,
        };
        match aws::get_object(&cfg, &region, opts).await {
            Ok(value) => {
                let text = if value["binary"].as_bool().unwrap_or(false) {
                    format!(
                        "s3://{}/{} is binary ({} bytes, {}); metadata only",
                        value["bucket"].as_str().unwrap_or("?"),
                        value["key"].as_str().unwrap_or("?"),
                        value["content_length"],
                        value["content_type"].as_str().unwrap_or("unknown type")
                    )
                } else {
                    format!(
                        "s3://{}/{} ({} of {} bytes{}, {}):\n{}",
                        value["bucket"].as_str().unwrap_or("?"),
                        value["key"].as_str().unwrap_or("?"),
                        value["returned_bytes"],
                        value["content_length"],
                        if value["truncated"].as_bool().unwrap_or(false) {
                            ", truncated"
                        } else {
                            ""
                        },
                        value["content_type"].as_str().unwrap_or("unknown type"),
                        value["body"].as_str().unwrap_or("")
                    )
                };
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// S3 PutObject (gated).
    #[tool(
        description = "Write a small UTF-8 text object to S3 (create or overwrite; at most \
                       1 MiB). Refused unless AWS_ALLOW_WRITES=true. if_none_match=true only \
                       creates and fails with 412 if the key exists."
    )]
    #[tracing::instrument(name = "tool.s3_put_object", skip(self, params))]
    async fn s3_put_object(
        &self,
        Parameters(params): Parameters<PutObjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        if !cfg.allow_writes {
            return Ok(tool_error(&Error::WritesDisabled {
                tool: "s3_put_object",
            }));
        }
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let bucket = params.bucket.trim().to_owned();
        if let Err(msg) = aws::validate_bucket(&bucket) {
            return Ok(refuse(msg));
        }
        if let Err(msg) = aws::validate_key(&params.key) {
            return Ok(refuse(msg));
        }
        if params.body.len() > aws::MAX_PUT_BYTES {
            return Ok(refuse(format!(
                "body is {} bytes; s3_put_object accepts at most {} bytes (1 MiB) — split the \
                 content or upload it another way",
                params.body.len(),
                aws::MAX_PUT_BYTES
            )));
        }
        let content_type = params
            .content_type
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "text/plain; charset=utf-8".to_owned());
        if content_type.len() > 256
            || !content_type.bytes().all(|b| (0x20..0x7f).contains(&b))
            || !content_type.contains('/')
        {
            return Ok(refuse(
                "content_type must be a printable ASCII media type like text/plain; charset=utf-8 \
                 or application/json (at most 256 characters)"
                    .to_owned(),
            ));
        }
        let opts = aws::PutObjectOpts {
            bucket,
            key: params.key,
            body: params.body,
            content_type,
            if_none_match: params.if_none_match.unwrap_or(false),
        };
        match aws::put_object(&cfg, &region, opts).await {
            Ok(value) => {
                let text = format!(
                    "wrote {} bytes to s3://{}/{} (etag {}{})",
                    value["bytes_written"],
                    value["bucket"].as_str().unwrap_or("?"),
                    value["key"].as_str().unwrap_or("?"),
                    value["etag"].as_str().unwrap_or("?"),
                    value["version_id"]
                        .as_str()
                        .map(|v| format!(", version {v}"))
                        .unwrap_or_default()
                );
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// EC2 DescribeInstances.
    #[tool(
        description = "List EC2 instances in a region with flattened fields (id, Name tag, \
                       state, type, AZ, IPs, launch time, VPC/subnet, tags). Filter by \
                       instance_ids or EC2 filters such as instance-state-name, tag:Name, \
                       vpc-id. Paginated with next_token (max_results clamped to 5..1000)."
    )]
    #[tracing::instrument(name = "tool.ec2_describe_instances", skip(self))]
    async fn ec2_describe_instances(
        &self,
        Parameters(params): Parameters<DescribeInstancesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let mut instance_ids = Vec::new();
        for id in params.instance_ids.unwrap_or_default() {
            let id = id.trim().to_owned();
            if let Err(msg) = aws::validate_instance_id(&id) {
                return Ok(refuse(msg));
            }
            instance_ids.push(id);
        }
        if instance_ids.len() > aws::MAX_INSTANCE_IDS {
            return Ok(refuse(format!(
                "at most {} instance_ids per call (got {}); page through them",
                aws::MAX_INSTANCE_IDS,
                instance_ids.len()
            )));
        }
        let mut filters: Vec<(String, Vec<String>)> = Vec::new();
        for (name, raw) in params.filters.unwrap_or_default() {
            if let Err(msg) = aws::validate_text("filter name", &name, 128) {
                return Ok(refuse(msg));
            }
            if name.is_empty() {
                return Ok(refuse("filter names must not be empty".to_owned()));
            }
            let values: Vec<String> = match raw {
                Value::String(s) => vec![s],
                Value::Array(items) => {
                    let mut out = Vec::new();
                    for item in items {
                        match item {
                            Value::String(s) => out.push(s),
                            Value::Number(n) => out.push(n.to_string()),
                            Value::Bool(b) => out.push(b.to_string()),
                            _ => {
                                return Ok(refuse(format!(
                                    "filter {name:?} values must be strings"
                                )))
                            }
                        }
                    }
                    out
                }
                Value::Number(n) => vec![n.to_string()],
                Value::Bool(b) => vec![b.to_string()],
                _ => {
                    return Ok(refuse(format!(
                        "filter {name:?} must be a string or an array of strings"
                    )))
                }
            };
            if values.is_empty() {
                return Ok(refuse(format!("filter {name:?} has no values")));
            }
            if values.len() > aws::MAX_FILTER_VALUES {
                return Ok(refuse(format!(
                    "filter {name:?} has {} values; at most {} are allowed",
                    values.len(),
                    aws::MAX_FILTER_VALUES
                )));
            }
            for value in &values {
                if let Err(msg) = aws::validate_text("filter value", value, 256) {
                    return Ok(refuse(msg));
                }
            }
            filters.push((name, values));
        }
        if filters.len() > aws::MAX_FILTERS {
            return Ok(refuse(format!(
                "at most {} filters per call (got {})",
                aws::MAX_FILTERS,
                filters.len()
            )));
        }
        let next_token = match opt_text("next_token", params.next_token, aws::MAX_TOKEN_LEN) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let opts = aws::DescribeInstancesOpts {
            instance_ids,
            filters,
            max_results: clamp(params.max_results, 100, 5, 1000),
            next_token,
        };
        match aws::describe_instances(&cfg, &region, opts).await {
            Ok(value) => {
                let lines: Vec<String> = value["instances"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .take(20)
                            .map(|i| {
                                format!(
                                    "{} {} {} {}{}",
                                    i["instance_id"].as_str().unwrap_or("?"),
                                    i["state"].as_str().unwrap_or("?"),
                                    i["type"].as_str().unwrap_or("?"),
                                    i["availability_zone"].as_str().unwrap_or("?"),
                                    i["name"]
                                        .as_str()
                                        .map(|n| format!(" name={n}"))
                                        .unwrap_or_default()
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let more = if value["next_token"].is_string() {
                    " — more pages: pass next_token"
                } else {
                    ""
                };
                let text = format!(
                    "{} instance(s) in {}{more}{}",
                    value["count"],
                    region,
                    if lines.is_empty() {
                        String::new()
                    } else {
                        format!(":\n{}", lines.join("\n"))
                    }
                );
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Lambda ListFunctions.
    #[tool(
        description = "List Lambda functions in a region with runtime, handler, memory, \
                       timeout, package type and last-modified summary. Paginated with marker \
                       / next_marker; Lambda returns at most 50 per page."
    )]
    #[tracing::instrument(name = "tool.lambda_list_functions", skip(self))]
    async fn lambda_list_functions(
        &self,
        Parameters(params): Parameters<ListFunctionsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let marker = match opt_text("marker", params.marker, aws::MAX_TOKEN_LEN) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let opts = aws::ListFunctionsOpts {
            max_items: clamp(params.max_items, 50, 1, 50),
            marker,
            include_versions: params.include_versions.unwrap_or(false),
        };
        match aws::list_functions(&cfg, &region, opts).await {
            Ok(value) => {
                let names: Vec<String> = value["functions"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .take(20)
                            .map(|f| {
                                format!(
                                    "{} ({}, {} MB, {} s)",
                                    f["name"].as_str().unwrap_or("?"),
                                    f["runtime"].as_str().unwrap_or("image"),
                                    f["memory_mb"],
                                    f["timeout_s"]
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let more = if value["next_marker"].is_string() {
                    " — more pages: pass next_marker as marker"
                } else {
                    ""
                };
                let text = format!(
                    "{} function(s) in {region}{}{more}",
                    value["count"],
                    if names.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", names.join(", "))
                    }
                );
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Lambda Invoke (gated except DryRun).
    #[tool(
        description = "Invoke a Lambda function: RequestResponse (synchronous, returns the \
                       payload, function_error and the decoded log tail), Event (asynchronous, \
                       202, no payload) or DryRun (permission check only, 204). \
                       RequestResponse and Event are refused unless AWS_ALLOW_WRITES=true; \
                       DryRun is never gated. A function that throws is HTTP 200 with \
                       function_error set and errorMessage/errorType in payload."
    )]
    #[tracing::instrument(name = "tool.lambda_invoke", skip(self, params))]
    async fn lambda_invoke(
        &self,
        Parameters(params): Parameters<InvokeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let invocation_type = params
            .invocation_type
            .unwrap_or(InvocationType::RequestResponse);
        if !matches!(invocation_type, InvocationType::DryRun) && !cfg.allow_writes {
            return Ok(tool_error(&Error::WritesDisabled {
                tool: "lambda_invoke",
            }));
        }
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let function_name = params.function_name.trim().to_owned();
        if let Err(msg) = aws::validate_function_name(&function_name) {
            return Ok(refuse(msg));
        }
        let qualifier = match opt_text("qualifier", params.qualifier, 128) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        if let Some(q) = &qualifier {
            if !q
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'$'))
            {
                return Ok(refuse(
                    "qualifier must be a version number or alias name ([A-Za-z0-9-_$])".to_owned(),
                ));
            }
        }
        let payload = params.payload.unwrap_or_else(|| json!({}));
        let serialized = payload.to_string();
        if serialized.len() > aws::MAX_INVOKE_PAYLOAD_BYTES {
            return Ok(refuse(format!(
                "payload serializes to {} bytes; lambda_invoke accepts at most {} bytes (1 MiB) — \
                 pass a reference (an S3 key) instead of the data",
                serialized.len(),
                aws::MAX_INVOKE_PAYLOAD_BYTES
            )));
        }
        let opts = aws::InvokeOpts {
            function_name,
            payload: Bytes::from(serialized),
            invocation_type: invocation_type.as_str(),
            qualifier,
            log_tail: params.log_tail.unwrap_or(true)
                && matches!(invocation_type, InvocationType::RequestResponse),
        };
        match aws::invoke(&cfg, &region, opts).await {
            Ok(value) => {
                let status = value["status_code"].as_u64().unwrap_or(0);
                let mut text = match invocation_type {
                    InvocationType::DryRun => format!(
                        "dry run ok (HTTP {status}): the credentials may invoke {}",
                        value["function_name"].as_str().unwrap_or("?")
                    ),
                    InvocationType::Event => format!(
                        "event accepted (HTTP {status}): {} will run asynchronously; no payload is returned",
                        value["function_name"].as_str().unwrap_or("?")
                    ),
                    InvocationType::RequestResponse => match value["function_error"].as_str() {
                        Some(kind) => format!(
                            "HTTP {status} but the function raised a {kind} error: {}",
                            value["payload"]
                        ),
                        None => format!(
                            "HTTP {status}, version {}: {}",
                            value["executed_version"].as_str().unwrap_or("?"),
                            value["payload"]
                        ),
                    },
                };
                if let Some(log) = value["log_tail"].as_str() {
                    text.push_str("\n--- log tail ---\n");
                    text.push_str(log);
                }
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// CloudWatch Logs DescribeLogGroups.
    #[tool(
        description = "Find CloudWatch log groups by name prefix (e.g. /aws/lambda/) or \
                       substring pattern, with retention, stored bytes and class. Paginated \
                       with next_token (limit clamped to 1..50)."
    )]
    #[tracing::instrument(name = "tool.cloudwatch_logs_describe_log_groups", skip(self))]
    async fn cloudwatch_logs_describe_log_groups(
        &self,
        Parameters(params): Parameters<DescribeLogGroupsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let prefix = match opt_text("prefix", params.prefix, 512) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let pattern = match opt_text("pattern", params.pattern, 512) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        if prefix.is_some() && pattern.is_some() {
            return Ok(refuse(
                "prefix and pattern are mutually exclusive (CloudWatch rejects both); drop one"
                    .to_owned(),
            ));
        }
        if let Some(p) = &prefix {
            if let Err(msg) = aws::validate_log_group(p) {
                return Ok(refuse(format!("prefix: {msg}")));
            }
        }
        let next_token = match opt_text("next_token", params.next_token, aws::MAX_TOKEN_LEN) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let opts = aws::DescribeLogGroupsOpts {
            prefix,
            pattern,
            limit: clamp(params.limit, 50, 1, 50),
            next_token,
            log_group_class: params.log_group_class.map(LogGroupClass::as_str),
        };
        match aws::describe_log_groups(&cfg, &region, opts).await {
            Ok(value) => {
                let names: Vec<String> = value["log_groups"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .take(20)
                            .map(|g| {
                                format!(
                                    "{} ({})",
                                    g["name"].as_str().unwrap_or("?"),
                                    g["retention_days"]
                                        .as_i64()
                                        .map(|d| format!("{d}-day retention"))
                                        .unwrap_or_else(|| "never expires".to_owned())
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let more = if value["next_token"].is_string() {
                    " — more pages: pass next_token"
                } else {
                    ""
                };
                let text = format!(
                    "{} log group(s) in {region}{}{more}",
                    value["count"],
                    if names.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", names.join(", "))
                    }
                );
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// CloudWatch Logs FilterLogEvents.
    #[tool(
        description = "Search or tail CloudWatch log events in one log group by filter \
                       pattern and time window (start_time/end_time or last_minutes), \
                       optionally restricted to log streams. Paginated with next_token; a \
                       page can be empty while next_token is present — keep paginating until \
                       it is absent. Throttled by AWS at 5 calls/s per account/region."
    )]
    #[tracing::instrument(name = "tool.cloudwatch_logs_filter_log_events", skip(self))]
    async fn cloudwatch_logs_filter_log_events(
        &self,
        Parameters(params): Parameters<FilterLogEventsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cfg = aws::config();
        let region = match aws::resolve_region(&cfg, params.region.as_deref()) {
            Ok(region) => region,
            Err(err) => return Ok(tool_error(&err)),
        };
        let log_group = params.log_group.trim().to_owned();
        if let Err(msg) = aws::validate_log_group(&log_group) {
            return Ok(refuse(msg));
        }
        let filter_pattern = match params.filter_pattern {
            Some(p) => {
                let chars = p.chars().count();
                if chars > aws::MAX_FILTER_PATTERN_CHARS {
                    return Ok(refuse(format!(
                        "filter_pattern is {chars} characters; CloudWatch accepts at most {} — \
                         shorten it (patterns are terms, \"phrases\", ?any, -exclude or JSON \
                         selectors, not regular expressions)",
                        aws::MAX_FILTER_PATTERN_CHARS
                    )));
                }
                if p.chars().any(|c| c.is_control() && c != '\t') {
                    return Ok(refuse(
                        "filter_pattern contains control characters".to_owned(),
                    ));
                }
                Some(p).filter(|p| !p.trim().is_empty())
            }
            None => None,
        };
        let start_time = match params.start_time.as_ref().map(parse_time) {
            Some(Ok(t)) => Some(t),
            Some(Err(msg)) => return Ok(refuse(format!("start_time: {msg}"))),
            None => match params.last_minutes {
                Some(minutes) => {
                    let minutes = minutes.clamp(1, 10_080);
                    Some(time::now_millis().saturating_sub(minutes.saturating_mul(60_000)))
                }
                None => None,
            },
        };
        let end_time = match params.end_time.as_ref().map(parse_time) {
            Some(Ok(t)) => Some(t),
            Some(Err(msg)) => return Ok(refuse(format!("end_time: {msg}"))),
            None => None,
        };
        if let (Some(start), Some(end)) = (start_time, end_time) {
            if end < start {
                return Ok(refuse(format!(
                    "end_time ({}) is before start_time ({}); swap them",
                    time::format_iso_millis(end),
                    time::format_iso_millis(start)
                )));
            }
        }
        let newest_first = params.newest_first.unwrap_or(false);
        if newest_first && start_time.is_none_or(|s| s < 1_704_067_200_000) {
            return Ok(refuse(
                "newest_first=true requires a start_time on or after 2024-01-01T00:00:00Z \
                 (CloudWatch rejects startFromHead=false otherwise); pass start_time or \
                 last_minutes, or drop newest_first"
                    .to_owned(),
            ));
        }
        let log_stream_name_prefix =
            match opt_text("log_stream_name_prefix", params.log_stream_name_prefix, 512) {
                Ok(v) => v,
                Err(msg) => return Ok(refuse(msg)),
            };
        let mut log_stream_names = Vec::new();
        for name in params.log_stream_names.unwrap_or_default() {
            let name = name.trim().to_owned();
            if name.is_empty()
                || name.len() > 512
                || name.contains([':', '*'])
                || name.chars().any(char::is_control)
            {
                return Ok(refuse(
                    "log_stream_names entries must be 1..512 characters without ':' or '*'"
                        .to_owned(),
                ));
            }
            log_stream_names.push(name);
        }
        if log_stream_names.len() > aws::MAX_STREAM_NAMES {
            return Ok(refuse(format!(
                "at most {} log_stream_names (got {})",
                aws::MAX_STREAM_NAMES,
                log_stream_names.len()
            )));
        }
        if log_stream_name_prefix.is_some() && !log_stream_names.is_empty() {
            return Ok(refuse(
                "log_stream_name_prefix and log_stream_names are mutually exclusive (CloudWatch \
                 rejects both); drop one"
                    .to_owned(),
            ));
        }
        if let Some(p) = &log_stream_name_prefix {
            if p.contains([':', '*']) {
                return Ok(refuse(
                    "log_stream_name_prefix must not contain ':' or '*'".to_owned(),
                ));
            }
        }
        let next_token = match opt_text("next_token", params.next_token, 4096) {
            Ok(v) => v,
            Err(msg) => return Ok(refuse(msg)),
        };
        let opts = aws::FilterLogEventsOpts {
            log_group,
            filter_pattern,
            start_time,
            end_time,
            limit: clamp(params.limit, 100, 1, 10_000),
            log_stream_name_prefix,
            log_stream_names,
            next_token,
            newest_first,
        };
        match aws::filter_log_events(&cfg, &region, opts).await {
            Ok(value) => {
                let lines: Vec<String> = value["events"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .take(50)
                            .map(|e| {
                                format!(
                                    "{} [{}] {}",
                                    e["time"].as_str().unwrap_or("?"),
                                    e["log_stream_name"].as_str().unwrap_or("?"),
                                    aws::cut(e["message"].as_str().unwrap_or("").trim_end(), 500)
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut text = format!(
                    "{} event(s) from {}",
                    value["count"],
                    value["log_group"].as_str().unwrap_or("?")
                );
                if value["next_token"].is_string() {
                    text.push_str(" — next_token present: keep paginating");
                }
                if let Some(note) = value["note"].as_str() {
                    text.push_str(" (");
                    text.push_str(note);
                    text.push(')');
                }
                if !lines.is_empty() {
                    text.push_str(":\n");
                    text.push_str(&lines.join("\n"));
                }
                Ok(shaped(text, value))
            }
            Err(err) => Ok(tool_error(&err)),
        }
    }

    /// Offline signer diagnostic.
    #[tool(
        description = "Offline SigV4 diagnostic: signs the four AWS-published S3 example \
                       requests with the documented example credentials and compares with the \
                       published signatures. Needs no credentials and no network. Use when AWS \
                       answers SignatureDoesNotMatch to tell a bad secret (selftest passes) from \
                       a signer bug (selftest fails)."
    )]
    #[tracing::instrument(name = "tool.sigv4_selftest", skip(self))]
    async fn sigv4_selftest(&self) -> Result<CallToolResult, ErrorData> {
        let cases = sigv4::self_test();
        let ok = cases.iter().all(|c| c.pass);
        let value = json!({
            "ok": ok,
            "cases": cases.iter().map(|c| json!({
                "name": c.name,
                "expected": c.expected,
                "computed": c.computed,
                "pass": c.pass,
            })).collect::<Vec<_>>(),
            "date_used": sigv4::EXAMPLE_DATE,
            "access_key_used": sigv4::EXAMPLE_ACCESS_KEY,
            "clock_offset_seconds": aws::clock_offset_secs(),
            "host_time_utc": time::format_amz_date(time::now_secs()),
        });
        let text = if ok {
            format!(
                "sigv4 selftest passed: all {} published signatures reproduced (clock offset {} s). \
                 If AWS still says SignatureDoesNotMatch, the registered secret is wrong.",
                cases.len(),
                aws::clock_offset_secs()
            )
        } else {
            format!(
                "sigv4 selftest FAILED: {}. This is a signer bug in {}; report it with this output.",
                cases
                    .iter()
                    .filter(|c| !c.pass)
                    .map(|c| format!("{} expected {} got {}", c.name, c.expected, c.computed))
                    .collect::<Vec<_>>()
                    .join("; "),
                env!("CARGO_PKG_NAME")
            )
        };
        let mut result = if ok {
            CallToolResult::structured(value)
        } else {
            CallToolResult::structured_error(value)
        };
        result.content = vec![ContentBlock::text(text)];
        Ok(result)
    }
}

/// Clamps an optional client integer into the upstream's documented range.
fn clamp(value: Option<i64>, default: u32, min: u32, max: u32) -> u32 {
    match value {
        Some(v) => u32::try_from(v.clamp(i64::from(min), i64::from(max))).unwrap_or(default),
        None => default,
    }
}

/// Trims an optional free-text argument, drops empty ones, bounds the rest.
fn opt_text(name: &str, value: Option<String>, max_bytes: usize) -> Result<Option<String>, String> {
    match value {
        Some(v) => {
            let v = v.trim().to_owned();
            if v.is_empty() {
                return Ok(None);
            }
            aws::validate_text(name, &v, max_bytes)?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

/// Epoch milliseconds from a JSON number (ms, or seconds when < 1e11) or an
/// RFC 3339 / epoch string.
fn parse_time(value: &Value) -> Result<i64, String> {
    const SECONDS_CUTOFF: i64 = 100_000_000_000;
    let scale = |n: i64| -> Result<i64, String> {
        if n < 0 {
            return Err("timestamps must not be negative".to_owned());
        }
        Ok(if n < SECONDS_CUTOFF {
            n.saturating_mul(1000)
        } else {
            n
        })
    };
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                scale(i)
            } else if let Some(f) = n.as_f64() {
                if !f.is_finite() || !(0.0..=9.0e15).contains(&f) {
                    return Err("timestamp out of range".to_owned());
                }
                scale(f as i64)
            } else {
                Err("timestamp out of range".to_owned())
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                return s
                    .parse::<i64>()
                    .map_err(|_| "timestamp out of range".to_owned())
                    .and_then(scale);
            }
            time::parse_rfc3339(s).ok_or_else(|| {
                format!(
                    "{:?} is neither epoch milliseconds nor an RFC 3339 timestamp like 2024-05-01T12:00:00Z",
                    aws::cut(s, 40)
                )
            })
        }
        _ => Err("must be a number (epoch ms) or an RFC 3339 string".to_owned()),
    }
}

/// A `check_auth` report. `is_error` marks the missing/invalid outcomes so a
/// client that ignores `structuredContent` still sees a failure.
fn auth_report(
    cfg: &Config,
    status: &str,
    identity: Option<Value>,
    remediation: String,
    is_error: bool,
) -> CallToolResult {
    let remediation = aws::redact_secrets(&remediation);
    let value = json!({
        "status": status,
        "identity": identity,
        "credential_type": cfg.credential_type(),
        "session_token_configured": cfg.session_token.is_some(),
        "region": cfg.region,
        "endpoint": cfg.endpoint_url.clone().unwrap_or_else(|| "https://<service>.<region>.amazonaws.com".to_owned()),
        "writes_enabled": cfg.allow_writes,
        "clock_offset_seconds": aws::clock_offset_secs(),
        "clock_skew_detected": aws::clock_offset_secs().abs() >= CLOCK_SKEW_NOTE_SECS,
        "secret_refs": {
            aws::ACCESS_KEY_ENV: aws::ACCESS_KEY_REF,
            aws::SECRET_KEY_ENV: aws::SECRET_KEY_REF,
            aws::SESSION_TOKEN_ENV: aws::SESSION_TOKEN_REF,
        },
        "obtain_url": aws::IAM_USERS_URL,
        "required_actions": aws::READ_ONLY_ACTIONS,
        "write_actions": aws::WRITE_ACTIONS,
        "remediation": if remediation.is_empty() { Value::Null } else { Value::String(remediation.clone()) },
    });
    let mut text = match status {
        "ok" => format!(
            "credentials ok: account {}, {} ({} credentials), default region {}, writes {}",
            identity
                .as_ref()
                .and_then(|i| i["account"].as_str())
                .unwrap_or("?"),
            identity
                .as_ref()
                .and_then(|i| i["arn"].as_str())
                .unwrap_or("?"),
            cfg.credential_type(),
            cfg.region,
            if cfg.allow_writes {
                "enabled"
            } else {
                "disabled"
            },
        ),
        other => format!("credential status: {other}"),
    };
    if !remediation.is_empty() {
        text.push_str(". ");
        text.push_str(&remediation);
    }
    // A cached correction means AWS already rejected one request for its
    // timestamp: say so, because the host clock (not the credentials) needs
    // fixing, and the correction is lost on every instance restart.
    let offset = aws::clock_offset_secs();
    if offset.abs() >= CLOCK_SKEW_NOTE_SECS {
        text.push_str(&format!(
            " Note: the host clock is {} s {} AWS time; the server corrects x-amz-date \
             automatically on this instance, but fix the host clock (chrony/timesyncd) and \
             restart the workload.",
            offset.abs(),
            if offset > 0 { "behind" } else { "ahead of" }
        ));
    }
    let mut result = if is_error {
        CallToolResult::structured_error(value)
    } else {
        CallToolResult::structured(value)
    };
    result.content = vec![ContentBlock::text(text)];
    result
}

/// A shaped success: `structuredContent` plus a readable text fallback.
fn shaped(text: String, value: Value) -> CallToolResult {
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

/// A tool-level refusal (argument validation) — no AWS call was made.
fn refuse(message: String) -> CallToolResult {
    let text = format!("invalid arguments: {message} (no AWS call was made)");
    let mut result = CallToolResult::structured_error(json!({
        "error": "InvalidArguments",
        "retryable": false,
        "message": text,
    }));
    result.content = vec![ContentBlock::text(text)];
    result
}

/// A tool-level error for a failed exchange or local policy refusal, with
/// the classification in `structuredContent` so agents can branch on it.
fn tool_error(err: &Error) -> CallToolResult {
    // Upstream text is redacted at the source (aws::classify); this is the
    // last line of defence before anything reaches the client.
    let message = aws::redact_secrets(&err.message());
    let mut result = CallToolResult::structured_error(json!({
        "error": err.code(),
        "http_status": err.status(),
        "retryable": err.retryable(),
        "message": message,
    }));
    result.content = vec![ContentBlock::text(message)];
    result
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
            "AWS MCP server running as a sandboxed WebAssembly component on Cosmonic \
             Desktop, signing every request with SigV4 from static credentials \
             (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY [/ AWS_SESSION_TOKEN] registered as \
             secrets, never passed as arguments). Tools: check_auth and \
             sts_get_caller_identity (verify credentials — call first), s3_list_buckets, \
             s3_list_objects, s3_get_object (text, byte-capped), s3_put_object (gated by \
             AWS_ALLOW_WRITES), ec2_describe_instances, lambda_list_functions, \
             lambda_invoke (gated except DryRun), cloudwatch_logs_describe_log_groups, \
             cloudwatch_logs_filter_log_events, and sigv4_selftest (offline signer check). \
             Everything except STS is region-scoped: AWS_REGION is only a default and every \
             tool takes a `region` override; S3 buckets live in exactly one region.\n\n\
             This server publishes skills — playbooks describing when and how to use its \
             tools, the error catalogue and the pagination rules per service. Read \
             `skill://index.json` for the catalog, then `skill://aws-cloud-mcp/SKILL.md`.",
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
