//! AWS client: configuration, SigV4-signed requests to STS, S3, EC2, Lambda
//! and CloudWatch Logs, response shaping and error normalization.
//!
//! Every call is plain HTTPS to `https://<service>.<region>.amazonaws.com`
//! (or `AWS_ENDPOINT_URL`), signed in [`crate::sigv4`]. The three wire
//! dialects AWS uses — Query/XML (STS, EC2), REST/XML (S3), JSON (Lambda
//! REST, CloudWatch Logs JSON-1.1) — are parsed here into plain JSON values;
//! [`crate::server`] holds the tool definitions and result rendering.
//!
//! Error bodies also come in three shapes (`<Error>`, `<ErrorResponse>`,
//! `<Response><Errors>`, `{"__type"}` / `x-amzn-ErrorType`) and are normalized
//! into one [`Error::Aws`] with an actionable message keyed on the AWS error
//! code, so agents see "what happened and what to do" rather than raw XML.
//!
//! Tool ideas follow the awslabs/mcp servers (Apache-2.0) — read-only by
//! default, explicit write consent, `FunctionError` + decoded log tail on
//! invoke — re-implemented here without any of their (Python/boto3) code.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};

use bytes::Bytes;
use serde_json::{json, Value};

use crate::sigv4::{self, time, Credentials, SigningRequest};

/// Env var carrying the access key id (`AKIA…` long-term, `ASIA…` temporary).
pub const ACCESS_KEY_ENV: &str = "AWS_ACCESS_KEY_ID";
/// Env var carrying the secret access key.
pub const SECRET_KEY_ENV: &str = "AWS_SECRET_ACCESS_KEY";
/// Env var carrying the session token of temporary credentials (optional).
pub const SESSION_TOKEN_ENV: &str = "AWS_SESSION_TOKEN";
/// Default region for endpoints and the credential scope.
pub const REGION_ENV: &str = "AWS_REGION";
/// `true` enables the mutating tools (`s3_put_object`, `lambda_invoke`).
pub const ALLOW_WRITES_ENV: &str = "AWS_ALLOW_WRITES";
/// Base URL override applied to every service (tests, MinIO/LocalStack).
pub const ENDPOINT_URL_ENV: &str = "AWS_ENDPOINT_URL";
/// Desktop secret reference that injects [`ACCESS_KEY_ENV`].
pub const ACCESS_KEY_REF: &str = "aws-cloud-mcp-access-key-id";
/// Desktop secret reference that injects [`SECRET_KEY_ENV`].
pub const SECRET_KEY_REF: &str = "aws-cloud-mcp-secret-access-key";
/// Desktop secret reference that injects [`SESSION_TOKEN_ENV`].
pub const SESSION_TOKEN_REF: &str = "aws-cloud-mcp-session-token";
/// Region used when `AWS_REGION` is unset.
pub const DEFAULT_REGION: &str = "us-east-1";
/// Where IAM access keys are created.
pub const IAM_USERS_URL: &str = "https://console.aws.amazon.com/iam/home#/users";

/// IAM actions the read-only tools need.
pub const READ_ONLY_ACTIONS: &[&str] = &[
    "sts:GetCallerIdentity",
    "s3:ListAllMyBuckets",
    "s3:ListBucket",
    "s3:GetObject",
    "ec2:DescribeInstances",
    "lambda:ListFunctions",
    "logs:DescribeLogGroups",
    "logs:FilterLogEvents",
];
/// IAM actions the gated write tools need on top.
pub const WRITE_ACTIONS: &[&str] = &["s3:PutObject", "lambda:InvokeFunction"];

/// S3 object keys are at most 1024 bytes.
pub const MAX_KEY_BYTES: usize = 1024;
/// Pagination tokens / markers we forward.
pub const MAX_TOKEN_LEN: usize = 2048;
/// Largest object body `s3_put_object` sends.
pub const MAX_PUT_BYTES: usize = 1024 * 1024;
/// Largest serialized payload `lambda_invoke` sends.
pub const MAX_INVOKE_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Largest object slice `s3_get_object` reads.
pub const MAX_GET_BYTES: u64 = 1024 * 1024;
/// Default object slice for `s3_get_object`.
pub const DEFAULT_GET_BYTES: u64 = 64 * 1024;
/// CloudWatch filter patterns are at most 1024 characters.
pub const MAX_FILTER_PATTERN_CHARS: usize = 1024;
/// `logStreamNames` takes at most 100 entries.
pub const MAX_STREAM_NAMES: usize = 100;
/// `InstanceId.N` entries per DescribeInstances call.
pub const MAX_INSTANCE_IDS: usize = 100;
/// `Filter.N` entries and values per filter.
pub const MAX_FILTERS: usize = 20;
pub const MAX_FILTER_VALUES: usize = 20;
/// Longest slice of an upstream body echoed into an error.
const SNIPPET_CHARS: usize = 500;
/// Tags copied per EC2 instance.
const MAX_TAGS: usize = 50;
/// Cap on list items shaped from one response (well above every page size).
const MAX_ITEMS: usize = 10_000;

/// Which AWS service a request goes to; also the credential-scope name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Sts,
    S3,
    Ec2,
    Lambda,
    Logs,
}

impl Service {
    /// Credential-scope / endpoint prefix.
    pub fn name(self) -> &'static str {
        match self {
            Service::Sts => "sts",
            Service::S3 => "s3",
            Service::Ec2 => "ec2",
            Service::Lambda => "lambda",
            Service::Logs => "logs",
        }
    }

    /// Human name for messages.
    pub fn display(self) -> &'static str {
        match self {
            Service::Sts => "STS",
            Service::S3 => "S3",
            Service::Ec2 => "EC2",
            Service::Lambda => "Lambda",
            Service::Logs => "CloudWatch Logs",
        }
    }
}

/// Runtime configuration, read from the environment on every call (cheap,
/// and the instance never holds a credential beyond the request).
#[derive(Clone)]
pub struct Config {
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub region: String,
    pub allow_writes: bool,
    pub endpoint_url: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("access_key_id", &self.access_key_id.is_some())
            .field("secret_access_key", &self.secret_access_key.is_some())
            .field("session_token", &self.session_token.is_some())
            .field("region", &self.region)
            .field("allow_writes", &self.allow_writes)
            .field("endpoint_url", &self.endpoint_url)
            .finish()
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

pub fn config() -> Config {
    let allow_writes = non_empty_env(ALLOW_WRITES_ENV)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    Config {
        access_key_id: non_empty_env(ACCESS_KEY_ENV),
        secret_access_key: non_empty_env(SECRET_KEY_ENV),
        session_token: non_empty_env(SESSION_TOKEN_ENV),
        region: non_empty_env(REGION_ENV).unwrap_or_else(|| DEFAULT_REGION.to_owned()),
        allow_writes,
        endpoint_url: non_empty_env(ENDPOINT_URL_ENV).map(|v| v.trim_end_matches('/').to_owned()),
    }
}

impl Config {
    /// The signing credentials, or the actionable missing-secret error.
    pub fn credentials(&self) -> Result<Credentials, Error> {
        let access_key_id = self.access_key_id.clone().ok_or(Error::MissingCredential {
            env: ACCESS_KEY_ENV,
            reference: ACCESS_KEY_REF,
        })?;
        let secret_access_key = self
            .secret_access_key
            .clone()
            .ok_or(Error::MissingCredential {
                env: SECRET_KEY_ENV,
                reference: SECRET_KEY_REF,
            })?;
        Ok(Credentials {
            access_key_id,
            secret_access_key,
            session_token: self.session_token.clone(),
        })
    }

    /// `long-term` for `AKIA…` keys, `temporary` for `ASIA…` keys or when a
    /// session token is configured, `unknown` otherwise.
    pub fn credential_type(&self) -> &'static str {
        match self.access_key_id.as_deref() {
            Some(key) if key.starts_with("ASIA") => "temporary",
            Some(_) if self.session_token.is_some() => "temporary",
            Some(key) if key.starts_with("AKIA") => "long-term",
            Some(_) => "unknown",
            None => "missing",
        }
    }

    /// Whether every required secret is present.
    pub fn configured(&self) -> bool {
        self.access_key_id.is_some() && self.secret_access_key.is_some()
    }
}

