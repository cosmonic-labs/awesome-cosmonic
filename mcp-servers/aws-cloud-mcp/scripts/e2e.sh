#!/usr/bin/env bash
# End-to-end tests for aws-cloud-mcp. Framework checks (protocol, spec
# enforcement, discovery route, skills over MCP, robustness, Host guard) come
# from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases live
# below.
#
# The suite is hermetic: scripts/fixture.py (a threaded Python server) stands
# in for sts/s3/ec2/lambda/logs.<region>.amazonaws.com behind one base URL and
# is a real SigV4 verifier — it rebuilds the canonical request from what it
# receives and recomputes the signature with the shared test secret, so every
# case below also proves the signer. Ten wasmtime instances run:
#   primary  — example credentials, AWS_ALLOW_WRITES=true (most cases)
#   guard    — no credentials, no writes (missing-secret path, write gate,
#              Host guard)
#   badkey   — access key the fixture rejects (per-service bad-key dialects)
#   badsecret— known ASIA… key + session token but the WRONG secret: the
#              fixture answers SignatureDoesNotMatch / InvalidSignatureException
#              echoing the canonical request (token header included, as AWS
#              does) — proves the mapping and that the token is redacted
#   expired  — access key the fixture reports as expired
#   skew     — fixture clock 20 min ahead (proves the Date-header retry)
#   session  — temporary ASIA… key + session token (x-amz-security-token)
#   badcfg   — AWS_ENDPOINT_URL that is not a base URL
#   ro       — credentials but writes disabled (DryRun still works)
#   dead     — AWS_ENDPOINT_URL pointing at a closed port (transport error)
# E2E_LIVE=1 with real AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
# [/ AWS_SESSION_TOKEN] exported adds three read-only calls against AWS.
#
# Usage: scripts/e2e.sh [--no-build]
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9670}
GUARD_PORT=${GUARD_PORT:-9671}
FIXTURE_PORT=${FIXTURE_PORT:-9672}
BADKEY_PORT=${BADKEY_PORT:-$((PORT + 3))}
SKEW_PORT=${SKEW_PORT:-$((PORT + 4))}
EXPIRED_PORT=${EXPIRED_PORT:-$((PORT + 5))}
SESSION_PORT=${SESSION_PORT:-$((PORT + 6))}
BADCFG_PORT=${BADCFG_PORT:-$((PORT + 7))}
RO_PORT=${RO_PORT:-$((PORT + 8))}
DEAD_PORT=${DEAD_PORT:-$((PORT + 9))}
LIVE_PORT=${LIVE_PORT:-$((PORT + 10))}
# +11/+12 belong to a sibling suite (obsidian-mcp); skip them.
BADSECRET_PORT=${BADSECRET_PORT:-$((PORT + 13))}
WASM=${WASM:-target/wasm32-wasip2/release/aws_cloud_mcp.wasm}
SKILL_NAME=aws-cloud-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

FIXTURE="http://127.0.0.1:${FIXTURE_PORT}"
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"
BADKEY_BASE="http://127.0.0.1:${BADKEY_PORT}/"
SKEW_BASE="http://127.0.0.1:${SKEW_PORT}/"
EXPIRED_BASE="http://127.0.0.1:${EXPIRED_PORT}/"
SESSION_BASE="http://127.0.0.1:${SESSION_PORT}/"
BADCFG_BASE="http://127.0.0.1:${BADCFG_PORT}/"
RO_BASE="http://127.0.0.1:${RO_PORT}/"
DEAD_BASE="http://127.0.0.1:${DEAD_PORT}/"
BADSECRET_BASE="http://127.0.0.1:${BADSECRET_PORT}/"
AK=AKIAIOSFODNN7EXAMPLE
SK=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
# For the badsecret instance: a wrong secret and a session token carrying a
# marker that must never appear in any tool output.
WRONG_SK=WRONGSECRETKEYxXtnFEMI/K7MDENG/bPxRfiCYWRONGKEY
LEAK_TOKEN="FwoGZXIvYXdzEBEaDS3CR3TMARKERtokenS3CR3TMARKER/+bodyS3CR3TMARKER=="

EXTRA_PIDS=()
cleanup_all() {
  for pid in ${EXTRA_PIDS[@]+"${EXTRA_PIDS[@]}"}; do kill "$pid" 2>/dev/null; done
  mcp_harness_cleanup
}
trap cleanup_all EXIT

# start_instance <port> <logname> [wasmtime args...]
start_instance() {
  local port="$1" name="$2"
  shift 2
  echo "starting $name instance on :${port}..."
  "$WASMTIME" serve -Sp3,cli,http "$@" --addr "127.0.0.1:${port}" "$WASM" \
    >"$E2E_TMP/$name.log" 2>&1 &
  EXTRA_PIDS+=($!)
  mcp_wait_ready "$port"
}

# assert_last <name> <python-expr> — evaluate <expr> with `d` bound to the
# last request the fixture verified (method, raw_path, query, headers, body,
# canonical_request, signed_headers, service, region, access_key).
assert_last() {
  local name="$1" expr="$2" body
  body=$(curl -sS --max-time 10 "${FIXTURE}/_last")
  if printf '%s' "$body" | python3 -c '
import json, sys
d = json.load(sys.stdin)
sys.exit(0 if eval(sys.argv[1]) else 1)
' "$expr" 2>/dev/null; then
    pass "$name"
  else
    fail "$name" "expression [$expr] false for: $body"
  fi
}

# assert_sc <name> <python-expr> <sse> — like assert_json with `s` bound to
# result.structuredContent (and `r` to the whole message).
assert_sc() {
  assert_json "$1" "(lambda s: $2)(r['result']['structuredContent'])" "$3"
}

