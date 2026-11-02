# MCP servers

[Model Context Protocol](https://modelcontextprotocol.io) (MCP) servers give an
agent tools to call. The servers here run as sandboxed workloads, so a tool
reaches only the network hosts its workload grants instead of having ambient
access to your machine.

Each server is an ordinary component that speaks MCP over HTTP. Nothing about
it is special to the runtime, so it deploys, scales and is versioned like any
other workload.

| Server | What it does | Outbound hosts |
|---|---|---|
| [github-mcp](github-mcp/) | Search and read GitHub repositories, issues and files. | `api.github.com` |
| [gitlab-mcp](gitlab-mcp/) | Search and read GitLab projects, issues and files. | `gitlab.com` |
| [iss-mcp](iss-mcp/) | Who is in space, and where the International Space Station is now. | `api.open-notify.org` |
| [md-html-sanitizer-mcp](md-html-sanitizer-mcp/) | Turn untrusted Markdown or HTML into safe HTML. | none |
| [pii-redactor-mcp](pii-redactor-mcp/) | Redact six patterns of sensitive value from text. | none |
| [threat-intel-mcp](threat-intel-mcp/) | Look up known vulnerabilities in the OSV database. | `api.osv.dev` |
| [web-fetch-mcp](web-fetch-mcp/) | Fetch a URL and return its contents as text. | the hosts you grant |

Every server is published to GHCR as
`ghcr.io/cosmonic-labs/components/<name>`, and each one's
`deploy/workload.yaml` pins the released image by digest, so you can deploy it
on Cosmonic Desktop without building anything.

## Add a server

One directory per hosted server. See [CONTRIBUTING.md](../../CONTRIBUTING.md)
for requirements, add the directory to the matrix in
[`.github/workflows/mcp-servers.yml`](../../.github/workflows/mcp-servers.yml)
so CI builds and publishes it, and add a line to the
[root README](../../README.md#mcp-servers).
