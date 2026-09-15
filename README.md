<div align="center">

<a href="https://cosmonic.com">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".assets/cosmonic-logo-white.svg">
    <img alt="Cosmonic" src=".assets/cosmonic-logo-color.svg" width="320">
  </picture>
</a>

# Awesome Cosmonic

**Community-maintained components, host plugins, workload examples, and tools for [Cosmonic Control](https://cosmonic.com/docs/).**

[![License](https://img.shields.io/badge/license-Apache--2.0-655dc6.svg)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-cosmonic.com-655dc6.svg)](https://cosmonic.com/docs/)
[![Slack](https://img.shields.io/badge/slack-community-655dc6.svg)](https://slack.wasmcloud.com)

</div>

---

[Cosmonic Control](https://cosmonic.com/docs/) is a Kubernetes-native control plane for running microservices, agentic workflows, MCP servers, and other sensitive or untrusted code inside WebAssembly component sandboxes. Workloads are built with [`wash`](https://wasmcloud.com/docs/wash/developer-guide/) and wired to capabilities at runtime rather than at build time. This repository collects what the community has built on top of it.

**Source may be hosted here or linked.** A project can live in this repository as a directory with its own README, license, and build instructions, or stay in its own repository and be listed here with a link. Entries are tagged `(hosted)` or `(linked)` so you know which you are getting. See [CONTRIBUTING.md](CONTRIBUTING.md) to add yours.

For a project hosted here, clone and build it directly:

```console
git clone https://github.com/cosmonic-labs/awesome-cosmonic.git
cd awesome-cosmonic/workload-examples/<project>
wash build
```

## Contents

- [Components](#components)
  - [Kafka](#kafka)
  - [MCP Servers](#mcp-servers)
- [Host Plugins](#host-plugins)
  - [Native Host Plugins](#native-host-plugins)
  - [Host Component Plugins](#host-component-plugins)
- [Workload Examples](#workload-examples)
- [Tools](#tools)
- [Contributing](#contributing)

## Components

Reusable WebAssembly components that implement a WIT interface. Hosted projects live in [`components/`](components/).

- [sandboxed-webhook](components/sandboxed-webhook/) (hosted): Receives a webhook, verifies its HMAC-SHA256 signature, then forwards the payload to exactly one host bounded by `allowedHosts`, so a compromised handler cannot exfiltrate anywhere else. The signing secret is required rather than defaulted, so an unconfigured deployment refuses every request instead of trusting a published constant. Rust component over `wasi:http`.

### Kafka

Starting points for Kafka workloads on `cosmonic:kafka@0.5.0`, one per delivery pattern. Each is a self-contained project with source, a `wkg.lock` pinning the WIT it fetches, and manifests for both Cosmonic and Kubernetes. The manifests point at prebuilt components, so a pattern can be deployed and watched before any of it is built locally. Hosted projects live in [`components/kafka/`](components/kafka/).

- [kafka-handler-consumer](components/kafka/rust/kafka-handler-consumer/) (hosted, recommended): The serverless default. The host keeps Kafka group membership stable while elastic component instances process partition-ordered batches and can scale back to zero.
- [http-kafka-producer](components/kafka/rust/http-kafka-producer/) (hosted): HTTP request in, Kafka record out, through a host-owned, binding-scoped `cosmonic:kafka/producer`. The ingest-gateway shape.
- [kafka-pull-service](components/kafka/rust/kafka-pull-service/) (hosted): A long-running service owning a pull-consumer session, for workloads that need direct assignment, pause, seek, rebalance, or commit control.
- [kafka-transactional](components/kafka/rust/kafka-transactional/) (hosted): The pull service plus transactions, so output records and input offsets commit atomically, giving exactly-once read-process-write.

### MCP Servers

[Model Context Protocol](https://modelcontextprotocol.io) servers built as WebAssembly components, so the tools an agent calls run inside a sandbox rather than with ambient host access. Hosted projects live in [`components/mcp-servers/`](components/mcp-servers/).

- [iss-mcp](components/mcp-servers/iss-mcp/) (hosted): MCP server in Rust that reports who is currently in space and the International Space Station's live position, calling the Open Notify APIs over `wasi:http`. Exports `wasi:http/handler@0.3.0`, built on the `rmcp` SDK with two no-argument tools.
- [mcp-server-template-ts](https://github.com/cosmonic-labs/mcp-server-template-ts) (linked): Template for building an MCP server as a TypeScript component served over `wasi:http`, scaffolded with `wash new`. The dev loop launches the official MCP inspector, and an `openapi2mcp` script generates tools from an OpenAPI specification.
- [github-mcp](components/mcp-servers/github-mcp/) (hosted): Search and read GitHub for an agent, through `search_repositories`, `get_repository`, `list_issues`, and `get_file_contents`. Outbound requests are bounded to `api.github.com` by the workload's `allowedHosts`, and an optional GitHub token (injected from a Cosmonic secret) raises the rate limit and enables private repositories. Rust, rmcp, exports `wasi:http/handler@0.3.0`.
- [web-fetch-mcp](components/mcp-servers/web-fetch-mcp/) (hosted): A `fetch_url` tool that retrieves a URL over HTTP or HTTPS and returns its contents as readable text or the raw body. Outbound requests are bounded by the workload's `allowedHosts` egress allowlist, so the tool can reach only the hosts you grant. Rust, rmcp, exports `wasi:http/handler@0.3.0`.
- [threat-intel-mcp](components/mcp-servers/threat-intel-mcp/) (hosted): Look up known vulnerabilities for open-source software from the [OSV](https://osv.dev) database, through `lookup_package_vulnerabilities` (by ecosystem, package, and optional version) and `get_vulnerability` (by CVE/GHSA/RUSTSEC/PYSEC/GO id). Outbound requests are bounded to `api.osv.dev` by the workload's `allowedHosts`, and no API key is required. Rust, rmcp, exports `wasi:http/handler@0.3.0`.
- [pii-redactor-mcp](components/mcp-servers/pii-redactor-mcp/) (hosted): Redact six specific patterns of sensitive value from text with a single `redact` tool: emails, US SSNs, North American phone numbers, Luhn-validated payment cards, IPv4 addresses, and AWS access key ids, each replaced by a distinct `[REDACTED_*]` placeholder, with per-category counts. Pure compute with **zero egress**: the component never constructs an outbound request, so the text it sees cannot leave the sandbox. Six regular expressions are not a PII classifier, and the README documents what they miss. Rust, rmcp, exports `wasi:http/handler@0.3.0`.
- [md-html-sanitizer-mcp](components/mcp-servers/md-html-sanitizer-mcp/) (hosted): Turn untrusted markdown or HTML into safe HTML. `sanitize_html` runs raw HTML through the [ammonia](https://docs.rs/ammonia) allowlist (dropping `<script>`/`<style>`/`<iframe>`, event-handler attributes, and `javascript:`/`data:` URLs), and `render_markdown` renders CommonMark with [pulldown-cmark](https://docs.rs/pulldown-cmark) and passes the result back through ammonia so embedded raw HTML is neutralized. Pure compute with **zero egress**: the component never constructs an outbound request, so the content it sees cannot leave the sandbox. Rust, rmcp, exports `wasi:http/handler@0.3.0`.
- [gitlab-mcp](components/mcp-servers/gitlab-mcp/) (hosted): Searches and reads GitLab (projects, issues, files) over the GitLab REST API v4, bounded by `allowedHosts` to the single host `gitlab.com`. Runs unauthenticated by default; an optional `GITLAB_TOKEN` secret raises the rate limit and reaches private projects. Rust component exporting `wasi:http/handler@0.3.0`.

## Host Plugins

Cosmonic Control schedules workloads onto [wasmCloud](https://github.com/wasmCloud/wasmCloud) hosts. [Host plugins](https://wasmcloud.com/docs/overview/hosts/plugins) extend a host with an implementation of a WIT world, which is linked to workloads at runtime. They come in two flavors, and a workload cannot tell which one is serving a capability it imports.

### Native Host Plugins

Rust implementations of the [`HostPlugin` trait](https://wasmcloud.com/docs/runtime/creating-host-plugins), linked into the host binary. The right choice when a capability needs direct host resources (filesystem, network, hardware) or has to run with the host's privileges. Hosted projects live in [`host-plugins/native/`](host-plugins/native/).

_Nothing here yet. [Add the first one](CONTRIBUTING.md)._

### Host Component Plugins

Capabilities built as [WebAssembly components](https://wasmcloud.com/docs/runtime/creating-host-component-plugins/) and deployed into a host at runtime as trigger services with a capability ingress, so you ship, version, and sandbox them like any other component. Currently opt-in via the `host-component-plugins` feature, so check the docs for the state of play before depending on one. Hosted projects live in [`host-plugins/component/`](host-plugins/component/).

_Nothing here yet. [Add the first one](CONTRIBUTING.md)._

## Workload Examples

End-to-end applications demonstrating how components compose into a running system. Hosted projects live in [`workload-examples/`](workload-examples/).

For starting points maintained by Cosmonic rather than the community, see the [Template Catalog](https://cosmonic.com/docs/template-catalog/).

- [first-flight](workload-examples/first-flight/) (hosted): The single `wasi:http` component behind the **First Flight** entry in the Cosmonic Desktop Launchpad, serving one self-contained page that reports the workload name, the host it answered on, and a live round-trip counter. The smallest thing that proves a workload is running rather than a static file. Rust, exports `wasi:http/incoming-handler@0.2.2`.
- [task-manager](workload-examples/task-manager/) (hosted): A stateful to-do list served over HTTP that persists to the host key-value store, so the list survives a restart with no database to run and no outbound network. A compact example of declaring a non-ambient import (`wasi:keyvalue/store`) alongside an HTTP trigger. Rust, exports `wasi:http/incoming-handler@0.2.2`.
- [control-demos](https://github.com/cosmonic-labs/control-demos) (linked): Reference components and demos for Cosmonic Control on Kubernetes, spanning Rust, Go, and TypeScript: a NATS-backed blobstore fileserver, HTTP servers, a Hono and Swagger UI API explorer, and an Argo CD GitOps integration. Ships Helm charts and a `kind` config for running the whole set locally.

## Tools

CLIs, libraries, editor integrations, and developer tooling. Hosted projects live in [`tools/`](tools/).

_Nothing here yet. [Add the first one](CONTRIBUTING.md)._

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first for the two contribution routes, licensing rules, and what a project needs to be accepted.

Projects here are contributed by the community and maintained by their authors. Inclusion is not an endorsement, security review, or statement of production readiness. Read the code before running it.

## Community

- [Cosmonic documentation](https://cosmonic.com/docs/) and the [glossary](https://cosmonic.com/docs/glossary/) if the terminology is new.
- [Community Slack](https://slack.wasmcloud.com) for questions about building and running workloads.
- [cosmonic-labs on GitHub](https://github.com/cosmonic-labs) for the projects behind the platform.

## License

The repository is [Apache-2.0](LICENSE). Projects hosted here carry their own `LICENSE` file in their directory, which governs that project.