# mcp_call_big <tool> <python-expr-for-arguments> — for arguments too large
# for a shell argument (MAX_ARG_STRLEN is 128 KiB).
mcp_call_big() {
  local tool="$1" expr="$2"
  python3 -c '
import json, sys
args = eval(sys.argv[2])
print(json.dumps({"jsonrpc": "2.0", "id": 99, "method": "tools/call", "params": {"name": sys.argv[1], "arguments": args, "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}}}}))
' "$tool" "$expr" >"$E2E_TMP/big.json"
  curl -sS --max-time 60 -X POST "$MCP_BASE" -H "$CT" -H "$ACCEPT" -H "$PV" \
    -H 'Mcp-Method: tools/call' -H "Mcp-Name: $tool" --data-binary @"$E2E_TMP/big.json"
}

# The concurrency test in framework_tests fires this tool 8x in parallel —
# an outbound, signed tool, so concurrent outbound (inter-task-wakeup) and
# concurrent signing are exercised.
FIRST_TOOL_NAME=sts_get_caller_identity
FIRST_TOOL_ARGS='{}'
FIRST_TOOL_EXPECT='"account":"123456789012"'

mcp_build_if_needed "${1:-}"

echo "starting fixture on :${FIXTURE_PORT}..."
python3 scripts/fixture.py "$FIXTURE_PORT" >"$E2E_TMP/fixture.log" 2>&1 &
FIXTURE_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "${FIXTURE}/_count" && break
  sleep 0.2
done

COMMON=(--env "AWS_ENDPOINT_URL=${FIXTURE}" --env AWS_REGION=us-east-1)
mcp_harness_start "${COMMON[@]}" --env "AWS_ACCESS_KEY_ID=${AK}" --env "AWS_SECRET_ACCESS_KEY=${SK}" --env AWS_ALLOW_WRITES=true
mcp_harness_start_guard "${COMMON[@]}"
start_instance "$BADKEY_PORT" badkey "${COMMON[@]}" --env AWS_ACCESS_KEY_ID=AKIABADKEY --env "AWS_SECRET_ACCESS_KEY=${SK}"
start_instance "$SKEW_PORT" skew "${COMMON[@]}" --env AWS_ACCESS_KEY_ID=AKIASKEW --env "AWS_SECRET_ACCESS_KEY=${SK}"
start_instance "$EXPIRED_PORT" expired "${COMMON[@]}" --env AWS_ACCESS_KEY_ID=AKIAEXPIRED --env "AWS_SECRET_ACCESS_KEY=${SK}"
start_instance "$SESSION_PORT" session "${COMMON[@]}" --env AWS_ACCESS_KEY_ID=ASIATEMPKEY --env "AWS_SECRET_ACCESS_KEY=${SK}" --env AWS_SESSION_TOKEN=FwoGZXIvYXdzEBEaDexampletoken --env AWS_ALLOW_WRITES=yes
start_instance "$BADCFG_PORT" badcfg --env "AWS_ENDPOINT_URL=ftp://not a url" --env AWS_REGION=us-east-1 --env "AWS_ACCESS_KEY_ID=${AK}" --env "AWS_SECRET_ACCESS_KEY=${SK}"
start_instance "$RO_PORT" ro "${COMMON[@]}" --env "AWS_ACCESS_KEY_ID=${AK}" --env "AWS_SECRET_ACCESS_KEY=${SK}" --env AWS_ALLOW_WRITES=false
start_instance "$DEAD_PORT" dead --env AWS_ENDPOINT_URL=http://127.0.0.1:9 --env AWS_REGION=us-east-1 --env "AWS_ACCESS_KEY_ID=${AK}" --env "AWS_SECRET_ACCESS_KEY=${SK}" --env MCP_OUTBOUND_TIMEOUT_MS=5000
start_instance "$BADSECRET_PORT" badsecret "${COMMON[@]}" --env AWS_ACCESS_KEY_ID=ASIATEMPKEY --env "AWS_SECRET_ACCESS_KEY=${WRONG_SK}" --env "AWS_SESSION_TOKEN=${LEAK_TOKEN}"

ALL_TOOLS="check_auth cloudwatch_logs_describe_log_groups cloudwatch_logs_filter_log_events ec2_describe_instances lambda_invoke lambda_list_functions s3_get_object s3_list_buckets s3_list_objects s3_put_object sigv4_selftest sts_get_caller_identity"
# shellcheck disable=SC2086
framework_tests $ALL_TOOLS
discovery_tests sts_get_caller_identity
ROOT=$(curl -sS --max-time 20 "$MCP_BASE")
assert_contains "GET / carries the access-key credential ref" '"ref": "aws-cloud-mcp-access-key-id"' "$ROOT"
assert_contains "GET / carries the secret-key credential ref" '"ref": "aws-cloud-mcp-secret-access-key"' "$ROOT"
assert_contains "GET / reports the credential as configured" '"status": "configured"' "$ROOT"
assert_contains "GET / names check_auth as the validator" '"validate": "check_auth"' "$ROOT"
assert_not_contains "GET / never leaks the secret value" "$SK" "$ROOT"
ROOT=$(curl -sS --max-time 20 "$GUARD_BASE")
assert_contains "GET / on the guard reports the credential as missing" '"status": "missing"' "$ROOT"
skills_tests "$SKILL_NAME" references/TOOLS.md references/ERRORS.md references/SIGV4.md
OUT=$(mcp_read_resource "skill://$SKILL_NAME/SKILL.md")
assert_contains "SKILL.md tells agents to call check_auth first" 'check_auth' "$OUT"
assert_contains "SKILL.md documents the empty-page pagination rule" 'empty page' "$OUT"
OUT=$(mcp_read_resource "skill://$SKILL_NAME/references/ERRORS.md")
assert_contains "ERRORS.md maps SignatureDoesNotMatch to sigv4_selftest" 'sigv4_selftest' "$OUT"

echo "== sigv4 selftest (published AWS vectors) =="
OUT=$(mcp_call sigv4_selftest '{}')
assert_sc "selftest passes" 's["ok"] is True and all(c["pass"] for c in s["cases"]) and len(s["cases"]) == 4' "$OUT"
assert_contains "selftest reproduces the GET+Range vector" 'f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41' "$OUT"
assert_contains "selftest reproduces the PUT vector" '98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd' "$OUT"
assert_contains "selftest reproduces the ?lifecycle vector" 'fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543' "$OUT"
assert_contains "selftest reproduces the list-objects vector" '34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7' "$OUT"
assert_contains "selftest text says the signer is fine" 'selftest passed' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" sigv4_selftest '{}')
assert_sc "selftest works without credentials (guard)" 's["ok"] is True and s["clock_offset_seconds"] == 0' "$OUT"

echo "== check_auth / sts =="
OUT=$(mcp_call check_auth '{}')
assert_contains "check_auth ok" '"isError":false' "$OUT"
assert_sc "check_auth reports identity, credential type and write policy" 's["status"] == "ok" and s["identity"]["account"] == "123456789012" and s["identity"]["arn"].endswith(":user/e2e") and s["credential_type"] == "long-term" and s["writes_enabled"] is True and s["region"] == "us-east-1"' "$OUT"
assert_last "sts is a signed form POST (content-type;host;x-amz-date)" 'd["service"] == "sts" and d["method"] == "POST" and d["signed_headers"] == "content-type;host;x-amz-date" and d["body"] == "Action=GetCallerIdentity&Version=2011-06-15" and d["headers"]["content-type"].startswith("application/x-www-form-urlencoded")'
assert_last "request bodies go out with Content-Length, not chunked" '"content-length" in d["headers"] and "transfer-encoding" not in d["headers"]'
assert_last "Host is signed with the non-default port" 'd["headers"]["host"] == "127.0.0.1:'"$FIXTURE_PORT"'"'
OUT=$(mcp_call sts_get_caller_identity '{"region":"eu-west-1"}')
assert_sc "sts region override is used in the credential scope" 's["region"] == "eu-west-1" and s["account"] == "123456789012"' "$OUT"
assert_last "credential scope carries the overridden region" 'd["region"] == "eu-west-1"'
OUT=$(mcp_call sts_get_caller_identity '{"region":"US East"}')
assert_contains "malformed region is a clean tool error" '"isError":true' "$OUT"
assert_contains "malformed region names the pattern" 'us-east-1' "$OUT"
assert_sc "malformed region error code" 's["error"] == "InvalidRegion" and s["retryable"] is False' "$OUT"
OUT=$(mcp_call sts_get_caller_identity '{"region":"us-east-1; DROP TABLE"}')
assert_contains "injection-shaped region refused" 'InvalidRegion' "$OUT"
OUT=$(mcp_call sts_get_caller_identity '{"region":"us-gov-west-1"}')
assert_sc "GovCloud-style region accepted" 's["region"] == "us-gov-west-1"' "$OUT"
OUT=$(mcp_call sts_get_caller_identity '{"region":123}')
assert_contains "ill-typed region is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$SESSION_BASE" check_auth '{}')
assert_sc "temporary credentials: assumed-role ARN and credential_type" 's["status"] == "ok" and "assumed-role" in s["identity"]["arn"] and s["credential_type"] == "temporary" and s["session_token_configured"] is True and s["writes_enabled"] is True' "$OUT"
assert_last "session token is sent and signed (x-amz-security-token)" '"x-amz-security-token" in d["signed_headers"].split(";") and d["headers"]["x-amz-security-token"] == "FwoGZXIvYXdzEBEaDexampletoken"'
OUT=$(mcp_call_on "$BADCFG_BASE" check_auth '{}')
assert_sc "bad AWS_ENDPOINT_URL is reported by check_auth" 's["status"] == "error" and "AWS_ENDPOINT_URL" in s["remediation"]' "$OUT"
OUT=$(mcp_call_on "$BADCFG_BASE" s3_list_buckets '{}')
assert_sc "bad AWS_ENDPOINT_URL is a clean tool error" 's["error"] == "InvalidEndpoint"' "$OUT"
OUT=$(mcp_call_on "$DEAD_BASE" sts_get_caller_identity '{}')
assert_sc "unreachable endpoint is a retryable Transport error naming allowedHosts" 's["error"] == "Transport" and s["retryable"] is True and "allowedHosts" in s["message"]' "$OUT"

echo "== credential errors (per-service dialects) =="
OUT=$(mcp_call_on "$BADKEY_BASE" check_auth '{}')
assert_sc "bad key: check_auth says invalid with the STS code" 's["status"] == "invalid" and "InvalidClientTokenId" in s["remediation"] and "aws-cloud-mcp-access-key-id" in s["remediation"]' "$OUT"
assert_contains "bad key: check_auth is an error result" '"isError":true' "$OUT"
OUT=$(mcp_call_on "$BADKEY_BASE" s3_list_buckets '{}')
assert_sc "bad key: S3 InvalidAccessKeyId + remediation" 's["error"] == "InvalidAccessKeyId" and s["http_status"] == 403 and s["retryable"] is False and "aws-cloud-mcp-access-key-id" in s["message"]' "$OUT"
OUT=$(mcp_call_on "$BADKEY_BASE" ec2_describe_instances '{}')
assert_sc "bad key: EC2 AuthFailure (401, <Response><Errors> dialect)" 's["error"] == "AuthFailure" and s["http_status"] == 401 and "check_auth" in s["message"]' "$OUT"
OUT=$(mcp_call_on "$BADKEY_BASE" cloudwatch_logs_describe_log_groups '{}')
assert_sc "bad key: Logs UnrecognizedClientException (JSON __type dialect)" 's["error"] == "UnrecognizedClientException" and s["http_status"] == 400' "$OUT"
OUT=$(mcp_call_on "$BADKEY_BASE" lambda_list_functions '{}')
assert_sc "bad key: Lambda UnrecognizedClientException (x-amzn-ErrorType dialect)" 's["error"] == "UnrecognizedClientException" and s["http_status"] == 403' "$OUT"
OUT=$(mcp_call_on "$EXPIRED_BASE" sts_get_caller_identity '{}')
assert_sc "expired: STS ExpiredToken tells the user to re-register all three refs" 's["error"] == "ExpiredToken" and "aws-cloud-mcp-session-token" in s["message"] and "export-credentials" in s["message"]' "$OUT"
OUT=$(mcp_call_on "$EXPIRED_BASE" check_auth '{}')
assert_sc "expired: check_auth reports invalid" 's["status"] == "invalid"' "$OUT"
OUT=$(mcp_call_on "$EXPIRED_BASE" cloudwatch_logs_filter_log_events '{"log_group":"app/prod"}')
assert_sc "expired: Logs ExpiredTokenException mapped the same way" 's["error"] == "ExpiredTokenException" and "re-register" in s["message"]' "$OUT"
OUT=$(mcp_call_on "$EXPIRED_BASE" s3_get_object '{"bucket":"alpha-bucket","key":"test.txt"}')
assert_sc "expired: S3 ExpiredToken (400)" 's["error"] == "ExpiredToken" and s["http_status"] == 400' "$OUT"

echo "== wrong secret (SignatureDoesNotMatch) and session-token redaction =="
# The fixture rebuilds the canonical request and, like AWS, echoes it back
# with every signed header — including x-amz-security-token — so a wrong
# secret is exactly the path that could leak the token to the client.
OUT=$(mcp_call_on "$BADSECRET_BASE" s3_list_buckets '{}')
assert_sc "wrong secret: S3 SignatureDoesNotMatch 403, not retryable" 's["error"] == "SignatureDoesNotMatch" and s["http_status"] == 403 and s["retryable"] is False' "$OUT"
assert_sc "wrong secret: advice says run sigv4_selftest then re-register the secret ref" '"sigv4_selftest" in s["message"] and "aws-cloud-mcp-secret-access-key" in s["message"]' "$OUT"
assert_contains "wrong secret: S3's CanonicalRequest echo is kept for diagnosis" 'AWS computed this CanonicalRequest' "$OUT"
assert_contains "wrong secret: the echoed token header is redacted (S3 element)" 'x-amz-security-token:<redacted>' "$OUT"
assert_not_contains "wrong secret: session token never reaches the client (S3)" 'S3CR3TMARKER' "$OUT"
assert_not_contains "wrong secret: the secret itself never reaches the client (S3)" 'WRONGSECRETKEY' "$OUT"
OUT=$(mcp_call_on "$BADSECRET_BASE" check_auth '{}')
assert_sc "wrong secret: check_auth reports invalid with the STS code" 's["status"] == "invalid" and "SignatureDoesNotMatch" in s["remediation"] and s["credential_type"] == "temporary" and s["session_token_configured"] is True' "$OUT"
assert_contains "wrong secret: check_auth is an error result" '"isError":true' "$OUT"
assert_contains "wrong secret: STS's canonical string in the message is redacted" 'x-amz-security-token:<redacted>' "$OUT"
assert_not_contains "wrong secret: session token never reaches the client (check_auth)" 'S3CR3TMARKER' "$OUT"
assert_not_contains "wrong secret: secret never reaches the client (check_auth)" 'WRONGSECRETKEY' "$OUT"
OUT=$(mcp_call_on "$BADSECRET_BASE" sts_get_caller_identity '{}')
assert_sc "wrong secret: STS SignatureDoesNotMatch (Query dialect, canonical string in Message)" 's["error"] == "SignatureDoesNotMatch" and s["http_status"] == 403 and "sigv4_selftest" in s["message"]' "$OUT"
assert_not_contains "wrong secret: session token never reaches the client (STS)" 'S3CR3TMARKER' "$OUT"
OUT=$(mcp_call_on "$BADSECRET_BASE" ec2_describe_instances '{}')
assert_sc "wrong secret: EC2 SignatureDoesNotMatch (<Response><Errors> dialect)" 's["error"] == "SignatureDoesNotMatch" and s["http_status"] == 403 and s["retryable"] is False' "$OUT"
assert_contains "wrong secret: EC2 echo redacted" 'x-amz-security-token:<redacted>' "$OUT"
assert_not_contains "wrong secret: session token never reaches the client (EC2)" 'S3CR3TMARKER' "$OUT"
OUT=$(mcp_call_on "$BADSECRET_BASE" lambda_list_functions '{}')
assert_sc "wrong secret: Lambda InvalidSignatureException mapped like SignatureDoesNotMatch" 's["error"] == "InvalidSignatureException" and s["http_status"] == 403 and s["retryable"] is False and "aws-cloud-mcp-secret-access-key" in s["message"]' "$OUT"
assert_contains "wrong secret: Lambda echo redacted" 'x-amz-security-token:<redacted>' "$OUT"
assert_not_contains "wrong secret: session token never reaches the client (Lambda)" 'S3CR3TMARKER' "$OUT"
OUT=$(mcp_call_on "$BADSECRET_BASE" cloudwatch_logs_describe_log_groups '{}')
assert_sc "wrong secret: Logs InvalidSignatureException (JSON __type dialect)" 's["error"] == "InvalidSignatureException" and s["http_status"] == 403 and "sigv4_selftest" in s["message"]' "$OUT"
assert_not_contains "wrong secret: session token never reaches the client (Logs)" 'S3CR3TMARKER' "$OUT"
OUT=$(mcp_call_on "$BADSECRET_BASE" cloudwatch_logs_filter_log_events '{"log_group":"app/prod"}')
assert_sc "wrong secret: Logs filter also InvalidSignatureException" 's["error"] == "InvalidSignatureException"' "$OUT"
assert_not_contains "wrong secret: session token never reaches the client (Logs filter)" 'S3CR3TMARKER' "$OUT"
OUT=$(mcp_call_on "$BADSECRET_BASE" sigv4_selftest '{}')
assert_sc "wrong secret: sigv4_selftest still passes (signer is fine, the secret is not)" 's["ok"] is True' "$OUT"
assert_not_contains "wrong secret: selftest output carries no secret" 'WRONGSECRETKEY' "$OUT"
# Sanity: the same token IS on the wire (the session instance proves signing),
# and the badsecret instance did reach the fixture — it is the output that is
# clean, not the request that was skipped.
OUT=$(mcp_call_on "$SESSION_BASE" sts_get_caller_identity '{}')
assert_sc "session instance still signs correctly with its own token" 's["account"] == "123456789012"' "$OUT"

echo "== clock skew retry =="
OUT=$(mcp_call_on "$SKEW_BASE" sts_get_caller_identity '{}')
assert_sc "RequestExpired is corrected from the Date header and retried once" 's["account"] == "123456789012"' "$OUT"
OUT=$(mcp_call_on "$SKEW_BASE" sigv4_selftest '{}')
assert_sc "the cached clock offset is about +20 minutes" '1150 <= s["clock_offset_seconds"] <= 1250' "$OUT"
OUT=$(mcp_call_on "$SKEW_BASE" s3_list_buckets '{}')
assert_sc "subsequent calls on the warm instance use the cached offset" 's["count"] == 3' "$OUT"
OUT=$(mcp_call_on "$SKEW_BASE" check_auth '{}')
assert_sc "check_auth reports the cached offset" '1150 <= s["clock_offset_seconds"] <= 1250 and s["status"] == "ok" and s["clock_skew_detected"] is True' "$OUT"
assert_contains "check_auth text tells the operator to fix the host clock" 'host clock is' "$OUT"
OUT=$(mcp_call check_auth '{}')
assert_sc "no skew note on an instance whose clock is fine" 's["clock_skew_detected"] is False' "$OUT"
assert_not_contains "no host-clock note when the clock is fine" 'host clock is' "$OUT"

echo "== missing secret (guard instance) =="
for TOOL_ARGS in 'check_auth {}' 'sts_get_caller_identity {}' 's3_list_buckets {}' 's3_list_objects {"bucket":"alpha-bucket"}' 's3_get_object {"bucket":"alpha-bucket","key":"test.txt"}' 'ec2_describe_instances {}' 'lambda_list_functions {}' 'lambda_invoke {"function_name":"echo","invocation_type":"DryRun"}' 'cloudwatch_logs_describe_log_groups {}' 'cloudwatch_logs_filter_log_events {"log_group":"app/prod"}'; do
  TOOL=${TOOL_ARGS%% *}
  ARGS=${TOOL_ARGS#* }
  OUT=$(mcp_call_on "$GUARD_BASE" "$TOOL" "$ARGS")
  assert_contains "$TOOL without the secret is an actionable error" 'AWS_ACCESS_KEY_ID is not set' "$OUT"
  assert_contains "$TOOL without the secret names the secret ref" 'aws-cloud-mcp-access-key-id' "$OUT"
done
OUT=$(mcp_call_on "$GUARD_BASE" check_auth '{}')
assert_sc "check_auth without the secret reports status missing" 's["status"] == "missing" and "cosmonic_set_secret" in s["remediation"]' "$OUT"
COUNT_BEFORE=$(curl -sS --max-time 10 "${FIXTURE}/_count" | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])' 2>/dev/null)
OUT=$(mcp_call_on "$GUARD_BASE" s3_list_buckets '{}')
COUNT_AFTER=$(curl -sS --max-time 10 "${FIXTURE}/_count" | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])' 2>/dev/null)
# Both must be integers (an unreachable fixture would leave them empty) and
# the count must not have moved.
if [[ "$COUNT_BEFORE" =~ ^[0-9]+$ ]] && [[ "$COUNT_AFTER" =~ ^[0-9]+$ ]] && [ "$COUNT_BEFORE" -gt 0 ] && [ "$COUNT_BEFORE" -eq "$COUNT_AFTER" ]; then
  pass "missing secret makes no upstream call (verified count $COUNT_BEFORE unchanged)"
else
  fail "missing secret makes no upstream call" "fixture count before=[$COUNT_BEFORE] after=[$COUNT_AFTER]"
fi

echo "== write gate =="
OUT=$(mcp_call_on "$GUARD_BASE" s3_put_object '{"bucket":"alpha-bucket","key":"x","body":"y"}')
assert_sc "s3_put_object refused when AWS_ALLOW_WRITES is unset (before credentials)" 's["error"] == "WritesDisabled" and "AWS_ALLOW_WRITES" in s["message"] and s["retryable"] is False' "$OUT"
assert_contains "write refusal says so in plain text" 'writes are disabled' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" lambda_invoke '{"function_name":"echo"}')
assert_sc "lambda_invoke RequestResponse refused without writes" 's["error"] == "WritesDisabled"' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" lambda_invoke '{"function_name":"echo","invocation_type":"Event"}')
assert_sc "lambda_invoke Event refused without writes" 's["error"] == "WritesDisabled"' "$OUT"
OUT=$(mcp_call_on "$GUARD_BASE" lambda_invoke '{"function_name":"echo","invocation_type":"DryRun"}')
assert_contains "lambda_invoke DryRun is not gated (fails later on the missing secret)" 'AWS_ACCESS_KEY_ID is not set' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" s3_put_object '{"bucket":"alpha-bucket","key":"x","body":"y"}')
assert_sc "AWS_ALLOW_WRITES=false refuses writes even with credentials" 's["error"] == "WritesDisabled"' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" lambda_invoke '{"function_name":"echo","invocation_type":"DryRun"}')
assert_sc "DryRun works on a read-only deployment (204)" 's["status_code"] == 204 and s["invocation_type"] == "DryRun"' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" check_auth '{}')
assert_sc "check_auth reports writes disabled" 's["writes_enabled"] is False' "$OUT"

echo "== s3_list_buckets =="
OUT=$(mcp_call s3_list_buckets '{}')
assert_sc "lists buckets with regions" 's["count"] == 3 and {b["name"]: b["region"] for b in s["buckets"]}["beta.bucket"] == "eu-west-1" and s["next_continuation_token"] is None' "$OUT"
assert_last "ListBuckets is a signed GET / with the default max-buckets" 'd["service"] == "s3" and d["raw_path"] == "/" and d["query"] == "max-buckets=100" and d["signed_headers"] == "host;x-amz-content-sha256;x-amz-date" and d["headers"]["x-amz-content-sha256"] == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"'
OUT=$(mcp_call s3_list_buckets '{"max_buckets":50000}')
assert_last "max_buckets clamped to 10000" 'd["query"] == "max-buckets=10000"'
OUT=$(mcp_call s3_list_buckets '{"max_buckets":0}')
assert_sc "max_buckets clamped to 1 and a continuation token comes back" 's["count"] == 1 and s["next_continuation_token"] == "bkt-next"' "$OUT"
OUT=$(mcp_call s3_list_buckets '{"continuation_token":"bkt-next","max_buckets":1}')
assert_sc "continuation token round-trips" 's["count"] >= 1' "$OUT"
assert_last "continuation-token is sent percent-encoded and sorted" 'd["query"] == "continuation-token=bkt-next&max-buckets=1"'
OUT=$(mcp_call s3_list_buckets '{"prefix":"beta"}')
assert_sc "prefix filters buckets" 's["count"] == 1 and s["prefix"] == "beta"' "$OUT"
OUT=$(mcp_call s3_list_buckets '{"prefix":"../x y&z=1"}')
assert_sc "traversal/injection-shaped prefix is harmless" 's["count"] == 0' "$OUT"
assert_last "prefix is RFC 3986 percent-encoded" 'd["query"] == "max-buckets=100&prefix=..%2Fx%20y%26z%3D1"'
OUT=$(mcp_call s3_list_buckets '{"bucket_region":"us-east-1"}')
assert_last "bucket-region is forwarded" '"bucket-region=us-east-1" in d["query"]'
OUT=$(mcp_call s3_list_buckets '{"bucket_region":"US"}')
assert_contains "malformed bucket_region refused locally" 'invalid arguments' "$OUT"
OUT=$(mcp_call s3_list_buckets '{"prefix":"a\u0001b"}')
assert_contains "control characters in prefix refused" 'control characters' "$OUT"
OUT=$(mcp_call s3_list_buckets '{"max_buckets":"ten"}')
assert_contains "ill-typed max_buckets is a tool error" '"isError":true' "$OUT"

echo "== s3_list_objects =="
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","delimiter":"/"}')
assert_sc "delimiter collapses folders into common_prefixes" 'set(s["common_prefixes"]) == {"photos/", "docs/"} and "a b+c?d.txt" in [o["key"] for o in s["objects"]] and s["is_truncated"] is False and s["delimiter"] == "/"' "$OUT"
assert_last "ListObjectsV2 path-style with sorted query" 'd["raw_path"] == "/alpha-bucket" and d["query"] == "delimiter=%2F&list-type=2&max-keys=100"'
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","prefix":"photos/"}')
assert_sc "unicode keys survive XML parsing" 's["objects"][0]["key"] == "photos/2006/Ünïcode ☃.jpg" and s["objects"][0]["size"] == 11 and s["objects"][0]["etag"].startswith("\"")' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","max_keys":5000}')
assert_last "max_keys clamped to 1000" '"max-keys=1000" in d["query"]'
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","max_keys":2}')
assert_sc "small page is truncated with a continuation token" 's["is_truncated"] is True and s["next_continuation_token"] == "obj-next" and s["key_count"] == 2' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","max_keys":2,"continuation_token":"obj-next"}')
assert_sc "continuation token fetches the next page" 's["key_count"] >= 1' "$OUT"
assert_last "continuation-token forwarded" '"continuation-token=obj-next" in d["query"]'
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","max_keys":-3}')
assert_last "negative max_keys clamped to 1" '"max-keys=1" in d["query"]'
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","start_after":"docs/readme.md"}')
assert_last "start-after forwarded encoded" '"start-after=docs%2Freadme.md" in d["query"]'
OUT=$(mcp_call s3_list_objects '{"bucket":"alpha-bucket","prefix":"../../etc"}')
assert_sc "traversal-shaped prefix is just a prefix" 's["key_count"] == 0' "$OUT"
assert_last "traversal-shaped prefix encoded, never normalized" '"prefix=..%2F..%2Fetc" in d["query"]'
OUT=$(mcp_call s3_list_objects '{"bucket":"A"}')
assert_contains "bucket too short refused" '3..=63' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"my/bucket"}')
assert_contains "bucket with slash refused" 'invalid arguments' "$OUT"
OUT=$(mcp_call s3_list_objects "{\"bucket\":\"$(printf 'b%.0s' $(seq 1 70))\"}")
assert_contains "bucket too long refused" '3..=63' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"arn:aws:s3:::x"}')
assert_contains "bucket ARN refused" 'invalid arguments' "$OUT"
OUT=$(mcp_call s3_list_objects '{}')
assert_contains "missing bucket is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"wrong-region"}')
assert_sc "wrong region: 301 reports the bucket's region" 's["error"] == "PermanentRedirect" and s["http_status"] == 301 and "region=eu-west-1" in s["message"] and s["retryable"] is False' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"wrong-region","region":"eu-west-1"}')
assert_sc "retrying with the reported region works" 's["region"] == "eu-west-1" and s["key_count"] > 0' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"nope"}')
assert_sc "NoSuchBucket points at s3_list_buckets" 's["error"] == "NoSuchBucket" and s["http_status"] == 404 and "s3_list_buckets" in s["message"]' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"denied"}')
assert_sc "AccessDenied names the IAM action and is not retryable" 's["error"] == "AccessDenied" and s["retryable"] is False and "s3:ListBucket" in s["message"] and "IAM policy" in s["message"]' "$OUT"
OUT=$(mcp_call s3_list_objects '{"bucket":"slow"}')
assert_sc "SlowDown is retryable with back-off advice" 's["error"] == "SlowDown" and s["http_status"] == 503 and s["retryable"] is True and "back off" in s["message"]' "$OUT"