/// The `credentials` block of the `GET /` discovery document (presence only,
/// never values).
pub fn credentials_json() -> Value {
    let status = |present: bool| if present { "configured" } else { "missing" };
    let key = non_empty_env(ACCESS_KEY_ENV).is_some();
    let secret = non_empty_env(SECRET_KEY_ENV).is_some();
    let token = non_empty_env(SESSION_TOKEN_ENV).is_some();
    json!([
        {
            "ref": ACCESS_KEY_REF,
            "env": ACCESS_KEY_ENV,
            "kind": "aws-access-key-id",
            "status": status(key),
            "required": true,
            "description": "IAM access key id (AKIA… long-term or ASIA… temporary) of a principal with the read-only actions below; SigV4-signed in the component",
            "obtainUrl": IAM_USERS_URL,
            "scopes": READ_ONLY_ACTIONS,
            "optionalScopes": WRITE_ACTIONS,
            "validate": "check_auth",
        },
        {
            "ref": SECRET_KEY_REF,
            "env": SECRET_KEY_ENV,
            "kind": "aws-secret-access-key",
            "status": status(secret),
            "required": true,
            "description": "Secret access key paired with the key id (shown once at creation)",
            "obtainUrl": IAM_USERS_URL,
            "scopes": READ_ONLY_ACTIONS,
            "validate": "check_auth",
        },
        {
            "ref": SESSION_TOKEN_REF,
            "env": SESSION_TOKEN_ENV,
            "kind": "aws-session-token",
            "status": status(token),
            "required": false,
            "description": "Session token for temporary credentials only (aws configure export-credentials --format env); must come from the same STS call as the key pair",
            "obtainUrl": "https://docs.aws.amazon.com/cli/latest/reference/configure/export-credentials.html",
            "scopes": [],
            "validate": "check_auth",
        }
    ])
}

/// The setup instruction shared by the missing- and invalid-credential
/// errors.
pub fn setup_hint() -> String {
    format!(
        "Create an access key in IAM ({IAM_USERS_URL} → user → Security credentials → Create \
         access key, use case CLI) for a principal allowed {reads} (plus {writes} only if \
         {ALLOW_WRITES_ENV}=true), or export temporary credentials with `aws configure \
         export-credentials --format env`. Register the values as secrets — paste them in \
         Cosmonic Desktop → Secrets or run cosmonic_set_secret name={ACCESS_KEY_REF} \
         uri=keychain://cosmonic/{ACCESS_KEY_REF} env={ACCESS_KEY_ENV} value=AKIA… and \
         cosmonic_set_secret name={SECRET_KEY_REF} uri=keychain://cosmonic/{SECRET_KEY_REF} \
         env={SECRET_KEY_ENV} value=… (temporary credentials also need \
         {SESSION_TOKEN_REF} / {SESSION_TOKEN_ENV}); list them under secretFrom in \
         deploy/workload.yaml, redeploy, then call check_auth. Never pass credentials as \
         tool arguments.",
        reads = READ_ONLY_ACTIONS.join(", "),
        writes = WRITE_ACTIONS.join(", "),
    )
}

/// `true` for region codes like `us-east-1`, `eu-west-2`, `us-gov-west-1`,
/// `us-isob-east-1`, `ap-southeast-3`.
pub fn is_valid_region(region: &str) -> bool {
    if region.len() < 8 || region.len() > 24 {
        return false;
    }
    if !region
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return false;
    }
    let parts: Vec<&str> = region.split('-').collect();
    if parts.len() != 3 && parts.len() != 4 {
        return false;
    }
    let first_ok = parts[0].len() == 2 && parts[0].bytes().all(|b| b.is_ascii_lowercase());
    let last_ok = !parts[parts.len() - 1].is_empty()
        && parts[parts.len() - 1].bytes().all(|b| b.is_ascii_digit());
    let middle_ok = parts[1..parts.len() - 1]
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()));
    let partition_ok = parts.len() == 3 || parts[1] == "gov" || parts[1].starts_with("iso");
    first_ok && last_ok && middle_ok && partition_ok
}

/// The region for one call: the tool's `region` argument when given, else
/// `AWS_REGION`; either must be a valid region code.
pub fn resolve_region(cfg: &Config, requested: Option<&str>) -> Result<String, Error> {
    match requested.map(str::trim).filter(|r| !r.is_empty()) {
        Some(region) => {
            if is_valid_region(region) {
                Ok(region.to_owned())
            } else {
                Err(Error::InvalidRegion {
                    value: cut(region, 64),
                    source: "the `region` argument",
                })
            }
        }
        None => {
            if is_valid_region(&cfg.region) {
                Ok(cfg.region.clone())
            } else {
                Err(Error::InvalidRegion {
                    value: cut(&cfg.region, 64),
                    source: REGION_ENV,
                })
            }
        }
    }
}

/// Clock correction learned from a `RequestTimeTooSkewed`-style response,
/// in seconds to add to the host clock. Survives across requests on a warm
/// instance; reset by restarting the workload.
static CLOCK_OFFSET_SECS: AtomicI64 = AtomicI64::new(0);

/// The currently cached clock correction.
pub fn clock_offset_secs() -> i64 {
    CLOCK_OFFSET_SECS.load(Ordering::Relaxed)
}

