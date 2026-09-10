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
  - [MCP Servers](#mcp-servers)
- [Host Plugins](#host-plugins)
  - [Native Host Plugins](#native-host-plugins)
  - [Component Host Plugins](#component-host-plugins)
- [Workload Examples](#workload-examples)
- [Tools](#tools)
- [Contributing](#contributing)

## Components

Reusable WebAssembly components that implement a WIT interface. Hosted projects live in [`components/`](components/).

_Nothing here yet. [Add the first one](CONTRIBUTING.md)._

### Kafka

Starting points for Kafka workloads on `cosmonic:kafka@0.3.0`, one per delivery pattern. Each is a self-contained project with source, a `wkg.lock` pinning the WIT it fetches, and manifests for both Cosmonic and Kubernetes. The manifests point at prebuilt components, so a pattern can be deployed and watched before any of it is built locally. Hosted projects live in [`components/kafka/`](components/kafka/).

- [http-kafka-producer](components/kafka/rust/http-kafka-producer/) (hosted): HTTP request in, Kafka record out, through `cosmonic:kafka/producer`. The ingest-gateway shape.
- [kafka-handler-consumer](components/kafka/rust/kafka-handler-consumer/) (hosted): The host owns the consumer and pushes batches into an exported `cosmonic:kafka/handler`, so the component holds no offsets and can be per-request. Least code of the four.
- [kafka-pull-service](components/kafka/rust/kafka-pull-service/) (hosted): A long-running service owning its own consumer and producer, for consume-transform-produce with your own batching and commit policy.
- [kafka-transactional](components/kafka/rust/kafka-transactional/) (hosted): The pull service plus transactions, so output records and input offsets commit atomically — exactly-once read-process-write.

### MCP Servers

[Model Context Protocol](https://modelcontextprotocol.io) servers built as WebAssembly components, so the tools an agent calls run inside a sandbox rather than with ambient host access. Hosted projects live in [`components/mcp-servers/`](components/mcp-servers/).

- [mcp-server-template-ts](https://github.com/cosmonic-labs/mcp-server-template-ts) (linked): Template for building an MCP server as a TypeScript component served over `wasi:http`, scaffolded with `wash new`. The dev loop launches the official MCP inspector, and an `openapi2mcp` script generates tools from an OpenAPI specification.
- [gitlab-mcp](components/mcp-servers/gitlab-mcp/): Searches and reads GitLab (projects, issues, files) over the GitLab REST API v4, bounded by `allowedHosts` to the single host `gitlab.com`. Runs unauthenticated by default; an optional `GITLAB_TOKEN` secret raises the rate limit and reaches private projects. Rust component exporting `wasi:http/handler@0.3.0`.

## Host Plugins

Cosmonic Control schedules workloads onto [wasmCloud](https://github.com/wasmCloud/wasmCloud) hosts. [Host plugins](https://wasmcloud.com/docs/overview/hosts/plugins) extend a host with an implementation of a WIT world, which is linked to workloads at runtime. They come in two flavors, and a workload cannot tell which one is serving a capability it imports.

### Native Host Plugins

Rust implementations of the [`HostPlugin` trait](https://wasmcloud.com/docs/runtime/creating-host-plugins), linked into the host binary. The right choice when a capability needs direct host resources (filesystem, network, hardware) or has to run with the host's privileges. Hosted projects live in [`host-plugins/native/`](host-plugins/native/).

_Nothing here yet. [Add the first one](CONTRIBUTING.md)._

### Component Host Plugins

Capabilities built as [WebAssembly components](https://wasmcloud.com/docs/runtime/creating-component-host-plugins) and deployed into a host at runtime as trigger services with a capability ingress, so you ship, version, and sandbox them like any other component. Currently opt-in via the `host-component-plugins` feature, so check the docs for the state of play before depending on one. Hosted projects live in [`host-plugins/component/`](host-plugins/component/).

_Nothing here yet. [Add the first one](CONTRIBUTING.md)._

## Workload Examples

End-to-end applications demonstrating how components compose into a running system. Hosted projects live in [`workload-examples/`](workload-examples/).

For starting points maintained by Cosmonic rather than the community, see the [Template Catalog](https://cosmonic.com/docs/template-catalog/).

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