echo "== s3_get_object =="
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"test.txt"}')
assert_sc "reads a whole small object (206 covering everything is not truncated)" 's["content_length"] == 443 and s["returned_bytes"] == 443 and s["truncated"] is False and s["body"].startswith("The quick") and s["binary"] is False and s["etag"]' "$OUT"
assert_last "GET object sends the default 64 KiB Range and signs it" 'd["headers"]["range"] == "bytes=0-65535" and d["signed_headers"] == "host;range;x-amz-content-sha256;x-amz-date"'
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"test.txt","max_bytes":100}')
assert_sc "partial read is flagged truncated with the full length" 's["truncated"] is True and s["returned_bytes"] == 100 and s["content_length"] == 443 and len(s["body"]) == 100 and "max_bytes" in s["note"]' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"test.txt","max_bytes":5000000}')
assert_last "max_bytes clamped to 1 MiB" 'd["headers"]["range"] == "bytes=0-1048575"'
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"test.txt","max_bytes":0}')
assert_sc "max_bytes clamped to 1" 's["returned_bytes"] == 1 and s["body"] == "T"' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"multibyte.txt","max_bytes":7}')
assert_sc "range cutting a multibyte char keeps the valid prefix (no panic)" 's["body"] == "☃☃" and s["returned_bytes"] == 6 and s["truncated"] is True' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"binary.bin"}')
assert_sc "non-UTF-8 object returns metadata only" 's["binary"] is True and "body" not in s and s["content_length"] == 24 and "binary" in s["note"]' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"empty"}')
assert_sc "zero-byte object: 416 retried without Range" 's["content_length"] == 0 and s["body"] == "" and s["truncated"] is False' "$OUT"
assert_last "the retry carries no Range header" '"range" not in d["headers"]'
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"photos/2006/Ünïcode ☃.jpg"}')
assert_sc "unicode key read verbatim" 's["body"] == "snowman ☃" and s["key"] == "photos/2006/Ünïcode ☃.jpg"' "$OUT"
assert_last "unicode key percent-encoded per segment with / preserved" 'd["raw_path"] == "/alpha-bucket/photos/2006/%C3%9Cn%C3%AFcode%20%E2%98%83.jpg"'
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"a b+c?d.txt"}')
assert_sc "space/plus/question-mark key works" 's["body"] == "plus and question"' "$OUT"
assert_last "space, + and ? are encoded (never a query separator)" 'd["raw_path"] == "/alpha-bucket/a%20b%2Bc%3Fd.txt" and d["query"] == ""'
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"test.txt","version_id":"v2"}')
assert_sc "version_id is returned" 's["version_id"] == "v2"' "$OUT"
assert_last "versionId is a signed query parameter" 'd["query"] == "versionId=v2"'
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"missing"}')
assert_sc "NoSuchKey points at s3_list_objects" 's["error"] == "NoSuchKey" and s["http_status"] == 404 and "s3_list_objects" in s["message"]' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"glacier"}')
assert_sc "InvalidObjectState explains the archive tier" 's["error"] == "InvalidObjectState" and "restore" in s["message"]' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"../../etc/passwd"}')
assert_contains "dot-dot key segment refused" "'..'" "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":""}')
assert_contains "empty key refused" 'key must not be empty' "$OUT"
OUT=$(mcp_call s3_get_object "{\"bucket\":\"alpha-bucket\",\"key\":\"$(printf 'k%.0s' $(seq 1 1100))\"}")
assert_contains "key over 1024 bytes refused" '1024' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"a\u000ab"}')
assert_contains "key with control characters refused" 'control characters' "$OUT"
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"test.txt","max_bytes":"lots"}')
assert_contains "ill-typed max_bytes is a tool error" '"isError":true' "$OUT"

