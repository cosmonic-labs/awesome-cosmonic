# aws-cloud-mcp

AWS MCP server for [Cosmonic Desktop](https://cosmonic.com/docs/desktop)
that signs every request with **AWS Signature Version 4 inside the sandbox**
(no AWS SDK, no proxy) from static IAM credentials registered as secrets. It
exposes a curated, read-mostly surface over STS, S3, EC2, Lambda and
CloudWatch Logs — verify who the keys belong to, list buckets/objects and read
small text objects, list instances and functions, search or tail log groups —
plus two writes (`s3_put_object`, `lambda_invoke`) behind an explicit
`AWS_ALLOW_WRITES` gate. Every call is plain HTTPS to
`<service>.<region>.amazonaws.com`, so the workload needs exactly one outbound
host pattern and no OAuth flow.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28 (stateless streamable HTTP), exports
`wasi:http/handler@0.3.0`, serves a discovery document on `GET /` and
`GET /health`, and publishes its playbook as a skill at
`skill://aws-cloud-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://aws-cloud-mcp.localhost:8200/>.

## Tools

| Tool | Params | Output (`structuredContent` + text) | Gated? |
|---|---|---|---|
| `check_auth` | — | `status` (`ok`/`missing`/`invalid`/`error`), `identity` {account, arn, user_id}, `credential_type` (`long-term`/`temporary`), `region`, `writes_enabled`, `clock_offset_seconds`, `clock_skew_detected`, `secret_refs`, `remediation` | no |
| `sts_get_caller_identity` | `region?` | {account, arn, user_id, request_id, region, credential_type} | no |
| `s3_list_buckets` | `prefix?`, `max_buckets?` 1..10000 (100), `continuation_token?`, `bucket_region?`, `region?` | `buckets[]` {name, creation_date, region}, `count`, `next_continuation_token` | no |
| `s3_list_objects` | `bucket`, `prefix?`, `delimiter?`, `max_keys?` 1..1000 (100), `continuation_token?`, `start_after?`, `region?` | `objects[]` {key, size, last_modified, etag, storage_class}, `common_prefixes[]`, `key_count`, `is_truncated`, `next_continuation_token` | no |
| `s3_get_object` | `bucket`, `key`, `max_bytes?` 1..1048576 (65536), `version_id?`, `region?` | {content_type, content_length, etag, last_modified, version_id, truncated, returned_bytes, body \| binary} | no |
| `s3_put_object` | `bucket`, `key`, `body` (≤1 MiB UTF-8), `content_type?`, `if_none_match?`, `region?` | {etag, version_id, bytes_written, content_type} | **yes** |
| `ec2_describe_instances` | `instance_ids?` (≤100), `filters?` {name: [values]} (≤20×20), `max_results?` 5..1000 (100), `next_token?`, `region?` | `instances[]` {instance_id, name, state, type, availability_zone, private_ip, public_ip, launch_time, image_id, vpc_id, subnet_id, tags{}}, `next_token` | no |
| `lambda_list_functions` | `max_items?` 1..50 (50), `marker?`, `include_versions?`, `region?` | `functions[]` {name, arn, runtime, handler, memory_mb, timeout_s, last_modified, description, package_type, architectures, version, code_size}, `next_marker` | no |
| `lambda_invoke` | `function_name`, `payload?` (JSON ≤1 MiB), `invocation_type?` (`RequestResponse`/`Event`/`DryRun`), `qualifier?`, `log_tail?`, `region?` | {status_code, executed_version, function_error, payload, payload_truncated, log_tail} | **yes** (except `DryRun`) |
| `cloudwatch_logs_describe_log_groups` | `prefix?` \| `pattern?`, `limit?` 1..50 (50), `next_token?`, `log_group_class?`, `region?` | `log_groups[]` {name, arn, creation_time, creation_time_iso, retention_days, stored_bytes, class}, `next_token` | no |
| `cloudwatch_logs_filter_log_events` | `log_group` (name or ARN), `filter_pattern?` (≤1024), `start_time?`/`end_time?` (epoch ms or RFC 3339), `last_minutes?` 1..10080, `limit?` 1..10000 (100), `log_stream_name_prefix?` \| `log_stream_names?`, `next_token?`, `newest_first?`, `region?` | `events[]` {timestamp, time, log_stream_name, message, event_id}, `count`, `next_token`, `note` | no |
| `sigv4_selftest` | — | {ok, cases[] {name, expected, computed, pass}, clock_offset_seconds, host_time_utc} — offline, no credentials | no |

"Gated" tools are refused locally — no AWS call — unless
`AWS_ALLOW_WRITES=true`; `lambda_invoke` with `invocation_type=DryRun` is a
permission check and is never gated. Every tool takes an optional `region`
overriding `AWS_REGION`. Page sizes are clamped to AWS's documented ranges,
query values and object keys are percent-encoded per segment, oversized
bodies/patterns are refused rather than cut, and every failure is a tool error
whose `structuredContent` carries `{error, http_status, retryable, message}`
with an action to take.

The playbook (call `check_auth` first, regions, per-service pagination, the
error catalogue, write gating) is the skill: `skill://aws-cloud-mcp/SKILL.md`,
with `references/TOOLS.md`, `references/ERRORS.md` and `references/SIGV4.md`.

## Configuration

| Env var | Kind | Default | Required | Purpose |
|---|---|---|---|---|
| `AWS_ACCESS_KEY_ID` | secret ref `aws-cloud-mcp-access-key-id` | — | yes | IAM access key id (`AKIA…` long-term or `ASIA…` temporary). Missing → every credentialed tool returns an actionable error naming the ref; rejected → AWS's code plus the same hint. |
| `AWS_SECRET_ACCESS_KEY` | secret ref `aws-cloud-mcp-secret-access-key` | — | yes | The paired secret. Values are trimmed; a wrong one surfaces as `SignatureDoesNotMatch` (run `sigv4_selftest` to rule out the signer). |
| `AWS_SESSION_TOKEN` | secret ref `aws-cloud-mcp-session-token` | unset | no | Only for temporary credentials (STS/SSO exports); sent and signed as `x-amz-security-token`. Must come from the same STS call as the key pair. |
| `AWS_REGION` | named config | `us-east-1` | no | Default region for every regional endpoint and the credential scope; validated as a region code. |
| `AWS_ALLOW_WRITES` | named config | `false` | no | `true` enables `s3_put_object` and `lambda_invoke` (RequestResponse/Event). |
| `AWS_ENDPOINT_URL` | named config (test override) | unset | no | `scheme://host[:port]` used for every service instead of `https://<service>.<region>.amazonaws.com` — the e2e fixture, MinIO, LocalStack. Its host must be in `allowedHosts`. |
| `MCP_ALLOWED_HOSTS` | named config | `aws-cloud-mcp.localhost` | yes | DNS-rebinding guard; must equal the ingress host. |
| `RUST_LOG` | named config | `info` | no | Log filter. |
| `MCP_OUTBOUND_TIMEOUT_MS` / `MCP_OUTBOUND_MAX_BYTES` | named config | `30000` / 4 MiB | no | Template outbound deadline and body cap; raise the cap to `8388608` for Lambda functions returning >4 MB. |

### Getting the credentials

1. Create an IAM user (or use a role you can assume) with a least-privilege
   policy. Read-only for this server:
   `sts:GetCallerIdentity`, `s3:ListAllMyBuckets`, `s3:ListBucket`,
   `s3:GetObject`, `ec2:DescribeInstances`, `lambda:ListFunctions`,
   `logs:DescribeLogGroups`, `logs:FilterLogEvents`. Add `s3:PutObject` and
   `lambda:InvokeFunction` only if you set `AWS_ALLOW_WRITES=true`, and scope
   the S3 actions to the buckets you mean to expose.
2. IAM console → Users → *your user* → **Security credentials** → **Create
   access key** (use case: CLI) — <https://console.aws.amazon.com/iam/home#/users>.
   The secret is shown once. SSO-only organizations instead run
   `aws sso login` then `aws configure export-credentials --format env`,
   which prints all three values (they expire, typically after 1–12 h).
3. Register them as secrets. Prefer pasting them in Cosmonic Desktop →
   Secrets (refs `aws-cloud-mcp-access-key-id` / env `AWS_ACCESS_KEY_ID`,
   `aws-cloud-mcp-secret-access-key` / `AWS_SECRET_ACCESS_KEY`); otherwise:

   ```console
   $ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock      # Linux; macOS: "$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
   $ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
       -H 'Content-Type: application/json' \
       -d '{"name":"aws-cloud-mcp-access-key-id","uri":"keychain://cosmonic/aws-cloud-mcp-access-key-id","env":"AWS_ACCESS_KEY_ID","value":"AKIA…"}'
   $ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
       -H 'Content-Type: application/json' \
       -d '{"name":"aws-cloud-mcp-secret-access-key","uri":"keychain://cosmonic/aws-cloud-mcp-secret-access-key","env":"AWS_SECRET_ACCESS_KEY","value":"…"}'
   # temporary credentials only:
   $ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
       -H 'Content-Type: application/json' \
       -d '{"name":"aws-cloud-mcp-session-token","uri":"keychain://cosmonic/aws-cloud-mcp-session-token","env":"AWS_SESSION_TOKEN","value":"…"}'
   ```

   or with the MCP tool: `cosmonic_set_secret name=aws-cloud-mcp-access-key-id
   uri=keychain://cosmonic/aws-cloud-mcp-access-key-id env=AWS_ACCESS_KEY_ID
   value=AKIA…` (and likewise for the secret key; `uri=env://AWS_ACCESS_KEY_ID`
   reuses a shell export instead of storing a value). If you use the session
   token, also uncomment it under `secretFrom` in `deploy/workload.yaml`.
4. Deploy, then call `check_auth`. `GET /` shows whether each secret is
   configured (never its value) in a `credentials` block.

Temporary credentials expire and the server cannot refresh them: on
`ExpiredToken` re-export and re-register all three refs, then redeploy or
restart the workload.

## Outbound policy

`allowedHosts: ["*.amazonaws.com"]` in `deploy/workload.yaml` and
`.wash/config.yaml` covers every regional endpoint the tools dial
(`sts|s3|ec2|lambda|logs.<region>.amazonaws.com`, HTTPS with public CA
roots). No loopback ports, no volumes, no extra host interfaces. Not covered:
the China partition (`amazonaws.com.cn`) and private VPC endpoints with
custom CAs.

To point the S3 tools at a **local S3-compatible store** (MinIO, LocalStack)
running on your machine, three things must line up:

1. `AWS_ENDPOINT_URL: "http://host.wasmcloud.internal:9000"` and
   `allowedHosts: ["host.wasmcloud.internal:9000"]` in the manifest,
2. `allowedHostLoopbackPorts: ["9000"]` on the component,
3. Desktop Settings → Security → *allow host loopback* (default off).

Plain-HTTP endpoints send the SigV4 headers in clear, so do this only for
local stores.

## Build and test

```console
$ cargo build --release                      # target/wasm32-wasip2/release/aws_cloud_mcp.wasm
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ wasm-tools component wit target/wasm32-wasip2/release/aws_cloud_mcp.wasm | grep 'export wasi:http/handler@0.3.0'
$ scripts/e2e.sh                             # hermetic: scripts/fixture.py stands in for AWS
$ E2E_LIVE=1 AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… scripts/e2e.sh --no-build   # + three read-only live calls
```

The e2e runs ten wasmtime instances (primary, guard without secrets,
bad-key, wrong-secret with a session token, expired, clock-skew,
session-token, bad-endpoint, read-only, dead-endpoint) against
`scripts/fixture.py`, a threaded fixture that is a **real SigV4 verifier**:
it rebuilds the canonical request from what it receives, recomputes the
signature with the shared test secret, and answers
`SignatureDoesNotMatch` / `InvalidSignatureException` with its
CanonicalRequest/StringToSign (as AWS does, session token header included)
on any mismatch — the wrong-secret cases assert that the token is redacted
from every tool result. It serves the three error dialects, honors Range /
If-None-Match / invocation types, and echoes each verified request under
`GET /_last` so the suite asserts encoding, clamping, signed headers and the
form/JSON bodies. `sigv4_selftest` additionally reproduces the four S3
signatures AWS publishes. `cargo test` is not used (wasm target).

## Deploy on Cosmonic Desktop

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock
$ cd mcp-servers/aws-cloud-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/aws-cloud-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"aws-cloud-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/aws-cloud-mcp:0.1.0@sha256:…", …}
```

Register the secrets (above), then apply `deploy/workload.yaml` with `image`
replaced by that digest-pinned reference (`cosmonic_apply_workload`, or
`POST /v1/workloads`):

```console
$ IMAGE='oci.localhost:8200/apps/aws-cloud-mcp:0.1.0@sha256:…'
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads -H 'Content-Type: application/json' --data-binary @-
```

The manifest carries the `mcp.ai/*` labels, the
`desktop.cosmonic.com/credentials` annotation, `MCP_ALLOWED_HOSTS`,
`AWS_REGION`, `AWS_ALLOW_WRITES: "false"`, `secretFrom` for the two required
refs and `allowedHosts: ["*.amazonaws.com"]`.

### Talk to it

```console
$ curl -s http://aws-cloud-mcp.localhost:8200/ | jq '.status, .capabilities.tools, [.credentials[] | {ref, status}]'
"ok"
["check_auth","cloudwatch_logs_describe_log_groups","cloudwatch_logs_filter_log_events","ec2_describe_instances","lambda_invoke","lambda_list_functions","s3_get_object","s3_list_buckets","s3_list_objects","s3_put_object","sigv4_selftest","sts_get_caller_identity"]
[{"ref":"aws-cloud-mcp-access-key-id","status":"configured"},{"ref":"aws-cloud-mcp-secret-access-key","status":"configured"},{"ref":"aws-cloud-mcp-session-token","status":"missing"}]

$ curl -s -X POST http://aws-cloud-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://aws-cloud-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: check_auth' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"check_auth","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"credentials ok: account 123456789012, arn:aws:iam::123456789012:user/agent (long-term credentials), default region us-east-1, writes disabled"}],"structuredContent":{"status":"ok",…},"isError":false}}

$ curl -s -X POST http://aws-cloud-mcp.localhost:8200/ -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: cloudwatch_logs_filter_log_events' \
    -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"cloudwatch_logs_filter_log_events","arguments":{"log_group":"/aws/lambda/my-fn","last_minutes":15,"filter_pattern":"ERROR","limit":50},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Connect a client (the server is stateless, any streamable-HTTP client works):

```console
$ claude mcp add --transport http aws-cloud-mcp http://aws-cloud-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"aws-cloud-mcp":{"type":"http","url":"http://aws-cloud-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [awslabs/mcp](https://github.com/awslabs/mcp) (Apache-2.0) — the AWS Labs
  MCP servers (`aws-api-mcp-server`, `cloudwatch-mcp-server`,
  `lambda-tool-mcp-server`). Tool *ideas* only: read-only by default with
  explicit mutation consent (→ `AWS_ALLOW_WRITES`), the `describe_log_groups`
  shape, and surfacing `FunctionError` plus the decoded `LogResult` on invoke.
  They are Python/boto3 over stdio; no code was ported.
- [rishikavikondala/mcp-server-aws](https://github.com/rishikavikondala/mcp-server-aws)
  (MIT) — the `s3_list_buckets` / `s3_get_object` / `s3_put_object` naming
  convention. No code.
- [YawLabs/aws-mcp](https://github.com/YawLabs/aws-mcp) (MIT) — the
  "whoami first" workflow (→ `check_auth` / `sts_get_caller_identity`) and
  the "last N minutes" log convenience (→ `last_minutes`). No code.
- AWS documentation — *Create a signed AWS API request* and S3
  *Authenticating Requests: Using the Authorization Header*: the SigV4
  canonicalization rules and the four published example signatures that
  `sigv4_selftest` reproduces; the per-service API references for wire
  formats, limits and error codes.
- [smithy-rs `aws-sigv4`](https://github.com/smithy-lang/smithy-rs/tree/main/aws/rust-runtime/aws-sigv4)
  (Apache-2.0) — consulted for the edge rules (single- vs double-encoded
  canonical URI, header value normalization). Not depended on.
- The managed **AWS MCP Server** (GA 2026, `aws-mcp.us-east-1.api.aws`) is
  the vendor route for the full AWS surface; it needs a host-native
  SigV4-to-OAuth proxy and is therefore not used here.

This port is Apache-2.0 (see `LICENSE`).

## Known limitations

- **Static credentials only.** IAM Identity Center / SSO device flows, the
  managed AWS MCP Server's OAuth 2.1 proxy and IMDS/ECS credential providers
  need an interactive or host-native flow the sandbox cannot run. Temporary
  credentials work but are not refreshed: re-register the three refs when
  they expire.
- **Read-mostly, bounded surface.** No bucket/instance/function creation or
  deletion, no binary downloads or multipart uploads, no Logs Insights
  queries, no IAM administration. `s3_get_object` reads at most 1 MiB of a
  UTF-8 object; `s3_put_object` and `lambda_invoke` payloads are capped at
  1 MiB; responses over `MCP_OUTBOUND_MAX_BYTES` (4 MiB) fail.
- **Path-style S3 on regional endpoints.** Directory (S3 Express) buckets,
  access-point ARNs and Outposts are virtual-host-only and unsupported. A
  bucket in another region is reported (301 with the right region) rather
  than followed.
- **One coarse write gate.** `AWS_ALLOW_WRITES` enables both writes for every
  bucket/function the IAM policy allows; keep it `false` and use a narrow
  policy, or run a separate write-enabled workload.
- **Clock skew** is corrected once per instance from AWS's `Date` header
  (`check_auth` then reports `clock_skew_detected: true` and says by how much
  the host clock is off); a continuously drifting host clock still needs
  fixing on the host, and the cached correction is lost on restart.
- **Error mapping is by AWS code**; an unrecognized body shape falls back to
  a generic `HTTP <status>` error with the first 500 characters of the body.
  Every excerpt of an upstream body is scrubbed first: the value of any
  echoed `x-amz-security-token` header (AWS returns the full canonical
  request on `SignatureDoesNotMatch`) and the configured session token /
  secret access key are replaced with `<redacted>` before they can reach a
  tool result or log.
- Throttling is reported, not retried: FilterLogEvents is 5 calls/s per
  account/region, so agents should narrow windows rather than loop.