/// A failed AWS exchange (or a local refusal), already classified.
#[derive(Debug)]
pub enum Error {
    /// A required secret env var is unset — nothing was sent.
    MissingCredential {
        env: &'static str,
        reference: &'static str,
    },
    /// `AWS_REGION` or the `region` argument is not a region code.
    InvalidRegion { value: String, source: &'static str },
    /// `AWS_ENDPOINT_URL` is not `scheme://host[:port]`.
    InvalidEndpoint(String),
    /// A gated tool was called while `AWS_ALLOW_WRITES` is not `true`.
    WritesDisabled { tool: &'static str },
    /// AWS answered with a non-2xx status.
    Aws {
        service: Service,
        status: u16,
        code: String,
        message: String,
        request_id: Option<String>,
        bucket_region: Option<String>,
        retry_after: Option<u64>,
        detail: Option<String>,
    },
    /// The host could not complete the exchange (DNS, TLS, policy, timeout).
    Transport { service: Service, detail: String },
    /// A 2xx whose body is not what the service documents.
    Malformed { service: Service, detail: String },
}

const SKEW_CODES: &[&str] = &[
    "RequestTimeTooSkewed",
    "RequestExpired",
    "RequestInTheFuture",
];
const BAD_KEY_CODES: &[&str] = &[
    "InvalidClientTokenId",
    "InvalidAccessKeyId",
    "AuthFailure",
    "UnrecognizedClientException",
    "InvalidUserID.NotFound",
];
const BAD_SIGNATURE_CODES: &[&str] = &["SignatureDoesNotMatch", "InvalidSignatureException"];
const DENIED_CODES: &[&str] = &[
    "AccessDenied",
    "AccessDeniedException",
    "UnauthorizedOperation",
    "Forbidden",
    "OptInRequired",
];
const THROTTLE_CODES: &[&str] = &[
    "SlowDown",
    "Throttling",
    "ThrottlingException",
    "RequestLimitExceeded",
    "TooManyRequestsException",
    "TooManyRequests",
    "RequestThrottled",
    "RequestThrottledException",
    "LimitExceededException",
];
const TRANSIENT_CODES: &[&str] = &[
    "InternalError",
    "InternalFailure",
    "ServiceUnavailable",
    "ServiceUnavailableException",
    "ServiceException",
    "BadGateway",
    "GatewayTimeout",
];

impl Error {
    /// The AWS error code (or a local pseudo-code) for `structuredContent`.
    pub fn code(&self) -> String {
        match self {
            Error::MissingCredential { env, .. } => format!("MissingCredential:{env}"),
            Error::InvalidRegion { .. } => "InvalidRegion".to_owned(),
            Error::InvalidEndpoint(_) => "InvalidEndpoint".to_owned(),
            Error::WritesDisabled { .. } => "WritesDisabled".to_owned(),
            Error::Aws { code, .. } => code.clone(),
            Error::Transport { .. } => "Transport".to_owned(),
            Error::Malformed { .. } => "MalformedResponse".to_owned(),
        }
    }

    /// HTTP status of an AWS error, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Aws { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Whether waiting and retrying could succeed without a human acting.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Aws { status, code, .. } => {
                THROTTLE_CODES.contains(&code.as_str())
                    || TRANSIENT_CODES.contains(&code.as_str())
                    || *status == 429
                    || (*status >= 500 && *status != 501)
            }
            Error::Transport { .. } => true,
            _ => false,
        }
    }

    /// Whether the request was rejected for a timestamp outside AWS's
    /// 15-minute window (the one error the client corrects and retries).
    pub fn is_clock_skew(&self) -> bool {
        match self {
            Error::Aws { code, message, .. } => {
                // STS/EC2 (Query) say `SignatureDoesNotMatch: Signature
                // expired: <x-amz-date> is now earlier than <now - 15 min>`,
                // JSON services say `InvalidSignatureException: Signature
                // expired` / `Signature not yet current`; S3 has its own code.
                let lower = message.to_ascii_lowercase();
                SKEW_CODES.contains(&code.as_str())
                    || (BAD_SIGNATURE_CODES.contains(&code.as_str())
                        && (lower.contains("expired")
                            || lower.contains("not yet current")
                            || lower.contains("earlier than")
                            || lower.contains("later than")))
            }
            _ => false,
        }
    }

    /// Whether the failure means the credentials themselves are rejected
    /// (as opposed to a missing permission) — `check_auth` reports `invalid`.
    pub fn is_credential_rejection(&self) -> bool {
        match self {
            Error::Aws { code, .. } => {
                BAD_KEY_CODES.contains(&code.as_str())
                    || (BAD_SIGNATURE_CODES.contains(&code.as_str()) && !self.is_clock_skew())
                    || code.starts_with("ExpiredToken")
                    || code == "InvalidToken"
                    || code == "TokenRefreshRequired"
            }
            _ => false,
        }
    }

    /// The caller-facing message: what happened and what to do about it.
    pub fn message(&self) -> String {
        match self {
            Error::MissingCredential { env, reference } => format!(
                "{env} is not set (secret ref `{reference}` not registered or not listed under \
                 secretFrom). {}",
                setup_hint()
            ),
            Error::InvalidRegion { value, source } => format!(
                "invalid region {value:?} from {source}: expected a region code such as \
                 us-east-1, eu-west-2 or us-gov-west-1 (pattern \
                 ^[a-z]{{2}}(-gov|-iso[a-z]?)?-[a-z]+-\\d$). Fix {source} and retry."
            ),
            Error::InvalidEndpoint(value) => format!(
                "{ENDPOINT_URL_ENV} is not a usable base URL ({value:?}): expected \
                 scheme://host[:port] with no path, e.g. http://host.wasmcloud.internal:9000 \
                 for a local S3-compatible store (its host must also be in allowedHosts)."
            ),
            Error::WritesDisabled { tool } => format!(
                "writes are disabled: {tool} is refused because {ALLOW_WRITES_ENV} is not \
                 \"true\" (no AWS call was made). Set {ALLOW_WRITES_ENV}: \"true\" in \
                 localResources.environment.config of deploy/workload.yaml and redeploy — \
                 ideally a separate write-enabled workload with a narrower IAM policy. \
                 lambda_invoke with invocation_type=DryRun stays available for permission \
                 checks."
            ),
            Error::Aws {
                service,
                status,
                code,
                message,
                request_id,
                detail,
                ..
            } => {
                let mut text = format!(
                    "AWS {} returned HTTP {status} {code}: {message}",
                    service.display()
                );
                if let Some(id) = request_id {
                    text.push_str(&format!(" (request id {id})"));
                }
                text.push_str(". ");
                text.push_str(&self.advice());
                if let Some(detail) = detail {
                    text.push(' ');
                    text.push_str(detail);
                }
                text
            }
            Error::Transport { service, detail } => format!(
                "could not reach AWS {} : {detail}. If this mentions HttpRequestDenied or a \
                 policy, the workload's allowedHosts must include the endpoint host \
                 (*.amazonaws.com, or the {ENDPOINT_URL_ENV} host plus the loopback grant for a \
                 local store); DNS/TLS failures mean the host is unreachable or uses a private \
                 CA, which this sandbox cannot trust. A timeout is worth one retry.",
                service.display()
            ),
            Error::Malformed { service, detail } => format!(
                "AWS {} returned something that is not the documented response shape: \
                 {detail}. Check {ENDPOINT_URL_ENV} (an S3-compatible store or a proxy may \
                 differ) and retry once.",
                service.display()
            ),
        }
    }

    fn advice(&self) -> String {
        let Error::Aws {
            service,
            status,
            code,
            message,
            bucket_region,
            retry_after,
            ..
        } = self
        else {
            return String::new();
        };
        let code = code.as_str();
        let lower = message.to_ascii_lowercase();
        if self.is_clock_skew() {
            return format!(
                "The request timestamp differs from AWS time by more than 15 minutes. The \
                 server read the response Date header, cached the offset (now {} s) and \
                 retried once; if this persists, fix the host clock (chrony/timesyncd) and \
                 restart the workload to reset the cached offset.",
                clock_offset_secs()
            );
        }
        if BAD_KEY_CODES.contains(&code) {
            if lower.contains("security token") && non_empty_env(SESSION_TOKEN_ENV).is_some() {
                return format!(
                    "The session token does not belong to this key pair (mixed sources, or a \
                     stale {SESSION_TOKEN_REF} left next to long-term AKIA keys). Register all \
                     three values from the same STS/SSO export, or remove {SESSION_TOKEN_REF} \
                     from secretFrom when using long-term keys. {}",
                    setup_hint()
                );
            }
            return format!(
                "The access key id is unknown, deactivated, deleted, or from another partition \
                 (GovCloud/China keys do not work against amazonaws.com). Check IAM → Users → \
                 Security credentials that the key is Active, re-register {ACCESS_KEY_REF} \
                 (watch for pasted whitespace), then call check_auth. {}",
                setup_hint()
            );
        }
        if BAD_SIGNATURE_CODES.contains(&code) {
            return format!(
                "The secret access key does not match the key id, or the request was altered \
                 in transit. Run sigv4_selftest: if it passes, re-register {SECRET_KEY_REF} \
                 (a trailing newline, a truncated paste or the key id pasted as the secret are \
                 the usual causes); if it fails, the signer is broken — report the selftest \
                 output. Do not retry blindly."
            );
        }
        if code.starts_with("ExpiredToken") || code == "TokenRefreshRequired" {
            return format!(
                "The temporary credentials have expired and this server cannot refresh them. \
                 Re-run `aws configure export-credentials --format env` (or sts \
                 get-session-token / assume-role), re-register {ACCESS_KEY_REF}, \
                 {SECRET_KEY_REF} and {SESSION_TOKEN_REF} together, then redeploy or restart \
                 the workload."
            );
        }
        if code == "InvalidToken"
            || lower.contains("security token included in the request is invalid")
        {
            return format!(
                "{SESSION_TOKEN_ENV} does not belong to the key pair in use. Register all three \
                 values from the same STS call, or remove {SESSION_TOKEN_REF} from secretFrom \
                 when using long-term keys."
            );
        }
        if DENIED_CODES.contains(&code) {
            return format!(
                "The credentials are valid but the IAM policy does not allow this action on \
                 that resource (the message usually names the action). Attach it to the \
                 principal check_auth reports — read-only set: {}; writes: {}. Bucket policies \
                 and SCPs can also deny. Not retryable.",
                READ_ONLY_ACTIONS.join(", "),
                WRITE_ACTIONS.join(", ")
            );
        }
        if code == "PermanentRedirect"
            || code == "AuthorizationHeaderMalformed"
            || code == "IllegalLocationConstraintException"
            || *status == 301
        {
            let hint = bucket_region
                .as_deref()
                .map(|r| format!("the bucket is in region {r}; retry with region={r}"))
                .unwrap_or_else(|| {
                    "retry with the bucket's real region (s3_list_buckets shows it)".to_owned()
                });
            return format!(
                "The bucket lives in a different region than the endpoint used and path-style \
                 requests are never redirected: {hint}."
            );
        }
        if code == "NoSuchBucket" {
            return "No bucket with that name exists in this account (names are global and \
                    exact). Call s3_list_buckets to confirm the name and region; do not guess."
                .to_owned();
        }
        if code == "NoSuchKey" || (*service == Service::S3 && *status == 404) {
            return "No object with that key (keys are case-sensitive and exact; 'folders' are \
                    just prefixes). Use s3_list_objects with prefix/delimiter to find the exact \
                    key; do not guess names."
                .to_owned();
        }
        if code == "InvalidObjectState" {
            return "The object is in an archive tier (Glacier / Deep Archive / \
                    Intelligent-Tiering archive) and must be restored before it can be read. \
                    Not supported here: restore it with the console or `aws s3api \
                    restore-object` and retry hours later; listing still works."
                .to_owned();
        }
        if code == "InvalidRange" {
            return "The requested byte range is unsatisfiable (a zero-byte object). The \
                    server retries without Range automatically."
                .to_owned();
        }
        if THROTTLE_CODES.contains(&code) || *status == 429 {
            let wait = retry_after
                .map(|s| format!("Retry-After says {s} s; "))
                .unwrap_or_default();
            return format!(
                "Rate limit for this account/region/resource exceeded (FilterLogEvents is 5 \
                 TPS; EC2 Describe calls share a token bucket; S3 prefixes throttle with \
                 SlowDown). {wait}back off exponentially starting at 1-2 s, reduce page sizes \
                 and narrow time windows. This server does not auto-retry throttles."
            );
        }
        if code == "RequestTooLargeException" || code == "EntityTooLarge" {
            return "The payload is over the service limit (Lambda: 6 MB sync / 256 KB async; \
                    this server caps at 1 MiB). Send less data or pass a reference (an S3 key) \
                    instead."
                .to_owned();
        }
        if code == "InvalidInstanceID.Malformed" || code == "InvalidInstanceID.NotFound" {
            return "The instance id is not of the form i-xxxxxxxxxxxxxxxxx or does not exist \
                    in this region/account. Use filters (tag:Name, instance-state-name) instead \
                    of guessed ids, and check the region."
                .to_owned();
        }
        if code == "ResourceNotFoundException" {
            return "The function or log group does not exist in this region (names are \
                    case-sensitive; Lambda accepts name, name:alias, partial or full ARN). Use \
                    lambda_list_functions / cloudwatch_logs_describe_log_groups with a prefix to \
                    find the exact name, or pass region=… if it lives elsewhere."
                .to_owned();
        }
        if code == "XAmzContentSHA256Mismatch" || code == "BadDigest" {
            return "The signed payload hash does not match the bytes AWS received: something \
                    rewrote the body in transit or the signer is broken. Run sigv4_selftest; if \
                    it passes, look for an intercepting proxy on the path to S3 and report the \
                    request id."
                .to_owned();
        }
        if code == "PreconditionFailed" || code == "ConditionalRequestConflict" {
            return "The object already exists (if_none_match=true refuses overwrites) or a \
                    concurrent write raced. Omit if_none_match to overwrite deliberately, or \
                    choose another key."
                .to_owned();
        }
        if code == "SnapStartNotReadyException"
            || code == "ResourceConflictException"
            || code == "ResourceNotReadyException"
        {
            return "The function is initializing or being updated. Wait a few seconds and \
                    retry once."
                .to_owned();
        }
        if code.starts_with("InvalidParameter")
            || code == "ValidationException"
            || code == "ValidationError"
            || code == "InvalidRequestContentException"
            || code == "InvalidArgument"
            || code == "MalformedXML"
            || code == "MissingParameter"
            || code == "InvalidQueryParameter"
            || code == "UnknownParameter"
        {
            return "A parameter is wrong, out of range or mutually exclusive (CloudWatch \
                    filterPattern syntax errors also arrive this way). Follow the message and \
                    fix the call; retrying the same arguments will fail again."
                .to_owned();
        }
        if code == "NotImplemented" || code == "MethodNotAllowed" {
            return "The endpoint does not support this request shape (directory buckets, \
                    access points and some S3-compatible stores differ). Use a general-purpose \
                    bucket / a regional endpoint."
                .to_owned();
        }
        if TRANSIENT_CODES.contains(&code)
            || *status >= 500
            || code.starts_with("KMS")
            || code.starts_with("EC2")
            || code.starts_with("ENI")
            || code.starts_with("EFS")
        {
            return "AWS-side transient error or a Lambda environment problem (VPC ENI, KMS \
                    key), not a client problem. Retry once after a few seconds; if 502 \
                    KMS*/ENI*/EFS* errors persist, the function's configuration needs fixing \
                    in AWS."
                .to_owned();
        }
        format!(
            "See the AWS error reference for {code} on {}; do not retry blindly.",
            service.display()
        )
    }
}