echo "== s3_put_object (writes enabled) =="
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"new dir/hello ☃.txt","body":"hello ☃"}')
assert_sc "put returns etag and byte count" 's["bytes_written"] == 9 and s["etag"].startswith("\"") and s["content_type"] == "text/plain; charset=utf-8"' "$OUT"
assert_last "PUT signs the real payload hash, content-type and encodes the key" 'd["method"] == "PUT" and d["raw_path"] == "/alpha-bucket/new%20dir/hello%20%E2%98%83.txt" and d["signed_headers"] == "content-type;host;x-amz-content-sha256;x-amz-date" and d["headers"]["x-amz-content-sha256"] != "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" and d["headers"]["content-length"] == "9"'
OUT=$(mcp_call s3_get_object '{"bucket":"alpha-bucket","key":"new dir/hello ☃.txt"}')
assert_sc "put/get round-trips body and content type" 's["body"] == "hello ☃" and s["content_type"] == "text/plain; charset=utf-8"' "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"data.json","body":"{\"a\":1}","content_type":"application/json"}')
assert_sc "custom content_type stored" 's["content_type"] == "application/json"' "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"test.txt","body":"x","if_none_match":true}')
assert_sc "if_none_match on an existing key is 412 with advice" 's["error"] == "PreconditionFailed" and s["http_status"] == 412 and "if_none_match" in s["message"] and s["retryable"] is False' "$OUT"
assert_last "If-None-Match: * is sent and signed" 'd["headers"]["if-none-match"] == "*" and "if-none-match" in d["signed_headers"]'
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"brand-new-'$$'.txt","body":"x","if_none_match":true}')
assert_sc "if_none_match on a new key succeeds" 's["bytes_written"] == 1' "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"versioned","key":"v.txt","body":"x"}')
assert_sc "version id surfaces on versioned buckets" 's["version_id"] == "v1fixture"' "$OUT"
OUT=$(mcp_call_big s3_put_object '{"bucket":"alpha-bucket","key":"big","body":"x"*(2*1024*1024)}')
assert_contains "2 MiB body refused locally" 'at most 1048576 bytes' "$OUT"
assert_contains "2 MiB body refusal is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"x","body":"y","content_type":"text/html\u000d\u000aX-Injected: 1"}')
assert_contains "header-injection content_type refused" 'content_type must be' "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"x","body":"y","content_type":"nonsense"}')
assert_contains "content_type without a slash refused" 'content_type must be' "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"denied","key":"x","body":"y"}')
assert_sc "put AccessDenied mapped" 's["error"] == "AccessDenied" and s["http_status"] == 403' "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"../x","body":"y"}')
assert_contains "put with dot-dot key refused" "'..'" "$OUT"
OUT=$(mcp_call s3_put_object '{"bucket":"alpha-bucket","key":"x"}')
assert_contains "put without body is a tool error" '"isError":true' "$OUT"

