# threat-intel-mcp

An MCP server that **looks up known vulnerabilities for open-source software**
from the [OSV](https://osv.dev) (Open Source Vulnerabilities) database, built as
a WebAssembly component for Cosmonic Desktop. Every tool call is an **outbound
HTTPS request** to the OSV API, and the workload's `allowedHosts` policy grants
exactly **one** host, `api.osv.dev`. The tool can reach nothing else; that
allowlist is the egress boundary.

No authentication is required. The OSV API is public and needs no API key.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `lookup_package_vulnerabilities` | Find known vulnerabilities for a package (optionally a specific version). |
| `get_vulnerability` | Fetch the full advisory record for one vulnerability id or alias. |

Parameters:

| Tool | Param | Type | Purpose |
|---|---|---|---|
| `lookup_package_vulnerabilities` | `ecosystem` | string | OSV ecosystem name: `npm`, `PyPI`, `crates.io`, `Go`, `Maven`, `RubyGems`, `NuGet`, … (case-sensitive, OSV's spelling). |
| | `package` | string | Package name within that ecosystem (e.g. `jinja2`, `log4j-core`). |
| | `version` | string (optional) | Exact version to filter to (e.g. `2.4.1`). Omit for every known advisory. |
| `get_vulnerability` | `id` | string | OSV id or alias: `GHSA-…`, `CVE-…`, `RUSTSEC-…`, `PYSEC-…`, `GO-…`. |

`lookup_package_vulnerabilities` POSTs to `/v1/query` and returns each matching
advisory's `id`, `summary`, `aliases` (CVE/GHSA cross-references), CVSS
`severity`, affected version ranges, and reference URLs, or a clear
"no known vulnerabilities" result. `get_vulnerability` GETs `/v1/vulns/{id}` and
returns the full record trimmed to useful fields (`details` capped at ~4 KB).
Every tool returns structured JSON, for example a lookup:

```json
{
  "ecosystem": "PyPI",
  "package": "jinja2",
  "version": "2.4.1",
  "vulnerable": true,
  "count": 3,
  "vulnerabilities": [
    {
      "id": "GHSA-462w-v97r-4m45",
      "summary": "Jinja2 sandbox escape via string formatting",
      "aliases": ["CVE-2019-10906"],
      "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }],
      "affected_ranges": ["PyPI/jinja2: >= 0, fixed in 2.10.1"],
      "references": ["https://github.com/pallets/jinja/..."]
    }
  ]
}
```

## `allowedHosts`: the egress boundary

The tools reach a host only if it is in the workload's outbound `allowedHosts`
list. The entire OSV API lives under a single host, so the list is exactly one
entry:

```yaml
allowedHosts:
  - api.osv.dev
```

Empty is deny-all. Because there is only ever one upstream, you should not need
to widen it.

## Build

```console
$ wash build
```

Tested with `wash` 2.5 and the Cosmonic Desktop daemon 0.5.x. The build emits a
WASI p3 component at `target/wasm32-wasip2/release/threat_intel_mcp.wasm`.

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
    -H 'Host: threat-intel-mcp.localhost.cosmonic.sh' \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: lookup_package_vulnerabilities' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"lookup_package_vulnerabilities","arguments":{"ecosystem":"PyPI","package":"jinja2","version":"2.4.1"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

See [`scripts/smoke.sh`](scripts/smoke.sh) for a full `tools/list` + both-tools
walk-through.

## Configuration

| Env var | Purpose |
|---|---|
| `RUST_LOG` | Log level (default `info`). |
| `MCP_ALLOWED_HOSTS` | DNS-rebinding guard for the ingress Host header; must list the workload's `host` (`threat-intel-mcp.localhost.cosmonic.sh`, `threat-intel-mcp.localhost`). |
| `MCP_OUTBOUND_MAX_BYTES` | Upper bound on the buffered outbound response body (default 4 MiB). |
| `MCP_OUTBOUND_TIMEOUT_MS` | Per-request outbound deadline (default 30000). |
