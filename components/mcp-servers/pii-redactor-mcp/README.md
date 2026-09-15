# pii-redactor-mcp

An MCP server that finds and replaces **six specific patterns of sensitive
value** in text (email addresses, US Social Security numbers, North American
phone numbers, Luhn-valid payment card numbers, IPv4 addresses, and AWS access
key IDs), built as a WebAssembly component for Cosmonic Desktop.

It is **pure-compute and zero-egress**: the single `redact` tool does all its
work on-device and the component makes no outbound calls anywhere in its source,
so text handed to it cannot be exfiltrated.

Those are two different guarantees, and only the first is complete. Containment
is a property of the sandbox and holds absolutely. Detection is a property of
six regular expressions and does not. Read [What it does not
catch](#what-it-does-not-catch) before treating this tool's output as safe to
share.

No authentication and no configuration are required.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tool

| Tool | Purpose |
|---|---|
| `redact` | Detect and replace PII in a string, returning the redacted text plus per-category and total counts. |

Parameters:

| Param | Type | Purpose |
|---|---|---|
| `text` | string | The text to redact. Capped at 256 KiB; larger input is rejected with a friendly error. |
| `types` | string[] (optional) | Restrict the scan to specific categories. Omit to redact every category. Unknown names return an error listing the valid types. |

### Categories

Each category has a distinct placeholder:

| Category name | Detects | Placeholder |
|---|---|---|
| `email` | Email addresses | `[REDACTED_EMAIL]` |
| `us_ssn` | US Social Security numbers in 3-2-4 form (`123-45-6789`, `123.45.6789`, or `123 45 6789`), or a bare `123456789` next to an "SSN"/"social security" context word | `[REDACTED_SSN]` |
| `phone` | NANP phone numbers: `(555) 123-4567`, `555-123-4567`, `555.123.4567`, `+1 555 123 4567` | `[REDACTED_PHONE]` |
| `credit_card` | 13–19 digit card numbers, space/dash separated, **validated with the Luhn checksum** | `[REDACTED_CC]` |
| `ipv4` | Dotted-quad IPv4 addresses, each octet 0–255 | `[REDACTED_IP]` |
| `aws_access_key_id` | `AKIA`/`ASIA` + 16 uppercase alphanumerics | `[REDACTED_AWS_KEY]` |

Matching runs in a single left-to-right pass over candidates from all enabled
categories, so overlapping matches never double-count; `email` takes precedence
so an address's own digits are not re-hit as a phone number or card. Patterns
use the [`regex`](https://docs.rs/regex) crate, which runs in guaranteed linear
time (no catastrophic backtracking / ReDoS), compiled once behind a `OnceLock`.

### What it does not catch

Six regular expressions are not a PII classifier. The categories above are the
whole of what this tool detects, and the gaps below are the ones most likely to
matter if you point it at real data. Every example here passes through
**unredacted**:

| Not detected | Example |
|---|---|
| Names, dates of birth, postal addresses | `Margaret Chen, DOB 1974-03-11, 42 Alder Lane` |
| Phone numbers outside North America | `+44 20 7946 0958`, `+33 1 42 68 53 00`, `+91 98765 43210` |
| IPv6 addresses | `2001:0db8:85a3::8a2e:0370:7334` |
| AWS **secret** access keys (only the `AKIA` id half is caught) | `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY` |
| Credentials that are not AWS key IDs | GitHub `ghp_…`, Slack `xoxb-…`, Google `AIzaSy…`, `-----BEGIN RSA PRIVATE KEY-----` |
| Bank and government identifiers outside the US 3-2-4 SSN shape | `IBAN GB29 NWBK 6016 1331 9268 19`, passport numbers |
| Email addresses containing non-ASCII characters | `josé.álvarez@exämple.de` |

It also over-matches in the other direction, rewriting text that is not
sensitive at all:

| Over-matched | Becomes |
|---|---|
| `version 1.2.3.4` | `version [REDACTED_IP]` |
| `netmask is 255.255.255.0` | `[REDACTED_IP]` |
| Three space-separated numeric columns, `100 20 3000` | `[REDACTED_SSN]` |
| Any 13 to 19 digit run that happens to pass the Luhn check, including one embedded in an API token or an order id | `[REDACTED_CC]` |

Use this as a first pass that removes the obvious things, not as the control
that decides whether text is safe to publish.

Every call returns structured JSON, for example:

```json
{
  "redacted": "Email me at [REDACTED_EMAIL] or call [REDACTED_PHONE]. SSN [REDACTED_SSN], card [REDACTED_CC], from [REDACTED_IP], key [REDACTED_AWS_KEY].",
  "counts": {
    "aws_access_key_id": 1,
    "credit_card": 1,
    "email": 1,
    "ipv4": 1,
    "phone": 1,
    "us_ssn": 1
  },
  "total": 6
}
```

## `allowedHosts`: zero egress by design

Unlike an MCP server that reaches an upstream API, this tool reaches **nothing**.
Its work is pure compute, so the workload's outbound allowlist is empty:

```yaml
allowedHosts: []
```

In a deployed Workload egress is deny-by-default, so an empty list and an
absent key both mean the component reaches nothing. Note that `wash dev` reads
the opposite default from `.wash/config.yaml`, where omitting the key means
allow-all, so keep it present there. The stronger guarantee is in the code rather than the
manifest: this component never constructs an outbound request, so there is no
host to add here and none should be added.

## Build

```console
$ wash build
```

Tested with `wash` 2.5 and the Cosmonic Desktop daemon 0.5.x. The build emits a
WASI p3 component at `target/wasm32-wasip2/release/pii_redactor_mcp.wasm`.

## Deploy on Cosmonic Desktop

Deployment is via [Cosmonic Desktop](https://cosmonic.com/docs/desktop): apply
[`deploy/workload.yaml`](deploy/workload.yaml) for the published image, or
promote a local build and apply the project's
[`.wash/config.yaml`](.wash/config.yaml) workload settings. Then call it through
the ingress. In the stateless 2026-07-28 transport, each request stands alone,
so `tools/list` and `tools/call` carry the `Mcp-Method`/`Mcp-Name` headers and a
`_meta` block:

```console
$ curl -X POST http://127.0.0.1:8200/ \
    -H 'Host: pii-redactor-mcp.localhost.cosmonic.sh' \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: redact' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"redact","arguments":{"text":"Email me at jane.doe@example.com or call (555) 123-4567."},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

See [`scripts/smoke.sh`](scripts/smoke.sh) for a full `tools/list` + `redact`
walk-through (including a Luhn-invalid negative case and a `types` filter).

## Configuration

| Env var | Purpose |
|---|---|
| `RUST_LOG` | Log level (default `info`). |
| `MCP_ALLOWED_HOSTS` | DNS-rebinding guard for the ingress Host header; must list the workload's `host` (`pii-redactor-mcp.localhost.cosmonic.sh`, `pii-redactor-mcp.localhost`). |