echo "== ec2_describe_instances =="
OUT=$(mcp_call ec2_describe_instances '{}')
assert_sc "flattens reservations into instances with Name tag and tags map" 's["count"] == 2 and s["instances"][0]["instance_id"] == "i-0abc123def4567890" and s["instances"][0]["name"] == "web-1" and s["instances"][0]["state"] == "running" and s["instances"][0]["public_ip"] == "54.1.2.3" and s["instances"][0]["tags"]["env"] == "e2e <x>" and s["instances"][1]["public_ip"] is None and s["next_token"] == "next-2"' "$OUT"
assert_last "DescribeInstances form body with the default MaxResults" 'd["service"] == "ec2" and d["body"] == "Action=DescribeInstances&Version=2016-11-15&MaxResults=100" and d["signed_headers"] == "content-type;host;x-amz-date"'
OUT=$(mcp_call ec2_describe_instances '{"filters":{"instance-state-name":["stopped"]}}')
assert_sc "filters narrow the result" 's["count"] == 1 and s["instances"][0]["state"] == "stopped"' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"filters":{"tag:Name":"web*'"'"'; DROP TABLE instances;--","vpc-id":["vpc-0bb"]}}')
assert_last "filter names/values are form-encoded (injection-shaped value inert)" '"Filter.1.Name=tag%3AName&Filter.1.Value.1=web%2A%27%3B%20DROP%20TABLE%20instances%3B--&Filter.2.Name=vpc-id&Filter.2.Value.1=vpc-0bb" in d["body"]'
OUT=$(mcp_call ec2_describe_instances '{"instance_ids":["i-0abc123def4567890"],"max_results":50}')
assert_sc "instance_ids select and MaxResults is dropped (EC2 rejects the pair)" 's["count"] == 1 and s["next_token"] is None' "$OUT"
assert_last "InstanceId.1 sent without MaxResults" '"InstanceId.1=i-0abc123def4567890" in d["body"] and "MaxResults" not in d["body"]'
OUT=$(mcp_call ec2_describe_instances '{"instance_ids":["i-0000000000000000f"]}')
assert_sc "InvalidInstanceID.NotFound mapped with advice" 's["error"] == "InvalidInstanceID.NotFound" and "filters" in s["message"]' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"instance_ids":["foo"]}')
assert_contains "malformed instance id refused locally" 'i-xxxxxxxx' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"instance_ids":["i-ABCDEF01"]}')
assert_contains "uppercase hex instance id refused" 'lowercase hex' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"max_results":3}')
assert_last "max_results clamped up to EC2's minimum of 5" '"MaxResults=5" in d["body"]'
OUT=$(mcp_call ec2_describe_instances '{"max_results":5000}')
assert_last "max_results clamped to 1000" '"MaxResults=1000" in d["body"]'
OUT=$(mcp_call ec2_describe_instances '{"next_token":"next-2"}')
assert_sc "next_token page has no further token" 's["next_token"] is None' "$OUT"
assert_last "NextToken forwarded" '"NextToken=next-2" in d["body"]'
OUT=$(mcp_call_big ec2_describe_instances '{"instance_ids":["i-%017x" % i for i in range(101)]}')
assert_contains "101 instance ids refused" 'at most 100' "$OUT"
OUT=$(mcp_call_big ec2_describe_instances '{"filters":{"f%d" % i: ["x"] for i in range(21)}}')
assert_contains "21 filters refused" 'at most 20 filters' "$OUT"
OUT=$(mcp_call_big ec2_describe_instances '{"filters":{"tag:Name":["v%d" % i for i in range(21)]}}')
assert_contains "21 filter values refused" 'at most 20' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"filters":{"tag:Name":[{"x":1}]}}')
assert_contains "non-string filter values refused" 'must be strings' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"filters":{"tag:Name":[]}}')
assert_contains "empty filter value list refused" 'no values' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"filters":"running"}')
assert_contains "ill-typed filters is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call ec2_describe_instances '{"region":"eu-central-1"}')
assert_last "region override reaches the EC2 credential scope" 'd["region"] == "eu-central-1"'

