# Error catalogue

Supporting file of the `aws-cloud-mcp` skill, served at
`skill://aws-cloud-mcp/references/ERRORS.md`. The server normalizes the three
AWS error dialects (S3/STS XML `<Error><Code>`, EC2 `<Response><Errors>`,
JSON `{"__type"}` / `x-amzn-ErrorType`) into one tool error whose
`structuredContent.error` is the AWS code below (or a local pseudo-code) and
whose text ends with the action to take. `retryable` is true only for
throttling, 5xx and transport failures.

## Local refusals (no AWS call was made)

| `error` | Condition | Do |
|---|---|---|
| `MissingCredential:AWS_ACCESS_KEY_ID` / `…:AWS_SECRET_ACCESS_KEY` | secret ref not registered or not in `secretFrom` | Create an IAM access key (IAM → Users → Security credentials → Create access key, use case CLI) or export temporary credentials; register `aws-cloud-mcp-access-key-id` and `aws-cloud-mcp-secret-access-key` (plus `aws-cloud-mcp-session-token` for temporary ones) via Desktop → Secrets or `cosmonic_set_secret`; list them under `secretFrom`; redeploy; `check_auth`. Never retry. |
| `InvalidRegion` | `AWS_REGION` or the `region` argument is not a region code | use e.g. `us-east-1`, `eu-west-2`, `us-gov-west-1` |
| `InvalidEndpoint` | `AWS_ENDPOINT_URL` is not `scheme://host[:port]` | fix the config |
| `WritesDisabled` | `s3_put_object` / `lambda_invoke` (RequestResponse/Event) with `AWS_ALLOW_WRITES` ≠ `true` | ask the operator to set `AWS_ALLOW_WRITES: "true"` and redeploy (ideally a separate write-enabled workload with a narrower IAM policy); `DryRun` still works |
| `InvalidArguments` | validation failed (bad bucket/key/id/name, oversized body or pattern, mutually exclusive params, malformed timestamp) | fix the arguments per the message |

## Credential and signing errors

| Code (service) | HTTP | Meaning | Do |
|---|---|---|---|
| `InvalidClientTokenId` (STS), `InvalidAccessKeyId` (S3), `AuthFailure` (EC2), `UnrecognizedClientException` (Logs, Lambda) | 403 / 401 / 400 | key id unknown, deactivated, deleted, or from another partition (GovCloud/China) | check the key is Active in IAM; re-register `aws-cloud-mcp-access-key-id` (watch for pasted whitespace); `check_auth` |
| `SignatureDoesNotMatch` (S3, STS, EC2), `InvalidSignatureException` (Logs, Lambda) | 403 | secret does not match the key id, or the request was altered in transit | run `sigv4_selftest`: pass → re-register `aws-cloud-mcp-secret-access-key` (trailing newline / wrong value); fail → signer bug, report the output. Never retry blindly. The message carries AWS's canonical-request echo with `x-amz-security-token:<redacted>` — the token is scrubbed, by design |
| `ExpiredToken` (S3, STS, EC2), `ExpiredTokenException` (Logs, Lambda) | 400 / 403 | temporary credentials expired; the server cannot refresh them | `aws configure export-credentials --format env` (or sts get-session-token / assume-role) and re-register all three refs together; redeploy or restart |
| `InvalidToken` (S3) / "The security token included in the request is invalid" (any) | 400 / 403 | `AWS_SESSION_TOKEN` does not belong to the key pair (mixed sources, or a stale token next to long-term AKIA keys) | register all three values from the same STS call, or remove `aws-cloud-mcp-session-token` from `secretFrom` |
| `RequestTimeTooSkewed` (S3), `SignatureDoesNotMatch` "Signature expired: … is now earlier than …" (STS), `RequestExpired` (EC2), `InvalidSignatureException` "Signature expired" (Logs, Lambda) | 403 / 400 | `x-amz-date` more than 15 minutes from AWS time (a host clock that is off — even by days after a sleep/VM restore) | handled: the server reads the `Date` header, caches the offset and retries once; if it persists, fix the host clock (chrony/timesyncd) and restart the workload |
| `XAmzContentSHA256Mismatch`, `BadDigest` (S3 PUT) | 400 | signed payload hash ≠ bytes received (proxy rewrote the body, or signer bug) | `sigv4_selftest`; look for an intercepting proxy; report the request id |

## Authorization

| Code (service) | HTTP | Meaning | Do |
|---|---|---|---|
| `AccessDenied` (S3), `AccessDeniedException` (STS, Logs, Lambda), `UnauthorizedOperation` (EC2) | 403 | credentials valid, IAM policy denies the action (message usually names it: "not authorized to perform: logs:FilterLogEvents on resource …") | attach the permission to the principal `check_auth` reports (read-only: `sts:GetCallerIdentity`, `s3:ListAllMyBuckets`, `s3:ListBucket`, `s3:GetObject`, `ec2:DescribeInstances`, `lambda:ListFunctions`, `logs:DescribeLogGroups`, `logs:FilterLogEvents`; writes: `s3:PutObject`, `lambda:InvokeFunction`). Bucket policies and SCPs can also deny. Not retryable |
| `OptInRequired` (EC2) | 403 | region not enabled for the account | pick another region or enable it in the account settings |

