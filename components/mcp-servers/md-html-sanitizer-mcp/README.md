# md-html-sanitizer-mcp

An MCP server that **turns untrusted markdown or HTML into safe HTML**, built as
a WebAssembly component for Cosmonic Desktop.

It is **pure-compute and zero-egress**: both tools do all their work on-device
and the component makes no outbound calls anywhere in its source, so content
handed to it cannot be exfiltrated.

Sanitizing is delegated to [ammonia](https://docs.rs/ammonia), which parses the
markup with html5ever and re-serializes from the parsed tree against an
allowlist. That is the part worth trusting: hand-rolled sanitizers that strip
tags with string matching lose to markup a browser parses differently than they
do.

No authentication and no configuration are required.

Built with [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`.

## Tools

| Tool | Purpose |
|---|---|
| `sanitize_html` | Run untrusted HTML through an allowlist-based cleaner and return the safe subset. |
| `render_markdown` | Render CommonMark to HTML, then sanitize the result so any embedded raw HTML is neutralized. |

### `sanitize_html`

| Param | Type | Purpose |
|---|---|---|
| `html` | string | The untrusted HTML to sanitize. Capped at 256 KiB; larger input is rejected. |

Returns:

```json
{ "sanitized": "<p>Hi</p><a rel=\"noopener noreferrer\">x</a>", "removed": true }
```

`removed` is `true` when the output differs from the input, i.e. something was
stripped or the markup was rewritten.

### `render_markdown`

| Param | Type | Purpose |
|---|---|---|
| `markdown` | string | The untrusted CommonMark to render to safe HTML. Capped at 256 KiB; larger input is rejected. |

Returns:

```json
{ "html": "<h1>Hi</h1>\n<p>normal <strong>bold</strong> and a raw  tag</p>\n" }
```

## How it sanitizes

HTML sanitization is done with [`ammonia`](https://docs.rs/ammonia), the standard
allowlist-based sanitizer (an html5ever tokenizer plus a safe tag/attribute
allowlist). It strips `<script>`, `<style>`, `<iframe>`/`<object>`/`<embed>`,
every event-handler attribute (`onclick`, `onerror`, …), and dangerous URL
schemes (`javascript:`, and `data:` on `href`/`src`), while keeping a safe subset
of tags and attributes. Hand-rolled HTML sanitizers leak XSS, so none is used
here.

`render_markdown` renders with [`pulldown-cmark`](https://docs.rs/pulldown-cmark)
(a pure-Rust CommonMark renderer) and then passes the rendered HTML **back
through ammonia**. CommonMark permits raw inline HTML, so this second pass is
what neutralizes an embedded `<script>` in the markdown. It is the safe path.

It renders **CommonMark, not GitHub Flavored Markdown**. The parser runs with
default options, so GFM-only syntax is not recognized and comes through as
literal text: a pipe table renders as a paragraph of pipes rather than a
`<table>`, and the same goes for strikethrough, task lists, and footnotes.

One note on the `removed` flag: it reports that the output differs from the
input, which includes harmless normalization. Feeding it `<p>hi` returns
`<p>hi</p>` with `removed` set to `true`. Treat it as "this markup was
rewritten", not as "this markup was hostile".

## `allowedHosts`: zero egress by design

Unlike an MCP server that reaches an upstream API, this tool reaches **nothing**.
Its work is pure compute, so the workload's outbound allowlist is empty:

```yaml
allowedHosts: []
```

Egress is deny-by-default, so an empty list and an omitted key both mean the
component reaches nothing. The stronger guarantee is in the code rather than the
manifest: this component never constructs an outbound request, so there is no
host to add here and none should be added.

## Build

```console
$ wash build
```

Tested with `wash` 2.5 and the Cosmonic Desktop daemon 0.5.x. The build emits a
WASI p3 component at `target/wasm32-wasip2/release/md_html_sanitizer_mcp.wasm`.

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
    -H 'Host: md-html-sanitizer-mcp.localhost.cosmonic.sh' \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H 'Mcp-Method: tools/call' -H 'Mcp-Name: sanitize_html' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"sanitize_html","arguments":{"html":"<p onclick=alert(1)>Hi</p><script>steal()</script>"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
```

See [`scripts/smoke.sh`](scripts/smoke.sh) for a full `tools/list` +
`sanitize_html` + `render_markdown` walk-through.

## Configuration

| Env var | Purpose |
|---|---|
| `RUST_LOG` | Log level (default `info`). |
| `MCP_ALLOWED_HOSTS` | DNS-rebinding guard for the ingress Host header; must list the workload's `host` (`md-html-sanitizer-mcp.localhost.cosmonic.sh`, `md-html-sanitizer-mcp.localhost`). |