/// One outbound AWS request before signing.
pub struct Request {
    pub service: Service,
    pub method: http::Method,
    /// Wire path, already URI-encoded per segment.
    pub path: String,
    /// Raw (unencoded) query pairs; encoded and sorted at send time.
    pub query: Vec<(String, String)>,
    /// Extra headers to send and sign.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Request {
    pub fn new(service: Service, method: http::Method, path: impl Into<String>) -> Self {
        Request {
            service,
            method,
            path: path.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    pub fn query(mut self, key: &str, value: impl Into<String>) -> Self {
        self.query.push((key.to_owned(), value.into()));
        self
    }

    pub fn query_opt(self, key: &str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.query(key, value),
            None => self,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    /// A form-encoded Query API body (`Action=…&Version=…&…`).
    pub fn form(self, pairs: &[(String, String)]) -> Self {
        let body = pairs
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    sigv4::uri_encode(k, true),
                    sigv4::uri_encode(v, true)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        self.body_with(
            Bytes::from(body),
            "application/x-www-form-urlencoded; charset=utf-8",
        )
    }

    pub fn body_with(mut self, body: Bytes, content_type: &str) -> Self {
        self.headers
            .push(("content-type".to_owned(), content_type.to_owned()));
        self.body = body;
        self
    }
}

/// A successful (2xx) AWS reply.
pub struct Response {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }
}

fn user_agent() -> String {
    format!(
        "{}/{} (Cosmonic Desktop; +https://github.com/cosmonic-labs/awesome-cosmonic)",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    )
}

/// `(base_url, host)` for a service in a region — the override when set,
/// else the regional AWS endpoint.
fn endpoint(cfg: &Config, service: Service, region: &str) -> Result<(String, String), Error> {
    match &cfg.endpoint_url {
        Some(url) => {
            let (scheme, rest) = url
                .split_once("://")
                .ok_or_else(|| Error::InvalidEndpoint(cut(url, 120)))?;
            if scheme != "http" && scheme != "https" {
                return Err(Error::InvalidEndpoint(cut(url, 120)));
            }
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            if host.is_empty()
                || host != rest
                || host.contains('@')
                || !host.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
                })
            {
                return Err(Error::InvalidEndpoint(cut(url, 120)));
            }
            Ok((format!("{scheme}://{host}"), host.to_owned()))
        }
        None => {
            let host = format!("{}.{region}.amazonaws.com", service.name());
            Ok((format!("https://{host}"), host))
        }
    }
}

/// Signs and performs one request, retrying exactly once with a corrected
/// clock when AWS rejects the timestamp.
pub async fn send(cfg: &Config, region: &str, request: &Request) -> Result<Response, Error> {
    let credentials = cfg.credentials()?;
    let (base, host) = endpoint(cfg, request.service, region)?;
    let query = sigv4::canonical_query(&request.query);
    let url = if query.is_empty() {
        format!("{base}{}", request.path)
    } else {
        format!("{base}{}?{query}", request.path)
    };
    let payload_hash = sigv4::sha256_hex(&request.body);
    let s3_style = request.service == Service::S3;

    let mut attempt = 0u8;
    loop {
        attempt += 1;
        let real_now = time::now_secs();
        let now = real_now.saturating_add(clock_offset_secs());
        let amz_date = time::format_amz_date(now);

        let mut headers: Vec<(String, String)> = Vec::with_capacity(request.headers.len() + 3);
        headers.push(("x-amz-date".to_owned(), amz_date.clone()));
        if s3_style {
            headers.push(("x-amz-content-sha256".to_owned(), payload_hash.clone()));
        }
        if let Some(token) = &credentials.session_token {
            headers.push(("x-amz-security-token".to_owned(), token.clone()));
        }
        headers.extend(request.headers.iter().cloned());

        let signed = sigv4::sign(
            &SigningRequest {
                method: request.method.as_str(),
                host: &host,
                raw_path: &request.path,
                canonical_query: &query,
                headers: &headers,
                payload_hash: &payload_hash,
                service: request.service.name(),
                region,
                amz_date: &amz_date,
                s3_style,
            },
            &credentials,
        );

        let mut builder = http::Request::builder()
            .method(request.method.clone())
            .uri(&url)
            .header("authorization", signed.authorization.as_str())
            .header("user-agent", user_agent());
        if !request.body.is_empty() {
            builder = builder.header("content-length", request.body.len());
        }
        for (name, value) in &headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let outbound = builder
            .body(request.body.clone())
            .map_err(|err| Error::Transport {
                service: request.service,
                detail: format!("could not build request: {err}"),
            })?;

        tracing::info!(
            service = request.service.name(),
            method = %request.method,
            path = %request.path,
            region,
            attempt,
            "aws request"
        );
        let response = crate::bridge::outbound::fetch(outbound)
            .await
            .map_err(|err| Error::Transport {
                service: request.service,
                detail: err.to_string(),
            })?;
        let status = response.status().as_u16();
        let (parts, body) = response.into_parts();
        let response = Response {
            status,
            headers: parts.headers,
            body,
        };
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let error = classify(request.service, &response);
        if attempt == 1 && error.is_clock_skew() {
            // A proxy may prepend its own Date; AWS's is the last one.
            if let Some(server_secs) = response
                .headers
                .get_all("date")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .filter_map(time::parse_imf_fixdate)
                .next_back()
            {
                let offset = server_secs.saturating_sub(real_now);
                CLOCK_OFFSET_SECS.store(offset, Ordering::Relaxed);
                tracing::warn!(
                    offset,
                    "AWS rejected the request timestamp; retrying with a corrected clock"
                );
                continue;
            }
        }
        tracing::warn!(
            service = request.service.name(),
            status,
            code = %error.code(),
            "aws error"
        );
        return Err(error);
    }
}

