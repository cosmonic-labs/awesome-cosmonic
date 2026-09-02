---
name: aws-cloud-mcp
description: Use when a task needs to read an AWS account with static IAM credentials — verify which account/principal the keys belong to, list S3 buckets and objects and read small text objects, list EC2 instances, list Lambda functions, search or tail CloudWatch log groups, and (only when writes are enabled) write a small S3 object or invoke a Lambda function — and when an AWS tool call failed and you need to know whether it is a credential, region, permission, throttling or clock problem and what to do about it.
---

# Using the aws-cloud-mcp MCP server

This server signs every request with **AWS Signature Version 4** inside a
sandboxed WebAssembly component on Cosmonic Desktop, using static credentials
registered as secrets (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional
`AWS_SESSION_TOKEN`). It is stateless apart from one per-instance cache — the
clock-offset correction described under `check_auth` item 6 — so nothing else
carries between calls, and the only network it has is HTTPS to
`*.amazonaws.com`. Credentials are never tool arguments; if a user offers keys
in chat, tell them to register the secrets instead (see
[Errors](references/ERRORS.md) → missing credential). Tool output never
contains credential material: AWS echoes the signed headers (including
`x-amz-security-token`) in `SignatureDoesNotMatch` bodies, and the server
redacts that value (`<redacted>`) before it reaches you.

Tool schemas come over `tools/list`; this playbook covers what the schemas
cannot tell you. Reference tables: [Tools](references/TOOLS.md),
[Errors](references/ERRORS.md), [SigV4 notes](references/SIGV4.md).

## Start here: `check_auth`

Call `check_auth` first in any session. It makes one STS `GetCallerIdentity`
call, which needs **no IAM permission**, so it isolates credential, region and
clock problems from authorization problems. It reports:

1. **`status`** — `ok`, `missing` (secret refs not registered), `invalid`
   (AWS rejected the key id, secret, or session token, or they expired) or
   `error` (transport / region config). `missing` and `invalid` come back as
   tool errors with a `remediation` naming the refs
   (`aws-cloud-mcp-access-key-id`, `aws-cloud-mcp-secret-access-key`,
   `aws-cloud-mcp-session-token`), the env vars, and how to get keys. **Never
   retry a `missing`/`invalid` result** — relay the remediation and stop.
2. **`identity.arn`** — which account you are in and whether this is an IAM
   user (`arn:aws:iam::…:user/…`) or a role session
   (`arn:aws:sts::…:assumed-role/…`, temporary credentials that will expire).
3. **`credential_type`** — `long-term` (AKIA…) or `temporary` (ASIA… /
   session token). Temporary credentials cannot be refreshed by this server.
4. **`writes_enabled`** — whether `s3_put_object` and `lambda_invoke`
   (RequestResponse/Event) will work. If `false`, do not promise a write; ask
   the operator to set `AWS_ALLOW_WRITES=true` and redeploy.
5. **`region`** — the default region. Everything except STS is region-scoped.
6. **`clock_skew_detected`** / **`clock_offset_seconds`** — `true` means AWS
   already rejected a request for its timestamp and the server is now
   correcting `x-amz-date` by that many seconds. Calls work, but tell the
   operator the **host clock** is wrong (the note in the text says by how
   much); the correction is per instance and lost on restart.

If `check_auth` says `invalid` with `SignatureDoesNotMatch`, run
`sigv4_selftest` (no credentials, no network): **pass** means the registered
secret is wrong (a trailing newline or the key id pasted as the secret are the
classic causes); **fail** means the signer is broken — report it.

## Regions: AWS_REGION is only a default

Every tool takes a `region` argument. S3 buckets live in exactly one region and
path-style requests to the wrong regional endpoint fail with **301
PermanentRedirect** (empty body) or **400 AuthorizationHeaderMalformed**; the
error text names the right region (`x-amz-bucket-region`) — retry with
`region=<that>`. `s3_list_buckets` returns each bucket's region, so list
buckets before listing objects. Lambda functions, log groups and instances in
another region simply **do not appear** (`ResourceNotFoundException` or empty
results) — that is a region problem, not a permission problem. GovCloud and
China partitions need matching keys and, for China, a different domain this
server does not dial.

## Sequencing that works

- **Explore an account**: `check_auth` → `s3_list_buckets` (note regions) →
  `ec2_describe_instances` / `lambda_list_functions` /
  `cloudwatch_logs_describe_log_groups` per region of interest.
- **Read a file in S3**: `s3_list_objects` with `prefix` and `delimiter="/"`
  to find the exact key (keys are case-sensitive; "folders" are prefixes) →
  `s3_get_object` with the key **verbatim** (spaces, unicode, `+`, `?` are
  encoded for you). It reads up to `max_bytes` (default 64 KiB, max 1 MiB)
  via an HTTP Range: `truncated=true` with the full `content_length` means
  the object is bigger; raise `max_bytes` only when needed. Non-UTF-8
  objects return metadata with `binary=true` — there is no download tool.
- **Debug a Lambda**: `lambda_list_functions` → `lambda_invoke` with
  `invocation_type=DryRun` (permission check, never gated, 204) → if writes
  are enabled, `RequestResponse` with a payload; a function that throws is
  **HTTP 200 with `function_error`** (`Handled`/`Unhandled`) and
  `errorMessage`/`errorType`/`stackTrace` in `payload` — not a transport or
  auth failure. `log_tail` carries the last 4 KB of execution log; for more,
  `cloudwatch_logs_filter_log_events` on `/aws/lambda/<function>` with
  `last_minutes=10`.