## Region and naming

| Code (service) | HTTP | Meaning | Do |
|---|---|---|---|
| `PermanentRedirect` (S3, empty body + `x-amz-bucket-region`), `AuthorizationHeaderMalformed` "expecting 'eu-west-1'", `IllegalLocationConstraintException` | 301 / 400 | bucket lives in another region; path-style requests are never redirected | retry with `region=<value from the message>`; `s3_list_buckets` shows every bucket's region |
| `NoSuchBucket` | 404 | no such bucket (names are global and exact) | `s3_list_buckets` |
| `NoSuchKey` | 404 | no such object (keys are exact and case-sensitive; "folders" are prefixes) | `s3_list_objects` with prefix/delimiter; never guess |
| `ResourceNotFoundException` (Lambda 404, Logs 400) | 404 / 400 | function or log group does not exist in this region | list with a prefix, or pass `region=…` |
| `InvalidInstanceID.Malformed` / `InvalidInstanceID.NotFound` (EC2) | 400 | id not `i-xxxxxxxxxxxxxxxxx` or not in this region/account | use filters instead of guessed ids; check the region |
| `InvalidObjectState` (S3 GetObject) | 403 | object archived (Glacier / Deep Archive) | restore with the console or `aws s3api restore-object`; retry hours later |
| `InvalidRange` (S3) | 416 | zero-byte object with a Range request | handled internally (retried without Range); agents never see it |

## Parameters

| Code (service) | HTTP | Meaning | Do |
|---|---|---|---|
| `InvalidParameterException` (Logs) | 400 | e.g. prefix + names together, `startFromHead=false` with an old `startTime`, bad `filterPattern` syntax | the tools enforce the combinations locally; follow AWS's text for pattern syntax |
| `InvalidParameterCombination` / `InvalidParameterValue` (EC2) | 400 | `MaxResults` with instance ids (never sent by this server), `MaxResults` < 5, bad filter name | fix per message |
| `InvalidRequestContentException` (Lambda) | 400 | payload is not the JSON the function expects | fix the payload |
| `RequestTooLargeException` (Lambda 413) / local "payload serializes to N bytes" | 413 | invoke payload over 1 MiB (AWS: 6 MB sync, 256 KB async) | send a reference (S3 key) instead |
| `PreconditionFailed` (412) / `ConditionalRequestConflict` (409) (S3 PUT) | 412 / 409 | `if_none_match=true` and the key exists, or a concurrent write raced | omit `if_none_match` to overwrite deliberately, or pick another key |
| `NotImplemented` / `MethodNotAllowed` (S3) | 501 / 405 | endpoint does not support the request shape (directory buckets, some S3-compatible stores) | use a general-purpose bucket / regional endpoint |

## Throttling and transient failures (`retryable: true`)

| Code (service) | HTTP | Meaning | Do |
|---|---|---|---|
| `SlowDown` (S3 503), `ThrottlingException` (Logs 400, STS), `RequestLimitExceeded` (EC2 503), `TooManyRequestsException` (Lambda 429 + `Retry-After`) | 503 / 400 / 429 | rate limit (FilterLogEvents 5 TPS/account/region; EC2 Describe token bucket; S3 per-prefix) | back off exponentially from 1-2 s (honor `Retry-After`), reduce page sizes, narrow time windows; the server does not auto-retry |
| `InternalError` / `InternalFailure` / `ServiceUnavailable(Exception)` / `ServiceException` / 502 `KMS*`, `EC2*`, `ENI*`, `EFS*` (Lambda) | 5xx | AWS-side transient error or a Lambda environment problem | retry once after a few seconds; persistent 502 environment errors need fixing in AWS |
| `SnapStartNotReadyException` / `ResourceConflictException` (Lambda 409) | 409 | function initializing or being updated | wait, retry once |
| `Transport` | — | `HttpRequestDenied` (host not in `allowedHosts`), DNS, TLS (private CA), timeout | policy: add `*.amazonaws.com` (or the `AWS_ENDPOINT_URL` host + loopback grant); timeouts: retry once; private CAs are unsupported |
| `MalformedResponse` | — | 2xx body is not the documented shape (proxy, S3-compatible store) | check `AWS_ENDPOINT_URL`; retry once |
| "response body exceeded the outbound size limit" | — | body over `MCP_OUTBOUND_MAX_BYTES` (4 MiB) | lower `max_results`/`limit`/`max_bytes`, filter more narrowly, or raise the cap |

## Not errors

- `lambda_invoke` with `status_code: 200` and `function_error: "Unhandled"`
  or `"Handled"`: the invoke succeeded, the function threw or timed out
  ("Task timed out after N seconds"). Read `payload.errorMessage` and
  `log_tail`; then `cloudwatch_logs_filter_log_events` on
  `/aws/lambda/<function>`.
- `cloudwatch_logs_filter_log_events` returning `count: 0` with a
  `next_token`: an empty page, not the end. Keep paginating.
- `s3_get_object` with `truncated: true`: a successful partial read; raise
  `max_bytes` if the rest is needed.