echo "== lambda_list_functions =="
OUT=$(mcp_call lambda_list_functions '{}')
assert_sc "lists functions with summary fields" 's["count"] == 2 and s["functions"][0]["name"] == "echo" and s["functions"][0]["runtime"] == "python3.12" and s["functions"][0]["memory_mb"] == 128 and s["functions"][0]["architectures"] == ["arm64"] and s["next_marker"] is None' "$OUT"
assert_last "ListFunctions GET with MaxItems=50 signed (host;x-amz-date)" 'd["service"] == "lambda" and d["method"] == "GET" and d["raw_path"] == "/2015-03-31/functions" and d["query"] == "MaxItems=50" and d["signed_headers"] == "host;x-amz-date"'
OUT=$(mcp_call lambda_list_functions '{"max_items":200}')
assert_last "max_items clamped to 50" 'd["query"] == "MaxItems=50"'
OUT=$(mcp_call lambda_list_functions '{"max_items":1}')
assert_sc "small page returns next_marker" 's["count"] == 1 and s["next_marker"] == "mk-2"' "$OUT"
OUT=$(mcp_call lambda_list_functions '{"max_items":1,"marker":"mk-2"}')
assert_sc "marker fetches the next page" 's["count"] == 1 and s["functions"][0]["name"] == "boom"' "$OUT"
assert_last "Marker forwarded" 'd["query"] == "Marker=mk-2&MaxItems=1"'
OUT=$(mcp_call lambda_list_functions '{"include_versions":true}')
assert_sc "include_versions adds published versions" 's["count"] == 3 and s["functions"][2]["version"] == "1"' "$OUT"
assert_last "FunctionVersion=ALL sent" '"FunctionVersion=ALL" in d["query"]'
OUT=$(mcp_call lambda_list_functions '{"marker":"a\u0009b"}')
assert_contains "marker with control characters refused" 'control characters' "$OUT"