- **Tail logs**: `cloudwatch_logs_describe_log_groups` with `prefix` to get
  the exact name → `cloudwatch_logs_filter_log_events` with `last_minutes`
  (or `start_time`/`end_time`) and a `filter_pattern`. Narrow the window
  before widening the pattern: FilterLogEvents is throttled at **5 calls per
  second per account/region**.
- **Write a small text file**: `check_auth` (`writes_enabled`) →
  `s3_put_object` with `if_none_match=true` when you must not overwrite
  (412 PreconditionFailed means it exists).

## Pagination differs per service — never assume one shape

| Service | Token | Page size | Quirk |
|---|---|---|---|
| S3 buckets | `continuation_token` ↔ `next_continuation_token` | `max_buckets` 1..10000 | `bucket_region` must equal the endpoint region |
| S3 objects | `continuation_token` ↔ `next_continuation_token`, `is_truncated` | `max_keys` 1..1000 | common prefixes count against `max_keys`; `start_after` is an alternative to a token |
| EC2 | `next_token` | `max_results` 5..1000 | `max_results` is dropped when `instance_ids` are given (EC2 rejects the combination) |
| Lambda | `marker` ↔ `next_marker` | at most 50 per page | asking for more is silently capped |
| Logs groups | `next_token` | `limit` 1..50 | `pattern` results carry only name/arn/creation time |
| Logs events | `next_token` (expires after 24 h) | `limit` 1..10000 | pages are bounded by **1 MB scanned**; an **empty page with a `next_token` is normal** — keep paginating until `next_token` is absent |

All limits are clamped silently to the documented range; tokens are opaque and
must be passed back verbatim, never across tools.

## CloudWatch filter patterns

`filter_pattern` is CloudWatch syntax, not a regex: terms (`ERROR`), quoted
phrases (`"connection refused"`), `?A ?B` (any of), `-term` (exclude), and
JSON selectors (`{ $.level = "error" }`). Up to 1024 characters; longer or
malformed patterns are refused locally or come back as
`InvalidParameterException`. `newest_first=true` needs a `start_time` on or
after 2024-01-01 (CloudWatch rule, enforced locally). Timestamps are epoch
milliseconds; RFC 3339 strings are accepted and converted.

## Write gating and safety

`s3_put_object` and `lambda_invoke` (RequestResponse/Event) refuse locally
unless `AWS_ALLOW_WRITES=true` — no AWS call is made and the refusal is never
retryable. `DryRun` invokes are always allowed. `Event` invocations return
202 with no payload and Lambda may retry the function up to twice; do not
fire them in a loop. Keys in the Desktop keychain are exactly as powerful as
the IAM policy allows: recommend least privilege (the read-only action list is
in [TOOLS.md](references/TOOLS.md)).

## Reading errors

Every failure is a tool error (`isError: true`) whose `structuredContent` has
`error` (the AWS code or a local pseudo-code), `http_status`, `retryable` and
`message`. The same cause has different names per service:

| You see | It means | Do |
|---|---|---|
| `MissingCredential:…` | secret refs not registered | relay the remediation; never retry |
| `InvalidClientTokenId` / `InvalidAccessKeyId` / `AuthFailure` / `UnrecognizedClientException` | key id unknown or inactive (or a stale session token) | fix `aws-cloud-mcp-access-key-id`; `check_auth` |
| `SignatureDoesNotMatch` / `InvalidSignatureException` | secret wrong (or signer bug) | `sigv4_selftest`, then fix `aws-cloud-mcp-secret-access-key` |
| `ExpiredToken` / `ExpiredTokenException` | temporary credentials expired | re-export and re-register all three refs; redeploy |
| `AccessDenied` / `AccessDeniedException` / `UnauthorizedOperation` | IAM policy lacks the named action | ask for the permission; not retryable |
| `RequestTimeTooSkewed` / `RequestExpired` / `SignatureDoesNotMatch` saying "Signature expired" | host clock off by >15 min | auto-corrected once from AWS's `Date` header; if it persists, fix the host clock |
| `PermanentRedirect` / `AuthorizationHeaderMalformed` | bucket in another region | retry with `region=` from the message |
| `NoSuchBucket` / `NoSuchKey` / `ResourceNotFoundException` | wrong name or region | list first; do not guess |
| `SlowDown` / `ThrottlingException` / `RequestLimitExceeded` / `TooManyRequestsException` | rate limit | back off 1-2 s exponentially; smaller pages; narrower windows |
| `WritesDisabled` | local policy | ask the operator; never retry |
| `InvalidArguments` | refused before any call | fix the arguments |
| `Transport` | allowedHosts / DNS / TLS / timeout | timeouts: one retry; policy: add the host |
| 5xx / `ServiceUnavailable` | AWS-side transient | retry once after a few seconds |

The full catalogue is in [references/ERRORS.md](references/ERRORS.md).

## Scope of this server

Read-mostly and bounded: no bucket/instance/function creation or deletion,
no binary downloads, no multipart uploads, no CloudWatch Logs Insights queries,
no IAM administration. Directory (S3 Express) buckets, access-point ARNs and
Outposts are virtual-host-only and unsupported. Bodies over 4 MiB fail
(`MCP_OUTBOUND_MAX_BYTES`), so keep `max_results`/`limit`/`max_bytes` modest.
For the full AWS surface, AWS's managed MCP Server (SigV4 bridged to OAuth by
a host-native proxy) is the alternative.
