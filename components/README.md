# Components

Reusable WebAssembly components that implement a WIT interface.

One directory per hosted component. Projects can also be listed as links
without a directory here. See [CONTRIBUTING.md](../CONTRIBUTING.md) for
requirements, and add a line to the [root README](../README.md#components)
either way.

Components serving a well-known protocol are grouped into a subdirectory:

- [`mcp-servers/`](mcp-servers/): Model Context Protocol servers.
- [`kafka/`](kafka/): Kafka workloads on `cosmonic:kafka`, one directory per
  delivery pattern. Start with the handler template for serverless and elastic
  event processing.
- [`nats/`](nats/): NATS workloads on `wasmcloud:nats`, one directory per
  messaging pattern, in Rust and Go. Start with the core subscriber when losing
  a message is survivable, and the JetStream consumer when it is not.

Everything else lives directly in this directory, such as
[`minimal-agent`](minimal-agent/) (an agent harness on `cosmonic:agent`) and
[`sandboxed-webhook`](sandboxed-webhook/) (a webhook receiver with a single
allowed outbound host).

## Using a hosted component

To start your own project from one hosted component, scaffold just that
directory instead of cloning the whole repository:

```console
wash new https://github.com/cosmonic-labs/awesome-cosmonic --subfolder components/<component>
```

This checks out `<component>` into a new local directory (named after it, or
pass `--name` to choose your own), ready to build on as your own project.
