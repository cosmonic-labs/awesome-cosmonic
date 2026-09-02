# SigV4 notes (for diagnosing signature failures and for maintainers)

Supporting file of the `aws-cloud-mcp` skill, served at
`skill://aws-cloud-mcp/references/SIGV4.md`. Signing is done inside the
component from scratch (`hmac` + `sha2`), with no AWS SDK. `sigv4_selftest`
reproduces the four S3 example signatures AWS publishes with the documented
example credentials (`AKIAIOSFODNN7EXAMPLE`, date `20130524T000000Z`):

| Case | Request | Expected signature |
|---|---|---|
| `get-object-range` | `GET /test.txt`, `Range: bytes=0-9` | `f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41` |
| `put-object` | `PUT /test%24file.text`, body "Welcome to Amazon S3." | `98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd` |
| `get-bucket-lifecycle` | `GET /?lifecycle` | `fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543` |
| `list-objects` | `GET /?max-keys=2&prefix=J` | `34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7` |

A pass proves the algorithm; it says nothing about the registered secret.

## What the component signs

- `Authorization: AWS4-HMAC-SHA256 Credential=<key id>/<yyyymmdd>/<region>/<service>/aws4_request, SignedHeaders=…, Signature=…`
- Service scope names: `sts`, `s3`, `ec2`, `lambda`, `logs`.
- Signed headers: `host`, `x-amz-date`, plus `x-amz-content-sha256` (S3
  only, always the real hex SHA-256 of the body — `UNSIGNED-PAYLOAD` is
  never used), `x-amz-security-token` (when a session token is configured),
  `content-type` (when a body is sent), `x-amz-target` (Logs),
  `x-amz-invocation-type` / `x-amz-log-type` (Lambda invoke), `range` (S3
  get), `if-none-match` (S3 put). `User-Agent` and `Content-Length` are sent
  unsigned.
- Canonical URI: S3 signs the wire path once-encoded (`a b` → `a%20b`,
  `/` kept); every other service double-encodes (a Lambda ARN colon is
  `%3A` on the wire and `%253A` in the canonical request).
- Canonical query: every pair URI-encoded with the AWS `UriEncode` set
  (`A-Za-z0-9-_.~` pass, everything else `%XX` uppercase), sorted by key
  then value; the same string goes on the wire, so signed and sent can never
  differ. A valueless key becomes `key=`.
- Query API bodies (STS, EC2) are `application/x-www-form-urlencoded;
  charset=utf-8` with RFC 3986 encoding (space → `%20`, `*` → `%2A`, `:` →
  `%3A`) and hashed as sent; JSON 1.1 (Logs) uses
  `application/x-amz-json-1.1` exactly.
- The `Host` value signed is the URL authority, including a non-default
  port (`127.0.0.1:9672` for a fixture, `host.wasmcloud.internal:9000` for
  MinIO).
- Credential-scope date = the UTC date of `x-amz-date`.

## Clock skew

AWS accepts requests whose `x-amz-date` is within 15 minutes of its own
clock. On `RequestTimeTooSkewed` (S3), `SignatureDoesNotMatch: Signature
expired: <x-amz-date> is now earlier than …` (STS), `RequestExpired` (EC2) or
`InvalidSignatureException: Signature expired` (Logs/Lambda) the component
parses the response `Date` header, stores `server − local` in a static that
survives across requests on the warm instance, and retries once.
`check_auth` and `sigv4_selftest` report the cached `clock_offset_seconds`; a
value far from 0 means the host clock is wrong. Restarting the workload
resets it.

## Reading a SignatureDoesNotMatch body

S3 echoes its own `CanonicalRequest` and `StringToSign` in the error body;
the tool error includes the first 600 characters as "AWS computed this
CanonicalRequest". STS, EC2, Lambda and Logs put the same thing in the
message ("The Canonical String for this request should have been …").
Compare it with the rules above: a difference in the header list means an
unsigned `x-amz-*` header; a difference in the URI means an encoding mismatch
(double vs single); identical strings with a different signature mean the
secret is wrong.

The echo contains every signed header verbatim — with temporary credentials
that includes `x-amz-security-token:<the full session token>`. The server
replaces that value with `<redacted>` (and scrubs the configured token and
secret wherever they appear) before the text enters a tool result, so the
line reads `x-amz-security-token:<redacted>`; do not expect to see, or ask
for, the raw token.
