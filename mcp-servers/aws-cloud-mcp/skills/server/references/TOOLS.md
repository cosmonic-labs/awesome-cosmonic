# Tool reference

Supporting file of the `aws-cloud-mcp` skill, served at
`skill://aws-cloud-mcp/references/TOOLS.md`. Every tool returns
`structuredContent` plus a readable text block; every failure is a tool error
(`isError: true`) with `{error, http_status, retryable, message}` in
`structuredContent` — see [ERRORS.md](ERRORS.md). Every tool except
`check_auth` and `sigv4_selftest` takes an optional `region` that overrides
`AWS_REGION` for that call.

| Tool | Arguments | Upstream call | Returns | Gated? |
|---|---|---|---|---|
| `check_auth` | none | `POST sts.<region>/ Action=GetCallerIdentity` | `status` (`ok`/`missing`/`invalid`/`error`), `identity` {account, arn, user_id}, `credential_type`, `session_token_configured`, `region`, `writes_enabled`, `clock_offset_seconds`, `clock_skew_detected`, `secret_refs`, `required_actions`, `remediation` | no |
| `sts_get_caller_identity` | `region?` | same | {account, arn, user_id, request_id, region, credential_type} | no |
| `s3_list_buckets` | `prefix?`, `max_buckets?` (1..10000, default 100), `continuation_token?`, `bucket_region?`, `region?` | `GET s3.<region>/?max-buckets&prefix&continuation-token&bucket-region` | `buckets[]` {name, creation_date, region}, `count`, `next_continuation_token`, `prefix`, `endpoint_region` | no |
| `s3_list_objects` | `bucket`, `prefix?`, `delimiter?`, `max_keys?` (1..1000, default 100), `continuation_token?`, `start_after?`, `region?` | `GET s3.<region>/<bucket>?list-type=2&…` (path-style) | `objects[]` {key, size, last_modified, etag, storage_class}, `common_prefixes[]`, `key_count`, `is_truncated`, `next_continuation_token` | no |
| `s3_get_object` | `bucket`, `key`, `max_bytes?` (1..1048576, default 65536), `version_id?`, `region?` | `GET s3.<region>/<bucket>/<key>` with `Range: bytes=0-N` (416 → retried without Range) | {content_type, content_length, etag, last_modified, version_id, truncated, returned_bytes, body \| binary=true, note} | no |
| `s3_put_object` | `bucket`, `key`, `body` (≤1 MiB UTF-8), `content_type?` (default `text/plain; charset=utf-8`), `if_none_match?`, `region?` | `PUT s3.<region>/<bucket>/<key>` (payload hash signed; `If-None-Match: *`) | {etag, version_id, bytes_written, content_type} | **yes** |
| `ec2_describe_instances` | `instance_ids?` (≤100, `i-…`), `filters?` {name: [values]} (≤20×20), `max_results?` (5..1000, default 100; omitted with ids), `next_token?`, `region?` | `POST ec2.<region>/ Action=DescribeInstances&Version=2016-11-15` | `instances[]` {instance_id, name, state, type, availability_zone, private_ip, public_ip, launch_time, image_id, vpc_id, subnet_id, reservation_id, tags{}}, `count`, `next_token` | no |
| `lambda_list_functions` | `max_items?` (1..50, default 50), `marker?`, `include_versions?`, `region?` | `GET lambda.<region>/2015-03-31/functions?MaxItems&Marker&FunctionVersion=ALL` | `functions[]` {name, arn, runtime, handler, memory_mb, timeout_s, last_modified, description, package_type, architectures, version, code_size}, `count`, `next_marker` | no |
| `lambda_invoke` | `function_name` (name, name:alias, partial/full ARN), `payload?` (JSON, ≤1 MiB), `invocation_type?` (`RequestResponse` default / `Event` / `DryRun`), `qualifier?`, `log_tail?` (default true), `region?` | `POST lambda.<region>/2015-03-31/functions/<name>/invocations?Qualifier` with `X-Amz-Invocation-Type`, `X-Amz-Log-Type` | {status_code (200/202/204), executed_version, function_error (`Handled`/`Unhandled`/null), payload (JSON or text), payload_truncated, log_tail} | **yes** except `DryRun` |
| `cloudwatch_logs_describe_log_groups` | `prefix?` \| `pattern?` (exclusive), `limit?` (1..50, default 50), `next_token?`, `log_group_class?` (`STANDARD`/`INFREQUENT_ACCESS`/`DELIVERY`), `region?` | `POST logs.<region>/ X-Amz-Target: Logs_20140328.DescribeLogGroups` (JSON 1.1) | `log_groups[]` {name, arn, creation_time, creation_time_iso, retention_days, stored_bytes, class}, `count`, `next_token` | no |
| `cloudwatch_logs_filter_log_events` | `log_group` (name or ARN), `filter_pattern?` (≤1024 chars), `start_time?`/`end_time?` (epoch ms or RFC 3339), `last_minutes?` (1..10080), `limit?` (1..10000, default 100), `log_stream_name_prefix?` \| `log_stream_names?` (≤100, exclusive), `next_token?`, `newest_first?`, `region?` | `POST logs.<region>/ X-Amz-Target: Logs_20140328.FilterLogEvents` | `events[]` {timestamp, time, log_stream_name, message, event_id, ingestion_time}, `count`, `next_token`, `start_time`, `end_time`, `note` (set on an empty page with a token) | no |
| `sigv4_selftest` | none | none (pure computation) | {ok, cases[] {name, expected, computed, pass}, date_used, access_key_used, clock_offset_seconds, host_time_utc} | no |