/// Builds [`Error::Aws`] from any non-2xx response, whatever the dialect.
fn classify(service: Service, response: &Response) -> Error {
    let status = response.status;
    let text = String::from_utf8_lossy(&response.body);
    let trimmed = text.trim_start();
    let mut code: Option<String> = None;
    let mut message: Option<String> = None;
    let mut request_id: Option<String> = None;
    let mut detail: Option<String> = None;

    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_slice::<Value>(&response.body) {
            code = ["__type", "code", "Code", "errorType"]
                .iter()
                .find_map(|k| value.get(*k).and_then(Value::as_str))
                .map(strip_error_type);
            message = ["message", "Message", "errorMessage"]
                .iter()
                .find_map(|k| value.get(*k).and_then(Value::as_str))
                .map(str::to_owned);
        }
    } else if trimmed.starts_with('<') {
        if let Ok(doc) = roxmltree::Document::parse(trimmed) {
            let find = |name: &str| {
                doc.descendants()
                    .find(|n| n.is_element() && n.tag_name().name() == name)
                    .and_then(|n| n.text())
                    .map(|t| t.trim().to_owned())
                    .filter(|t| !t.is_empty())
            };
            code = find("Code");
            message = find("Message");
            request_id = find("RequestId").or_else(|| find("RequestID"));
            if let Some(canonical) = find("CanonicalRequest") {
                // S3 echoes the canonical request it computed, header block
                // included — redact the session token line before keeping it.
                detail = Some(format!(
                    "AWS computed this CanonicalRequest: {:?}",
                    cut(&redact_secrets(&canonical), 600)
                ));
            }
        }
    }
    // EC2/STS/Lambda/Logs put "The Canonical String for this request should
    // have been …" (again with the x-amz-security-token header) in the
    // message itself; never let credential material reach the client.
    let message = message.map(|m| redact_secrets(&m));
    if code.is_none() {
        code = response
            .header("x-amzn-errortype")
            .map(|h| strip_error_type(&h))
            .filter(|c| !c.is_empty());
    }
    if request_id.is_none() {
        request_id = response
            .header("x-amzn-requestid")
            .or_else(|| response.header("x-amz-request-id"));
    }
    let code = code
        .map(|c| cut(&c, 100))
        .unwrap_or_else(|| default_code(status).to_owned());
    let message = message
        .map(|m| cut(&m, SNIPPET_CHARS))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            let snippet = snippet(&response.body);
            if snippet.is_empty() {
                "(empty body)".to_owned()
            } else {
                snippet
            }
        });
    Error::Aws {
        service,
        status,
        code,
        message,
        request_id: request_id.map(|r| cut(&r, 100)),
        bucket_region: response.header("x-amz-bucket-region").map(|r| cut(&r, 32)),
        retry_after: response
            .header("retry-after")
            .and_then(|v| v.parse::<u64>().ok()),
        detail,
    }
}

/// `com.amazonaws.logs#ResourceNotFoundException` → `ResourceNotFoundException`;
/// `ResourceNotFoundException:http://…` → `ResourceNotFoundException`.
fn strip_error_type(raw: &str) -> String {
    let after_hash = raw.rsplit('#').next().unwrap_or(raw);
    let before_colon = after_hash.split(':').next().unwrap_or(after_hash);
    before_colon.trim().to_owned()
}

fn default_code(status: u16) -> &'static str {
    match status {
        301 | 307 | 308 => "PermanentRedirect",
        400 => "BadRequest",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "NotFound",
        405 => "MethodNotAllowed",
        409 => "Conflict",
        412 => "PreconditionFailed",
        413 => "RequestTooLargeException",
        416 => "InvalidRange",
        429 => "TooManyRequests",
        500 => "InternalError",
        502 => "BadGateway",
        503 => "ServiceUnavailable",
        504 => "GatewayTimeout",
        _ => "HttpError",
    }
}

fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    cut(&redact_secrets(text.trim()), SNIPPET_CHARS)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Removes credential material from text an upstream (or a proxy) may echo
/// back at us: the value of any `x-amz-security-token` header/query line in
/// a canonical request, and the configured session token and secret access
/// key wherever they appear verbatim. Everything that reaches a tool result
/// from an AWS error body passes through here before it is cut.
pub fn redact_secrets(text: &str) -> String {
    let mut out = redact_named_value(text, "x-amz-security-token:");
    out = redact_named_value(&out, "x-amz-security-token=");
    for env in [SESSION_TOKEN_ENV, SECRET_KEY_ENV] {
        if let Some(value) = non_empty_env(env) {
            // Short values would over-redact ordinary text; real secrets
            // are 40+ characters.
            if value.chars().count() >= 8 && out.contains(&value) {
                out = out.replace(&value, "<redacted>");
            }
        }
    }
    out
}

/// Replaces the run of token characters (`[A-Za-z0-9+/=_.-]`) that follows
/// every case-insensitive occurrence of `name` with `<redacted>`.
fn redact_named_value(text: &str, name: &str) -> String {
    // ASCII lowercasing keeps byte offsets identical, so indices found in
    // `lower` are valid char boundaries in `text`.
    let lower = text.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while let Some(found) = lower.get(pos..).and_then(|rest| rest.find(&name)) {
        let start = pos + found + name.len();
        out.push_str(text.get(pos..start).unwrap_or_default());
        let end = text
            .get(start..)
            .map(|rest| {
                rest.char_indices()
                    .find(|(_, c)| {
                        !(c.is_ascii_alphanumeric()
                            || matches!(c, '+' | '/' | '=' | '_' | '.' | '-'))
                    })
                    .map(|(i, _)| start + i)
                    .unwrap_or(text.len())
            })
            .unwrap_or(text.len());
        if end > start {
            out.push_str("<redacted>");
        }
        pos = end;
    }
    out.push_str(text.get(pos..).unwrap_or_default());
    out
}

/// Cuts `s` to at most `max_chars` characters (never mid-code-point).
pub fn cut(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// Cuts a string to at most `max_bytes` bytes on a char boundary; returns
/// the cut text and whether anything was removed.
pub fn cut_bytes(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_owned(), false);
    }
    let mut index = max_bytes;
    while !s.is_char_boundary(index) {
        index -= 1;
    }
    (s[..index].to_owned(), true)
}

// ---------------------------------------------------------------------------
// Argument validators (shared by the tools; each returns the refusal text).
// ---------------------------------------------------------------------------

fn has_control(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// Bucket names: 3..=63 characters of `a-z 0-9 . -` (uppercase tolerated for
/// legacy buckets), no slash, not starting/ending with `.` or `-`.
pub fn validate_bucket(bucket: &str) -> Result<(), String> {
    let n = bucket.chars().count();
    if !(3..=63).contains(&n) {
        return Err(format!(
            "bucket name must be 3..=63 characters (got {n}); use the exact name from s3_list_buckets"
        ));
    }
    if !bucket
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        || bucket.starts_with(['.', '-'])
        || bucket.ends_with(['.', '-'])
    {
        return Err(
            "bucket name may contain only letters, digits, '.' and '-', and must start and end \
             with a letter or digit (no ARNs, no s3:// prefix, no slashes)"
                .to_owned(),
        );
    }
    Ok(())
}

/// Object keys: 1..=1024 bytes, no control characters, no `.`/`..` segments.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("key must not be empty".to_owned());
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(format!(
            "key is {} bytes; S3 keys are at most {MAX_KEY_BYTES} bytes",
            key.len()
        ));
    }
    if has_control(key) {
        return Err("key contains control characters".to_owned());
    }
    if key.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(
            "key contains a '.' or '..' path segment; proxies normalize those and S3 cannot \
             address them reliably — use the exact key from s3_list_objects"
                .to_owned(),
        );
    }
    Ok(())
}

