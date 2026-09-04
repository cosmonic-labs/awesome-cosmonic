# MCP Servers

[Model Context Protocol](https://modelcontextprotocol.io) servers built as
WebAssembly components, so the tools an agent calls run inside a sandbox
rather than with ambient host access.

An MCP server here is an ordinary component that happens to speak MCP over
`wasi:http`. Nothing about it is special to the runtime, so it deploys, scales,
and is versioned like any other workload.

One directory per hosted server. See [CONTRIBUTING.md](../CONTRIBUTING.md)
for requirements, and add a line to the
[root README](../README.md#mcp-servers) either way.