## Validation done before any call

- `bucket`: 3..63 characters of letters, digits, `.`, `-`; no slashes/ARNs.
- `key`: 1..1024 bytes, no control characters, no `.`/`..` path segments;
  sent verbatim (each `/` segment percent-encoded, `/` preserved).
- `instance_ids`: `i-` + 8 or 17 lowercase hex digits.
- `function_name`: 1..170 characters of `[A-Za-z0-9-_:$.]` (ARN colons are
  encoded as `%3A` on the wire and double-encoded for signing).
- `log_group`: `[.\-_/#A-Za-z0-9]+` ≤512, or an ARN ≤2048.
- `region` / `bucket_region`: `^[a-z]{2}(-gov|-iso[a-z]?)?-[a-z]+-\d$`.
- Free text (prefixes, tokens, filter values): bounded, no control characters,
  percent-encoded on the wire.
- Bodies: `s3_put_object.body` ≤ 1 MiB, `lambda_invoke.payload` ≤ 1 MiB
  serialized, `filter_pattern` ≤ 1024 characters — larger inputs are refused,
  never silently cut.
- Numeric page sizes are clamped to the documented ranges above.

## Write gate

`s3_put_object` and `lambda_invoke` (`RequestResponse`/`Event`) check
`AWS_ALLOW_WRITES` **before** credentials: a refusal says "writes are
disabled" and is never retryable. `DryRun` bypasses the gate (it only asks
Lambda whether the caller may invoke; nothing runs).

## IAM policy for this server

Read-only: `sts:GetCallerIdentity`, `s3:ListAllMyBuckets`, `s3:ListBucket`,
`s3:GetObject`, `ec2:DescribeInstances`, `lambda:ListFunctions`,
`logs:DescribeLogGroups`, `logs:FilterLogEvents`. Writes (only with
`AWS_ALLOW_WRITES=true`): `s3:PutObject`, `lambda:InvokeFunction`. Scope S3
actions to the buckets you intend to expose.

## Environment

| Env var | Kind | Default | Effect |
|---|---|---|---|
| `AWS_ACCESS_KEY_ID` | secret ref `aws-cloud-mcp-access-key-id` | — | required |
| `AWS_SECRET_ACCESS_KEY` | secret ref `aws-cloud-mcp-secret-access-key` | — | required |
| `AWS_SESSION_TOKEN` | secret ref `aws-cloud-mcp-session-token` | unset | only with temporary credentials; sent as `x-amz-security-token` and signed |
| `AWS_REGION` | named config | `us-east-1` | default region and credential scope |
| `AWS_ALLOW_WRITES` | named config | `false` | `true` enables the gated tools |
| `AWS_ENDPOINT_URL` | named config / test override | unset | `scheme://host[:port]` used for every service (fixtures, MinIO, LocalStack) |
| `MCP_OUTBOUND_MAX_BYTES` | named config | 4 MiB | response cap; raise for big Lambda payloads |