echo "== lambda_invoke (writes enabled) =="
OUT=$(mcp_call lambda_invoke '{"function_name":"echo","payload":{"a":1,"unicode":"☃"}}')
assert_sc "synchronous invoke echoes the payload with version and log tail" 's["status_code"] == 200 and s["payload"] == {"a": 1, "unicode": "☃"} and s["executed_version"] == "$LATEST" and s["function_error"] is None and "START RequestId" in s["log_tail"] and s["payload_truncated"] is False' "$OUT"
assert_contains "invoke is not an error" '"isError":false' "$OUT"
assert_last "invoke POST signs content-type and the x-amz-* invocation headers" 'd["method"] == "POST" and d["raw_path"] == "/2015-03-31/functions/echo/invocations" and d["signed_headers"] == "content-type;host;x-amz-date;x-amz-invocation-type;x-amz-log-type" and d["headers"]["x-amz-invocation-type"] == "RequestResponse" and d["headers"]["x-amz-log-type"] == "Tail" and json.loads(d["body"]) == {"a": 1, "unicode": "☃"}'
OUT=$(mcp_call lambda_invoke '{"function_name":"echo"}')
assert_sc "invoke without payload sends {}" 's["payload"] == {}' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"boom"}')
assert_sc "a function that throws is 200 with function_error and the error payload" 's["status_code"] == 200 and s["function_error"] == "Unhandled" and s["payload"]["errorMessage"] == "boom went off" and s["payload"]["errorType"] == "RuntimeError"' "$OUT"
assert_contains "thrown function error is a success result (not isError)" '"isError":false' "$OUT"
assert_contains "thrown function error is called out in the text" 'raised a Unhandled error' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"echo","invocation_type":"Event","payload":{"x":1}}')
assert_sc "Event invocation is 202 with no payload" 's["status_code"] == 202 and s["payload"] is None and s["log_tail"] is None' "$OUT"
assert_last "Event sets X-Amz-Log-Type: None" 'd["headers"]["x-amz-invocation-type"] == "Event" and d["headers"]["x-amz-log-type"] == "None"'
OUT=$(mcp_call lambda_invoke '{"function_name":"echo","invocation_type":"DryRun"}')
assert_sc "DryRun is 204" 's["status_code"] == 204' "$OUT"
assert_contains "DryRun text explains it" 'dry run ok' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"echo","qualifier":"prod","log_tail":false}')
assert_sc "qualifier is honoured and log_tail=false omits the log" 's["executed_version"] == "prod" and s["log_tail"] is None' "$OUT"
assert_last "Qualifier query signed and log type None" 'd["query"] == "Qualifier=prod" and d["headers"]["x-amz-log-type"] == "None"'
OUT=$(mcp_call lambda_invoke '{"function_name":"arn:aws:lambda:us-east-1:123456789012:function:echo","payload":"text"}')
assert_sc "full ARN function name works" 's["status_code"] == 200 and s["payload"] == "text"' "$OUT"
assert_last "ARN colons are %3A on the wire and %253A in the canonical URI" 'd["raw_path"] == "/2015-03-31/functions/arn%3Aaws%3Alambda%3Aus-east-1%3A123456789012%3Afunction%3Aecho/invocations" and d["canonical_request"].splitlines()[1] == "/2015-03-31/functions/arn%253Aaws%253Alambda%253Aus-east-1%253A123456789012%253Afunction%253Aecho/invocations"'
OUT=$(mcp_call lambda_invoke '{"function_name":"missing"}')
assert_sc "ResourceNotFoundException (404, x-amzn-ErrorType) mapped with advice" 's["error"] == "ResourceNotFoundException" and s["http_status"] == 404 and "lambda_list_functions" in s["message"]' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"throttle"}')
assert_sc "TooManyRequestsException is retryable and quotes Retry-After" 's["error"] == "TooManyRequestsException" and s["http_status"] == 429 and s["retryable"] is True and "Retry-After says 1 s" in s["message"]' "$OUT"
OUT=$(mcp_call_big lambda_invoke '{"function_name":"echo","payload":{"blob":"x"*(2*1024*1024)}}')
assert_contains "2 MiB payload refused locally" 'at most 1048576 bytes' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"bad name!"}')
assert_contains "function name with bad characters refused" 'function_name may contain only' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":""}')
assert_contains "empty function name refused" '1..=170' "$OUT"
OUT=$(mcp_call lambda_invoke "{\"function_name\":\"$(printf 'f%.0s' $(seq 1 200))\"}")
assert_contains "over-long function name refused" '1..=170' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"echo","qualifier":"a b"}')
assert_contains "qualifier with a space refused" 'qualifier must be' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"echo","invocation_type":"Bogus"}')
assert_contains "unknown invocation_type is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call lambda_invoke '{"function_name":"echo","payload":"not json but a string"}')
assert_sc "string payload is sent as a JSON string" 's["payload"] == "not json but a string"' "$OUT"

echo "== cloudwatch_logs_describe_log_groups =="
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{}')
assert_sc "lists log groups with retention, class and ISO creation time" 's["count"] == 3 and s["log_groups"][0]["name"] == "/aws/lambda/echo" and s["log_groups"][0]["retention_days"] == 14 and s["log_groups"][1]["retention_days"] is None and s["log_groups"][2]["class"] == "INFREQUENT_ACCESS" and s["log_groups"][0]["creation_time_iso"] == "2024-04-01T19:33:20.000Z"' "$OUT"
assert_contains "never-expiring group is described in text" 'never expires' "$OUT"
assert_last "DescribeLogGroups is JSON 1.1 with the signed target header" 'd["service"] == "logs" and d["signed_headers"] == "content-type;host;x-amz-date;x-amz-target" and d["headers"]["x-amz-target"] == "Logs_20140328.DescribeLogGroups" and d["headers"]["content-type"] == "application/x-amz-json-1.1" and json.loads(d["body"]) == {"limit": 50}'
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"prefix":"/aws/lambda/"}')
assert_sc "prefix filters groups" 's["count"] == 2' "$OUT"
assert_last "logGroupNamePrefix sent" 'json.loads(d["body"])["logGroupNamePrefix"] == "/aws/lambda/"'
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"pattern":"prod"}')
assert_sc "pattern matches substrings (sparse records)" 's["count"] == 1 and s["log_groups"][0]["name"] == "app/prod" and s["log_groups"][0]["retention_days"] is None' "$OUT"
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"prefix":"/aws","pattern":"x"}')
assert_contains "prefix + pattern refused locally" 'mutually exclusive' "$OUT"
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"limit":0}')
assert_sc "limit clamped to 1 with next_token" 's["count"] == 1 and s["next_token"] == "lg-next"' "$OUT"
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"limit":500}')
assert_last "limit clamped to 50" 'json.loads(d["body"])["limit"] == 50'
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"limit":1,"next_token":"lg-next"}')
assert_sc "next_token fetches the rest" 's["count"] == 2' "$OUT"
assert_last "nextToken sent" 'json.loads(d["body"])["nextToken"] == "lg-next"'
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"log_group_class":"INFREQUENT_ACCESS"}')
assert_sc "log_group_class filter" 's["count"] == 1' "$OUT"
assert_last "logGroupClass sent verbatim" 'json.loads(d["body"])["logGroupClass"] == "INFREQUENT_ACCESS"'
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"log_group_class":"FOO"}')
assert_contains "unknown log_group_class is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call cloudwatch_logs_describe_log_groups '{"prefix":"bad prefix!"}')
assert_contains "prefix with illegal characters refused" 'may contain only' "$OUT"