/// Free-text query values (prefixes, delimiters, tokens): bounded, no
/// control characters.
pub fn validate_text(name: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.len() > max_bytes {
        return Err(format!(
            "{name} is {} bytes; at most {max_bytes} are accepted",
            value.len()
        ));
    }
    if has_control(value) {
        return Err(format!("{name} contains control characters"));
    }
    Ok(())
}

/// EC2 instance ids: `i-` followed by 8 or 17 hex digits.
pub fn validate_instance_id(id: &str) -> Result<(), String> {
    let hex = id.strip_prefix("i-").unwrap_or("");
    let ok = (hex.len() == 8 || hex.len() == 17)
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "instance id {:?} is not of the form i-xxxxxxxx or i-xxxxxxxxxxxxxxxxx (lowercase hex)",
            cut(id, 40)
        ))
    }
}

/// Lambda function references: name, name:alias, partial or full ARN.
pub fn validate_function_name(name: &str) -> Result<(), String> {
    let n = name.chars().count();
    if !(1..=170).contains(&n) {
        return Err(format!(
            "function_name must be 1..=170 characters (got {n})"
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'$' | b'.'))
    {
        return Err(
            "function_name may contain only letters, digits, '-', '_', ':', '$' and '.' \
             (a name, name:alias, name:version, partial or full ARN)"
                .to_owned(),
        );
    }
    Ok(())
}

/// Log group names (`[\.\-_/#A-Za-z0-9]+`, ≤512) or log group ARNs (≤2048).
pub fn validate_log_group(group: &str) -> Result<(), String> {
    if group.is_empty() {
        return Err("log_group must not be empty".to_owned());
    }
    if group.starts_with("arn:") {
        if group.len() > 2048 || group.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(
                "log group ARN must be at most 2048 characters without whitespace".to_owned(),
            );
        }
        return Ok(());
    }
    if group.len() > 512 {
        return Err(format!(
            "log group name is {} bytes; at most 512 are allowed",
            group.len()
        ));
    }
    if !group
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'/' | b'#'))
    {
        return Err(
            "log group name may contain only letters, digits, '.', '-', '_', '/' and '#' \
             (or pass the log group ARN)"
                .to_owned(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn xml_child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn xml_children<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Vec<roxmltree::Node<'a, 'i>> {
    node.children()
        .filter(|c| c.is_element() && c.tag_name().name() == name)
        .take(MAX_ITEMS)
        .collect()
}

fn xml_text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    xml_child(node, name)
        .and_then(|c| c.text())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn parse_xml<'i>(service: Service, text: &'i str) -> Result<roxmltree::Document<'i>, Error> {
    roxmltree::Document::parse(text.trim_start()).map_err(|err| Error::Malformed {
        service,
        detail: format!("{err} (body starts with {:?})", snippet(text.as_bytes())),
    })
}

fn parse_json(service: Service, body: &[u8]) -> Result<Value, Error> {
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(body).map_err(|err| Error::Malformed {
        service,
        detail: format!("{err} (body starts with {:?})", snippet(body)),
    })
}

fn opt_u64(s: Option<String>) -> Option<u64> {
    s.and_then(|v| v.parse().ok())
}

// ---------------------------------------------------------------------------
// STS
// ---------------------------------------------------------------------------

/// `GetCallerIdentity`: `{account, arn, user_id, request_id}`.
pub async fn get_caller_identity(cfg: &Config, region: &str) -> Result<Value, Error> {
    let request = Request::new(Service::Sts, http::Method::POST, "/").form(&[
        ("Action".to_owned(), "GetCallerIdentity".to_owned()),
        ("Version".to_owned(), "2011-06-15".to_owned()),
    ]);
    let response = send(cfg, region, &request).await?;
    let text = String::from_utf8_lossy(&response.body);
    let doc = parse_xml(Service::Sts, &text)?;
    let root = doc.root_element();
    let result = xml_child(root, "GetCallerIdentityResult").unwrap_or(root);
    let account = xml_text(result, "Account");
    if account.is_none() {
        return Err(Error::Malformed {
            service: Service::Sts,
            detail: "no GetCallerIdentityResult/Account element".to_owned(),
        });
    }
    Ok(json!({
        "account": account,
        "arn": xml_text(result, "Arn"),
        "user_id": xml_text(result, "UserId"),
        "request_id": xml_child(root, "ResponseMetadata").and_then(|m| xml_text(m, "RequestId")),
    }))
}

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

pub struct ListBucketsOpts {
    pub prefix: Option<String>,
    pub max_buckets: u32,
    pub continuation_token: Option<String>,
    pub bucket_region: Option<String>,
}

/// `ListBuckets`: `{buckets: [{name, creation_date, region}], count, next_continuation_token, prefix}`.
pub async fn list_buckets(
    cfg: &Config,
    region: &str,
    opts: ListBucketsOpts,
) -> Result<Value, Error> {
    let request = Request::new(Service::S3, http::Method::GET, "/")
        .query("max-buckets", opts.max_buckets.to_string())
        .query_opt("prefix", opts.prefix)
        .query_opt("continuation-token", opts.continuation_token)
        .query_opt("bucket-region", opts.bucket_region);
    let response = send(cfg, region, &request).await?;
    let text = String::from_utf8_lossy(&response.body);
    let doc = parse_xml(Service::S3, &text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListAllMyBucketsResult" {
        return Err(Error::Malformed {
            service: Service::S3,
            detail: format!(
                "expected ListAllMyBucketsResult, got <{}>",
                root.tag_name().name()
            ),
        });
    }
    let buckets: Vec<Value> = xml_child(root, "Buckets")
        .map(|b| xml_children(b, "Bucket"))
        .unwrap_or_default()
        .into_iter()
        .map(|b| {
            json!({
                "name": xml_text(b, "Name"),
                "creation_date": xml_text(b, "CreationDate"),
                "region": xml_text(b, "BucketRegion"),
            })
        })
        .collect();
    Ok(json!({
        "buckets": buckets,
        "count": buckets.len(),
        "next_continuation_token": xml_text(root, "ContinuationToken"),
        "prefix": xml_text(root, "Prefix"),
        "endpoint_region": region,
    }))
}

pub struct ListObjectsOpts {
    pub bucket: String,
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
    pub max_keys: u32,
    pub continuation_token: Option<String>,
    pub start_after: Option<String>,
}

/// `ListObjectsV2` (path-style, regional endpoint).
pub async fn list_objects(
    cfg: &Config,
    region: &str,
    opts: ListObjectsOpts,
) -> Result<Value, Error> {
    let request = Request::new(
        Service::S3,
        http::Method::GET,
        format!("/{}", sigv4::uri_encode(&opts.bucket, true)),
    )
    .query("list-type", "2")
    .query("max-keys", opts.max_keys.to_string())
    .query_opt("prefix", opts.prefix)
    .query_opt("delimiter", opts.delimiter)
    .query_opt("continuation-token", opts.continuation_token)
    .query_opt("start-after", opts.start_after);
    let response = send(cfg, region, &request).await?;
    let text = String::from_utf8_lossy(&response.body);
    let doc = parse_xml(Service::S3, &text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListBucketResult" {
        return Err(Error::Malformed {
            service: Service::S3,
            detail: format!(
                "expected ListBucketResult, got <{}>",
                root.tag_name().name()
            ),
        });
    }
    let objects: Vec<Value> = xml_children(root, "Contents")
        .into_iter()
        .map(|c| {
            json!({
                "key": xml_text(c, "Key"),
                "size": opt_u64(xml_text(c, "Size")),
                "last_modified": xml_text(c, "LastModified"),
                "etag": xml_text(c, "ETag"),
                "storage_class": xml_text(c, "StorageClass"),
            })
        })
        .collect();
    let common_prefixes: Vec<Value> = xml_children(root, "CommonPrefixes")
        .into_iter()
        .filter_map(|p| xml_text(p, "Prefix"))
        .map(Value::String)
        .collect();
    let is_truncated = xml_text(root, "IsTruncated").is_some_and(|v| v == "true");
    Ok(json!({
        "bucket": opts.bucket,
        "prefix": xml_text(root, "Prefix"),
        "delimiter": xml_text(root, "Delimiter"),
        "objects": objects,
        "common_prefixes": common_prefixes,
        "key_count": opt_u64(xml_text(root, "KeyCount")).unwrap_or(objects.len() as u64),
        "is_truncated": is_truncated,
        "next_continuation_token": xml_text(root, "NextContinuationToken"),
        "region": region,
    }))
}

pub struct GetObjectOpts {
    pub bucket: String,
    pub key: String,
    pub max_bytes: u64,
    pub version_id: Option<String>,
}

/// `GetObject` with a byte range; text-only output.
pub async fn get_object(cfg: &Config, region: &str, opts: GetObjectOpts) -> Result<Value, Error> {
    let path = format!(
        "/{}/{}",
        sigv4::uri_encode(&opts.bucket, true),
        sigv4::encode_path(&opts.key)
    );
    let ranged = Request::new(Service::S3, http::Method::GET, path.clone())
        .query_opt("versionId", opts.version_id.clone())
        .header(
            "range",
            format!("bytes=0-{}", opts.max_bytes.saturating_sub(1)),
        );
    let response = match send(cfg, region, &ranged).await {
        Ok(response) => response,
        // A zero-byte object makes any range unsatisfiable: read it plainly.
        Err(Error::Aws { status: 416, .. }) => {
            let plain = Request::new(Service::S3, http::Method::GET, path)
                .query_opt("versionId", opts.version_id.clone());
            send(cfg, region, &plain).await?
        }
        Err(err) => return Err(err),
    };
    let returned = response.body.len() as u64;
    let total = response
        .header("content-range")
        .and_then(|cr| cr.rsplit('/').next().and_then(|t| t.parse::<u64>().ok()))
        .or_else(|| opt_u64(response.header("content-length")))
        .unwrap_or(returned)
        .max(returned);
    let truncated = returned < total;
    let mut out = json!({
        "bucket": opts.bucket,
        "key": opts.key,
        "content_type": response.header("content-type"),
        "content_length": total,
        "etag": response.header("etag"),
        "last_modified": response.header("last-modified"),
        "version_id": response.header("x-amz-version-id"),
        "truncated": truncated,
        "returned_bytes": returned,
        "region": region,
    });
    match std::str::from_utf8(&response.body) {
        Ok(text) => {
            out["body"] = Value::String(text.to_owned());
            out["binary"] = Value::Bool(false);
        }
        Err(err) if truncated && err.error_len().is_none() => {
            // The range cut a multi-byte character at the end; keep the
            // valid prefix.
            let valid = err.valid_up_to();
            out["body"] =
                Value::String(String::from_utf8_lossy(&response.body[..valid]).into_owned());
            out["binary"] = Value::Bool(false);
            out["returned_bytes"] = json!(valid);
        }
        Err(_) => {
            out["binary"] = Value::Bool(true);
            out["note"] = Value::String(
                "object is not UTF-8 text; metadata only (this server has no binary download)"
                    .to_owned(),
            );
        }
    }
    if truncated {
        out["note"] = Value::String(format!(
            "showing the first {returned} of {total} bytes; raise max_bytes (up to {MAX_GET_BYTES}) for more"
        ));
    }
    Ok(out)
}

pub struct PutObjectOpts {
    pub bucket: String,
    pub key: String,
    pub body: String,
    pub content_type: String,
    pub if_none_match: bool,
}

/// `PutObject` with the payload hash signed.
pub async fn put_object(cfg: &Config, region: &str, opts: PutObjectOpts) -> Result<Value, Error> {
    let path = format!(
        "/{}/{}",
        sigv4::uri_encode(&opts.bucket, true),
        sigv4::encode_path(&opts.key)
    );
    let bytes_written = opts.body.len();
    let mut request = Request::new(Service::S3, http::Method::PUT, path)
        .body_with(Bytes::from(opts.body), &opts.content_type);
    if opts.if_none_match {
        request = request.header("if-none-match", "*");
    }
    let response = send(cfg, region, &request).await?;
    Ok(json!({
        "bucket": opts.bucket,
        "key": opts.key,
        "etag": response.header("etag"),
        "version_id": response.header("x-amz-version-id"),
        "bytes_written": bytes_written,
        "content_type": opts.content_type,
        "region": region,
    }))
}

// ---------------------------------------------------------------------------
// EC2
// ---------------------------------------------------------------------------

pub struct DescribeInstancesOpts {
    pub instance_ids: Vec<String>,
    pub filters: Vec<(String, Vec<String>)>,
    /// Omitted automatically when `instance_ids` is non-empty.
    pub max_results: u32,
    pub next_token: Option<String>,
}

/// `DescribeInstances` flattened to agent-friendly instance records.
pub async fn describe_instances(
    cfg: &Config,
    region: &str,
    opts: DescribeInstancesOpts,
) -> Result<Value, Error> {
    let mut form: Vec<(String, String)> = vec![
        ("Action".to_owned(), "DescribeInstances".to_owned()),
        ("Version".to_owned(), "2016-11-15".to_owned()),
    ];
    for (i, id) in opts.instance_ids.iter().enumerate() {
        form.push((format!("InstanceId.{}", i + 1), id.clone()));
    }
    for (fi, (name, values)) in opts.filters.iter().enumerate() {
        form.push((format!("Filter.{}.Name", fi + 1), name.clone()));
        for (vi, value) in values.iter().enumerate() {
            form.push((format!("Filter.{}.Value.{}", fi + 1, vi + 1), value.clone()));
        }
    }
    if opts.instance_ids.is_empty() {
        form.push(("MaxResults".to_owned(), opts.max_results.to_string()));
    }
    if let Some(token) = opts.next_token {
        form.push(("NextToken".to_owned(), token));
    }
    let request = Request::new(Service::Ec2, http::Method::POST, "/").form(&form);
    let response = send(cfg, region, &request).await?;
    let text = String::from_utf8_lossy(&response.body);
    let doc = parse_xml(Service::Ec2, &text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "DescribeInstancesResponse" {
        return Err(Error::Malformed {
            service: Service::Ec2,
            detail: format!(
                "expected DescribeInstancesResponse, got <{}>",
                root.tag_name().name()
            ),
        });
    }
    let mut instances = Vec::new();
    for reservation in xml_child(root, "reservationSet")
        .map(|r| xml_children(r, "item"))
        .unwrap_or_default()
    {
        for inst in xml_child(reservation, "instancesSet")
            .map(|s| xml_children(s, "item"))
            .unwrap_or_default()
        {
            if instances.len() >= MAX_ITEMS {
                break;
            }
            let mut tags: BTreeMap<String, String> = BTreeMap::new();
            for tag in xml_child(inst, "tagSet")
                .map(|t| xml_children(t, "item"))
                .unwrap_or_default()
                .into_iter()
                .take(MAX_TAGS)
            {
                if let Some(key) = xml_text(tag, "key") {
                    tags.insert(
                        cut(&key, 128),
                        cut(&xml_text(tag, "value").unwrap_or_default(), 256),
                    );
                }
            }
            instances.push(json!({
                "instance_id": xml_text(inst, "instanceId"),
                "name": tags.get("Name"),
                "state": xml_child(inst, "instanceState").and_then(|s| xml_text(s, "name")),
                "type": xml_text(inst, "instanceType"),
                "availability_zone": xml_child(inst, "placement").and_then(|p| xml_text(p, "availabilityZone")),
                "private_ip": xml_text(inst, "privateIpAddress"),
                "public_ip": xml_text(inst, "ipAddress"),
                "launch_time": xml_text(inst, "launchTime"),
                "image_id": xml_text(inst, "imageId"),
                "vpc_id": xml_text(inst, "vpcId"),
                "subnet_id": xml_text(inst, "subnetId"),
                "reservation_id": xml_text(reservation, "reservationId"),
                "tags": tags,
            }));
        }
    }
    Ok(json!({
        "instances": instances,
        "count": instances.len(),
        "next_token": xml_text(root, "nextToken"),
        "region": region,
    }))
}

// ---------------------------------------------------------------------------
// Lambda
// ---------------------------------------------------------------------------

pub struct ListFunctionsOpts {
    pub max_items: u32,
    pub marker: Option<String>,
    pub include_versions: bool,
}

/// `ListFunctions` summarized.
pub async fn list_functions(
    cfg: &Config,
    region: &str,
    opts: ListFunctionsOpts,
) -> Result<Value, Error> {
    let mut request = Request::new(Service::Lambda, http::Method::GET, "/2015-03-31/functions")
        .query("MaxItems", opts.max_items.to_string())
        .query_opt("Marker", opts.marker);
    if opts.include_versions {
        request = request.query("FunctionVersion", "ALL");
    }
    let response = send(cfg, region, &request).await?;
    let body = parse_json(Service::Lambda, &response.body)?;
    let functions: Vec<Value> = body
        .get("Functions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(MAX_ITEMS)
                .map(|f| {
                    json!({
                        "name": f.get("FunctionName"),
                        "arn": f.get("FunctionArn"),
                        "runtime": f.get("Runtime"),
                        "handler": f.get("Handler"),
                        "memory_mb": f.get("MemorySize"),
                        "timeout_s": f.get("Timeout"),
                        "last_modified": f.get("LastModified"),
                        "description": f.get("Description"),
                        "package_type": f.get("PackageType"),
                        "architectures": f.get("Architectures"),
                        "version": f.get("Version"),
                        "code_size": f.get("CodeSize"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "functions": functions,
        "count": functions.len(),
        "next_marker": body.get("NextMarker").and_then(Value::as_str),
        "region": region,
    }))
}

pub struct InvokeOpts {
    pub function_name: String,
    pub payload: Bytes,
    /// `RequestResponse`, `Event` or `DryRun`.
    pub invocation_type: &'static str,
    pub qualifier: Option<String>,
    pub log_tail: bool,
}

/// `Invoke`: payload, function error and decoded tail log.
pub async fn invoke(cfg: &Config, region: &str, opts: InvokeOpts) -> Result<Value, Error> {
    let path = format!(
        "/2015-03-31/functions/{}/invocations",
        sigv4::uri_encode(&opts.function_name, true)
    );
    let mut request = Request::new(Service::Lambda, http::Method::POST, path)
        .query_opt("Qualifier", opts.qualifier)
        .header("x-amz-invocation-type", opts.invocation_type)
        .header(
            "x-amz-log-type",
            if opts.log_tail { "Tail" } else { "None" },
        );
    if !opts.payload.is_empty() {
        request = request.body_with(opts.payload, "application/json");
    }
    let response = send(cfg, region, &request).await?;
    let function_error = response.header("x-amz-function-error");
    let log_tail = response.header("x-amz-log-result").map(|encoded| {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map(|bytes| cut_bytes(&String::from_utf8_lossy(&bytes), 16 * 1024).0)
            .unwrap_or(encoded)
    });
    let (payload, payload_truncated) = if response.body.is_empty() {
        (Value::Null, false)
    } else {
        match serde_json::from_slice::<Value>(&response.body) {
            Ok(value) => (value, false),
            Err(_) => {
                let (text, truncated) = cut_bytes(
                    &String::from_utf8_lossy(&response.body),
                    MAX_INVOKE_PAYLOAD_BYTES,
                );
                (Value::String(text), truncated)
            }
        }
    };
    Ok(json!({
        "function_name": opts.function_name,
        "status_code": response.status,
        "invocation_type": opts.invocation_type,
        "executed_version": response.header("x-amz-executed-version"),
        "function_error": function_error,
        "payload": payload,
        "payload_truncated": payload_truncated,
        "log_tail": log_tail,
        "region": region,
    }))
}

// ---------------------------------------------------------------------------
// CloudWatch Logs
// ---------------------------------------------------------------------------

fn logs_request(target: &str, body: &Value) -> Request {
    Request::new(Service::Logs, http::Method::POST, "/")
        .header("x-amz-target", format!("Logs_20140328.{target}"))
        .body_with(Bytes::from(body.to_string()), "application/x-amz-json-1.1")
}

pub struct DescribeLogGroupsOpts {
    pub prefix: Option<String>,
    pub pattern: Option<String>,
    pub limit: u32,
    pub next_token: Option<String>,
    pub log_group_class: Option<&'static str>,
}

/// `DescribeLogGroups`.
pub async fn describe_log_groups(
    cfg: &Config,
    region: &str,
    opts: DescribeLogGroupsOpts,
) -> Result<Value, Error> {
    let mut body = json!({ "limit": opts.limit });
    if let Some(prefix) = opts.prefix {
        body["logGroupNamePrefix"] = Value::String(prefix);
    }
    if let Some(pattern) = opts.pattern {
        body["logGroupNamePattern"] = Value::String(pattern);
    }
    if let Some(token) = opts.next_token {
        body["nextToken"] = Value::String(token);
    }
    if let Some(class) = opts.log_group_class {
        body["logGroupClass"] = Value::String(class.to_owned());
    }
    let response = send(cfg, region, &logs_request("DescribeLogGroups", &body)).await?;
    let parsed = parse_json(Service::Logs, &response.body)?;
    let groups: Vec<Value> = parsed
        .get("logGroups")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(MAX_ITEMS)
                .map(|g| {
                    let created = g.get("creationTime").and_then(Value::as_i64);
                    json!({
                        "name": g.get("logGroupName"),
                        "arn": g.get("arn").or_else(|| g.get("logGroupArn")),
                        "creation_time": created,
                        "creation_time_iso": created.map(time::format_iso_millis),
                        "retention_days": g.get("retentionInDays"),
                        "stored_bytes": g.get("storedBytes"),
                        "class": g.get("logGroupClass"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "log_groups": groups,
        "count": groups.len(),
        "next_token": parsed.get("nextToken").and_then(Value::as_str),
        "region": region,
    }))
}

pub struct FilterLogEventsOpts {
    pub log_group: String,
    pub filter_pattern: Option<String>,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub limit: u32,
    pub log_stream_name_prefix: Option<String>,
    pub log_stream_names: Vec<String>,
    pub next_token: Option<String>,
    pub newest_first: bool,
}

/// `FilterLogEvents`.
pub async fn filter_log_events(
    cfg: &Config,
    region: &str,
    opts: FilterLogEventsOpts,
) -> Result<Value, Error> {
    let mut body = json!({ "limit": opts.limit });
    if opts.log_group.starts_with("arn:") {
        body["logGroupIdentifier"] = Value::String(opts.log_group.clone());
    } else {
        body["logGroupName"] = Value::String(opts.log_group.clone());
    }
    if let Some(pattern) = opts.filter_pattern {
        body["filterPattern"] = Value::String(pattern);
    }
    if let Some(start) = opts.start_time {
        body["startTime"] = json!(start);
    }
    if let Some(end) = opts.end_time {
        body["endTime"] = json!(end);
    }
    if let Some(prefix) = opts.log_stream_name_prefix {
        body["logStreamNamePrefix"] = Value::String(prefix);
    }
    if !opts.log_stream_names.is_empty() {
        body["logStreamNames"] = json!(opts.log_stream_names);
    }
    if let Some(token) = opts.next_token {
        body["nextToken"] = Value::String(token);
    }
    if opts.newest_first {
        body["startFromHead"] = Value::Bool(false);
    }
    let response = send(cfg, region, &logs_request("FilterLogEvents", &body)).await?;
    let parsed = parse_json(Service::Logs, &response.body)?;
    let events: Vec<Value> = parsed
        .get("events")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(MAX_ITEMS)
                .map(|e| {
                    let ts = e.get("timestamp").and_then(Value::as_i64);
                    json!({
                        "timestamp": ts,
                        "time": ts.map(time::format_iso_millis),
                        "log_stream_name": e.get("logStreamName"),
                        "message": e.get("message"),
                        "event_id": e.get("eventId"),
                        "ingestion_time": e.get("ingestionTime"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let next_token = parsed
        .get("nextToken")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let note = if events.is_empty() && next_token.is_some() {
        Some(
            "empty page with a next_token: keep paginating — FilterLogEvents pages are bounded \
             by 1 MB of scanned data / 10,000 events and may be empty before the window is \
             exhausted; pagination is finished only when next_token is absent",
        )
    } else {
        None
    };
    Ok(json!({
        "log_group": opts.log_group,
        "events": events,
        "count": events.len(),
        "next_token": next_token,
        "start_time": opts.start_time,
        "end_time": opts.end_time,
        "note": note,
        "region": region,
    }))
}