echo "== cloudwatch_logs_filter_log_events =="
NOW_MS=$(python3 -c 'import time; print(int(time.time()*1000))')
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","last_minutes":5,"filter_pattern":"ERROR"}')
assert_sc "last_minutes window with a pattern returns events" 's["count"] == 3 and "ERROR" in s["events"][0]["message"] and s["events"][0]["time"] == "2024-05-01T12:00:00.000Z" and s["events"][0]["log_stream_name"] == "stream-0" and s["next_token"] is None and s["note"] is None' "$OUT"
assert_last "FilterLogEvents body: name, pattern, default limit, startTime = now-5min" '(lambda b: b["logGroupName"] == "/aws/lambda/echo" and b["filterPattern"] == "ERROR" and b["limit"] == 100 and '"$NOW_MS"' - 6*60*1000 < b["startTime"] <= '"$NOW_MS"' + 60000 and "startFromHead" not in b and "logGroupIdentifier" not in b)(json.loads(d["body"]))'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","start_time":"2024-05-01T12:00:00Z","end_time":"2024-05-01T13:30:00.500+01:00"}')
assert_last "RFC 3339 timestamps (with zone offset) become epoch ms" '(lambda b: b["startTime"] == 1714564800000 and b["endTime"] == 1714566600500)(json.loads(d["body"]))'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","start_time":1714564800,"end_time":1714568400000}')
assert_last "epoch seconds are scaled to milliseconds" '(lambda b: b["startTime"] == 1714564800000 and b["endTime"] == 1714568400000)(json.loads(d["body"]))'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","start_time":"2024-05-02","end_time":"2024-05-01"}')
assert_contains "end before start refused" 'before start_time' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","start_time":"yesterday"}')
assert_contains "unparseable timestamp refused with the expected format" 'RFC 3339' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","newest_first":true}')
assert_contains "newest_first without a start_time refused (CloudWatch rule)" '2024-01-01' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","newest_first":true,"last_minutes":10}')
assert_last "newest_first sends startFromHead=false" 'json.loads(d["body"])["startFromHead"] is False'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","limit":50000}')
assert_last "limit clamped to 10000" 'json.loads(d["body"])["limit"] == 10000'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","limit":0}')
assert_sc "limit clamped to 1 with a next_token" 's["count"] == 1 and s["next_token"] == "ev-next"' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","limit":1,"next_token":"ev-next"}')
assert_sc "next_token continues" 's["count"] == 2' "$OUT"
assert_last "nextToken sent" 'json.loads(d["body"])["nextToken"] == "ev-next"'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","log_stream_names":["a","b"]}')
assert_last "logStreamNames sent" 'json.loads(d["body"])["logStreamNames"] == ["a", "b"]'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","log_stream_name_prefix":"2024/"}')
assert_last "logStreamNamePrefix sent" 'json.loads(d["body"])["logStreamNamePrefix"] == "2024/"'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","log_stream_name_prefix":"x","log_stream_names":["a"]}')
assert_contains "stream prefix + names refused locally" 'mutually exclusive' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","log_stream_names":["a:b"]}')
assert_contains "stream name with a colon refused" "without ':' or '*'" "$OUT"
OUT=$(mcp_call_big cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","log_stream_names":["s%d" % i for i in range(101)]}')
assert_contains "101 stream names refused" 'at most 100' "$OUT"
OUT=$(mcp_call_big cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","filter_pattern":"ERROR " * 1000}')
assert_contains "5 KB filter pattern refused (not silently cut)" 'at most 1024' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","filter_pattern":"{ $.level = \"error\" } \"; DROP TABLE\" -noise ?☃"}')
assert_sc "injection-shaped pattern is sent verbatim as JSON" 's["count"] == 3' "$OUT"
assert_last "filterPattern verbatim in the JSON body" 'json.loads(d["body"])["filterPattern"] == "{ $.level = \"error\" } \"; DROP TABLE\" -noise ?☃"'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"arn:aws:logs:us-east-1:123456789012:log-group:/aws/lambda/echo:*"}')
assert_sc "log group ARN is accepted" 's["count"] == 3' "$OUT"
assert_last "ARN goes out as logGroupIdentifier, not logGroupName" '(lambda b: b["logGroupIdentifier"].startswith("arn:") and "logGroupName" not in b)(json.loads(d["body"]))'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"missing"}')
assert_sc "ResourceNotFoundException (400 JSON) mapped with advice" 's["error"] == "ResourceNotFoundException" and s["http_status"] == 400 and "cloudwatch_logs_describe_log_groups" in s["message"] and s["retryable"] is False' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"throttle"}')
assert_sc "ThrottlingException is retryable with the 5 TPS hint" 's["error"] == "ThrottlingException" and s["retryable"] is True and "5" in s["message"] and "back off" in s["message"]' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"empty-page"}')
assert_sc "empty page with a token carries the keep-paginating note" 's["count"] == 0 and s["next_token"] == "more" and "keep paginating" in s["note"]' "$OUT"
assert_contains "empty-page note is in the text too" 'keep paginating' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"bad name!"}')
assert_contains "log group with illegal characters refused" 'may contain only' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":""}')
assert_contains "empty log group refused" 'must not be empty' "$OUT"
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","last_minutes":999999}')
assert_last "last_minutes clamped to 7 days" '(lambda b: '"$NOW_MS"' - 7*24*60*60*1000 - 60000 < b["startTime"] < '"$NOW_MS"' - 7*24*60*60*1000 + 120000)(json.loads(d["body"]))'
OUT=$(mcp_call cloudwatch_logs_filter_log_events '{"log_group":"/aws/lambda/echo","region":"ap-southeast-2"}')
assert_last "region override reaches the Logs credential scope" 'd["region"] == "ap-southeast-2"'

echo "== server still healthy after the adversarial cases =="
OUT=$(mcp_call sts_get_caller_identity '{}')
assert_sc "primary instance alive" 's["account"] == "123456789012"' "$OUT"

if [ "${E2E_LIVE:-0}" = "1" ]; then
  echo "== live (E2E_LIVE=1) =="
  if [ -n "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "${AWS_SECRET_ACCESS_KEY:-}" ]; then
    LIVE_ARGS=(--env "AWS_ACCESS_KEY_ID=${AWS_ACCESS_KEY_ID}" --env "AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}" --env "AWS_REGION=${AWS_REGION:-us-east-1}")
    [ -n "${AWS_SESSION_TOKEN:-}" ] && LIVE_ARGS+=(--env "AWS_SESSION_TOKEN=${AWS_SESSION_TOKEN}")
    start_instance "$LIVE_PORT" live "${LIVE_ARGS[@]}"
    LIVE_BASE="http://127.0.0.1:${LIVE_PORT}/"
    OUT=$(mcp_call_on "$LIVE_BASE" check_auth '{}')
    assert_sc "live: check_auth ok against AWS" 's["status"] == "ok" and s["identity"]["arn"].startswith("arn:aws")' "$OUT"
    OUT=$(mcp_call_on "$LIVE_BASE" s3_list_buckets '{"max_buckets":5}')
    assert_contains "live: s3_list_buckets answers" '"buckets"' "$OUT"
    OUT=$(mcp_call_on "$LIVE_BASE" cloudwatch_logs_describe_log_groups '{"limit":5}')
    assert_contains "live: cloudwatch_logs_describe_log_groups answers" '"log_groups"' "$OUT"
  else
    fail "live cases" "E2E_LIVE=1 but AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY are not exported"
  fi
fi

guard_tests
mcp_harness_report
