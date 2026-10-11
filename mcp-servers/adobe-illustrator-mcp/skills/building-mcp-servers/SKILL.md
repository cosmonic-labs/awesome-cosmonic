---
name: building-mcp-servers
description: Build, test, and deploy an MCP server as a WebAssembly component on Cosmonic Desktop using the mcp-server-template-rs framework (rmcp 3.x, wasi:http@0.3.0, MCP spec 2026-07-28). Use when creating a new MCP server, adding tools to one, or deploying/debugging one on Cosmonic Desktop.
---

# Building MCP servers with mcp-server-template-rs

Version: 6 (updated after: deploy/ manifests for Desktop; Desktop-only deploy docs)

This skill turns a tool idea into a deployed, spec-compliant MCP server
component. Follow the phases in order; the Pitfalls section at the end is a
living list — read it BEFORE writing code, not after something breaks.

## Reference implementations

Four worked examples live in
[cosmonic-labs/mcp-examples](https://github.com/cosmonic-labs/mcp-examples).
Copy the one whose shape matches your target:

| Your server is… | Start from | It demonstrates |
|---|---|---|
| Pure compute (math, codegen, conversion) | `after-effects-mcp` | enum params, numeric-trap discipline, structured output |
| Pure compute with tricky domain math | `premiere-mcp` | reference-vector testing, domain validation, checked/saturating arithmetic |
| Outbound API, no key | `sec-edgar-mcp` | `bridge::outbound::fetch`, required upstream headers, instance caching, `allowedHosts`, base-URL test override |
| Outbound API with a key | `fred-mcp` | env/secret API keys, query-param auth, param clamping, missing-key UX |

They all source the shared test harness `scripts/mcp_e2e_lib.sh`.

## Non-negotiable guardrails

1. **Spec**: MCP 2026-07-28 (stateless core, `Mcp-Method`/`Mcp-Name` header
   routing, per-request `_meta`, `resultType: complete`, structured output).
   The template's transport config already enforces this — do not change
   `legacy_session_mode`, the session manager, or protocol pinning.
2. **Target**: the component must export `wasi:http/handler@0.3.0` (WASI p3),
   built on `wasm32-wasip2` with the template's `.cargo/config.toml` intact.
3. **Cosmonic Desktop compatibility**: deploys as a Workload with the
   `interfaces: ["handler"]` hostInterface (p3 name — NOT `incoming-handler`),
   `mcp.ai/*` labels, `MCP_ALLOWED_HOSTS` matching the ingress host, and
   fail-closed `allowedHosts`.
4. **Never** await a WASI p3 future from tool code. Outbound HTTP goes
   through `crate::bridge::outbound::fetch` only.
5. Every server ships with a passing `scripts/e2e.sh` adapted to its tools.

## Phase 1 — scaffold

Copy the template (do not `git clone` into the new project):

```bash
SRC=<path-to-mcp-server-template-rs>
mkdir -p <project> && cd <project>
cp -R $SRC/.cargo $SRC/.wash $SRC/src $SRC/scripts $SRC/deploy $SRC/Cargo.toml \
      $SRC/Cargo.lock $SRC/.gitignore .
```

Then rename everywhere, keeping the three names aligned:
- `Cargo.toml` `[package] name` (kebab-case, e.g. `sec-edgar-mcp`)
- `.wash/config.yaml` `build.component_path` → `target/wasm32-wasip2/release/<name_with_underscores>.wasm`
- `scripts/e2e.sh` `WASM=` path
- `workload.yaml` AND `deploy/workload.yaml`: `metadata.name`, hostInterface
  `config.host` (`<name>.localhost` / `<name>.localhost`),
  labels (`app.kubernetes.io/name`, `mcp.ai/domain`), the `image` ref, and
  env `MCP_ALLOWED_HOSTS` = the matching host(s).

`mcp.ai` label conventions (catalogued by Cosmonic Desktop):
`auth-type: none|oauth`, `domain: <subject-area>`, `function-type: tools`,
`spec-version: "2026-07-28"`, `statefulness: stateless`,
`transport: streamable-http`; plus the annotation
`desktop.cosmonic.com/source: mcp`.

## Phase 2 — implement tools (src/server.rs)

Replace the example tools. Patterns:

- Params: a `#[derive(Debug, Deserialize, JsonSchema)]` struct per tool with
  doc comments on every field (they become the client-visible schema).
- Simple result: `CallToolResult::success(vec![ContentBlock::text(..)])`.
- Machine-readable result: `CallToolResult::structured(serde_json::json!({..}))`
  — prefer this whenever the output has shape; it fills both
  `structuredContent` and a text fallback.
- **Error discipline**: `Err(ErrorData::invalid_params(..))` ONLY for
  requests the server cannot route/parse; `Ok(CallToolResult::error(..))`
  for "the tool ran and failed" — the caller sees your message.
- Add `#[tracing::instrument(name = "tool.<name>", skip(self))]` on every
  tool.
- Update `get_info()` instructions to describe the server's purpose and
  tools.
- No per-session state on the server struct: instances are ephemeral.
  Durable state → host capability or upstream API.

Outbound HTTP from a tool:

```rust
let request = http::Request::get(url)
    .header("User-Agent", ua)                 // set REQUIRED upstream headers
    .body(bytes::Bytes::new())
    .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
let response = crate::bridge::outbound::fetch(request).await; // deadline + size cap
```

- **CRITICAL — enable `inter-task-wakeup`.** Any server that does outbound
  HTTP MUST add this to `Cargo.toml` (the template already does):
  ```toml
  wit-bindgen = { version = "0.57.1", default-features = false,
      features = ["async", "async-spawn", "inter-task-wakeup"] }
  ```
  Without it, concurrent invocations on one reused instance **trap** ("Rust
  task cannot sleep waiting only on Rust-originating events") the moment a task
  parks on a Rust-only event (the bridge's request lock / outbound oneshot).
  Pure-compute servers never hit this; outbound servers hit it under any real
  concurrency. `wasip3` does not forward the feature, so declare `wit-bindgen`
  directly — Cargo unifies it into wasip3's generated code.
- Always set upstream-required headers (e.g. SEC EDGAR needs a descriptive
  `User-Agent` or it returns 403).
- Map upstream status codes deliberately: distinguish "not found" (a normal
  tool result — `CallToolResult::error`) from an infrastructure failure, and
  surface actionable messages (403 → "check the User-Agent / API key").
- Keep the `deny_private_target` guard for any tool taking a client-supplied
  URL. Tools calling a FIXED upstream host don't need it (allowlist decides).
- Every upstream host must be in `deploy/workload.yaml` `allowedHosts` AND the
  `.wash/config.yaml` workload section. Empty = deny-all. Raise
  `MCP_OUTBOUND_MAX_BYTES` if the API returns bodies over 4 MiB.
- **Instance-level caching is fine and useful.** Requests are serialized by
  the transport and share the instance's statics, so a
  `OnceLock<tokio::sync::Mutex<Option<Arc<T>>>>` lazily populated on first use
  (e.g. SEC's ticker→CIK table) caches across requests for that instance's
  lifetime — exactly like the reference. Not shared across instances; that's
  correct for a stateless-scaling model.
- Secrets (API keys): read from env (`std::env::var`); in the Workload use
  `secretFrom` + `cosmonic_set_secret` for production, plain env config only
  for examples — and say so in the README. **Never compile a key into the
  component**; never commit a real key to `deploy/workload.yaml`.
- When a key is **missing**, return a distinct, actionable tool error ("X is
  not set. Get one at <url> and set it via …"), not a generic failure — and
  e2e-test that path with a second server instance started without the key.
- Query-parameter auth (FRED) vs header auth (most APIs): put the key wherever
  the upstream wants it, and **percent-encode** query values yourself (the
  `http` crate won't). The key value itself usually shouldn't be encoded if
  it's already URL-safe, but encode user-supplied query text.
- **Make upstream base URLs overridable via env** (default to the real host)
  so the e2e can point at a local fixture — hermetic, network-free CI. This is
  the single highest-leverage testability move for an outbound server.
- **Clamp numeric params to the upstream's documented range** (FRED `limit`
  1..100000) rather than forwarding raw client values that the API would 400.

## Phase 3 — compile gates

```bash
cargo fmt && cargo clippy --all-features -- -D warnings && cargo build --release
wasm-tools component wit target/wasm32-wasip2/release/<name>.wasm | grep 'export wasi:http/handler@0.3.0'
```

`cargo test` does NOT work here (build target is wasm) — `scripts/e2e.sh` is
the test entry point.

## Phase 4 — adapt and run the e2e suite

Edit `scripts/e2e.sh`:
- Update `WASM=` and pick unique ports (avoid 8199/8198/8197 if another
  server's suite may run concurrently).
- Replace the example-tool cases (echo/add/current_time/http_get) with cases
  for YOUR tools: happy path, malformed params, and at least one
  domain-specific adversarial case per tool (boundary values, huge inputs,
  unicode, injection-shaped strings).
- Keep the protocol/spec-enforcement/robustness/guard sections — they test
  the framework and must keep passing.
- For tools calling a fixed upstream: add a local fixture server (see the
  python fixture in the template's e2e) and, where the real API is keyless
  and cheap, one live smoke case. Wasmtime's `-Shttp` has no outbound
  allow-list, so fixtures work without policy changes.
- **Fixture servers MUST be threaded** (`ThreadingHTTPServer`, not
  `HTTPServer`) — the concurrency test fires 8 requests at once, and a
  single-threaded fixture refuses the extras, producing spurious "fetch
  failed" results that look like a server bug but are the test harness's fault.
- The shared harness's concurrency test uses `FIRST_TOOL_*`; point it at an
  outbound tool so it actually exercises concurrent outbound (the case that
  catches the `inter-task-wakeup` trap).

Run: `scripts/e2e.sh` — all cases green before proceeding.

## Phase 5 — deploy to Cosmonic Desktop and verify

The daemon socket is `~/Library/Application Support/Cosmonic/cosmonicd.sock`.

**Deploy hygiene — do NOT clobber existing workloads (learned the hard way).**
`cosmonic_apply_workload` is idempotent BY `namespace/name`: applying a name
that already exists *replaces* its spec, silently discarding a running
deployment. Before applying:
1. `cosmonic_list_workloads` and check for your intended `namespace/name`. The
   ingress `host` is ALSO global — two workloads cannot share one `config.host`
   even across namespaces.
2. Deploy examples into a **dedicated namespace** (e.g. `mcp-examples`) and use
   a **distinct host suffix** (e.g. `<name>.example.localhost`) so you never
   collide with reference/demo deployments — the reference demos here use
   `<name>.localhost` and plain `<name>.localhost`.
3. If you must replace a workload, `cosmonic_inspect` its current image first
   and save the digest-pinned ref so you can restore it.
The shipped `deploy/workload.yaml` should still use the clean `<name>.localhost` host
(correct for a fresh machine); only your local *verification* deploy needs the
collision-avoiding host.

Use the MCP tools: `cosmonic_scaffold_project` (or register an existing
directory) then `cosmonic_promote` with `{"project_id": "<id>", "reference":
"<name>:<version>", "confirm": true}`. A reference that names no registry is
pushed to Cosmonic Desktop's built-in local registry (as `apps/<name>`, over
plain HTTP — no `insecure` flag, and no Docker Hub); promote returns the
digest-pinned image ref, which is what the workload must use.

The same thing over the socket API, when you need it outside an MCP client:

```bash
SOCK="$HOME/Library/Application Support/Cosmonic/cosmonicd.sock"
# 1. Register the project directory (needs .wash/config.yaml):
curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
  -H 'Content-Type: application/json' -d '{"path":"<absolute project path>"}'
# 2. Promote (push; builds only when there is no artifact yet). The REST field
#    is `ref` (the MCP tool's `reference` argument maps to it):
curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/<id>/promote \
  -H 'Content-Type: application/json' -d '{"ref":"<name>:<version>"}'
# returns a digest-pinned image ref — use THAT in the workload.
```

Apply the workload with the `cosmonic_apply_workload` MCP tool (or POST
/v1/workloads), with the digest-pinned image. Then verify through the
ingress (routes by Host header):

```bash
curl -sS -X POST -H 'Host: <name>.localhost' http://127.0.0.1:8200/ \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"check","version":"0"}}}'
```

Must return `data: {...  "protocolVersion":"2026-07-28" ...}`. Then exercise
one real tool call the same way (add `Mcp-Method: tools/call`,
`Mcp-Name: <tool>`, and `"_meta":{"io.modelcontextprotocol/protocolVersion":
"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}` in params).

Deployment is Cosmonic Desktop only (docs:
<https://cosmonic.com/docs/desktop>). The e2e harness serves the component
under wasmtime purely as test infrastructure (`-Sp3,cli,http`, always
`--addr 127.0.0.1` — the default binds 0.0.0.0); do not document wasmtime as
a deployment target.

Ship ONE manifest: `deploy/workload.yaml`. Local iteration does not need a
second one — `.wash/config.yaml` already carries the labels, environment and
outbound allow-list, and Desktop's project mode (dev -> promote) builds a
digest-pinned Workload draft from them. A root `workload.yaml` duplicating
that is redundant, and duplicated config drifts.

Component-level fields have no home in `.wash/config.yaml`, which mirrors only
`components[].localResources` — so `poolSize`, `maxInvocations` and the image
ref live in `deploy/workload.yaml` and nowhere else. Set `poolSize` above 1 for
any server whose tools block on something that must be served concurrently.

Use `<name>.localhost` as the ingress host, and `ghcr.io/...` (or your
registry) as the image. On Windows use `oci.localhost.cosmonic.sh:8200` for a
locally promoted ref — Windows resolves no `*.localhost` names.
`deploy/workload.yaml` documents each `mcp.ai/*` label, which Cosmonic Desktop
detects (with `desktop.cosmonic.com/source: mcp`) to catalog and manage the
workload as an MCP server. See mcp-examples for filled-in ones.

## Phase 6 — README

Every server gets a README: what it does, tool table (name, params, output),
env vars, `allowedHosts` needed, build/test commands, Cosmonic Desktop
deployment (link <https://cosmonic.com/docs/desktop>), and example curl
calls. Do not document wasmtime as a way to run the server.

## Pitfalls (living list — add what bites you)

- **rmcp 3.x API**: it's `ContentBlock` (not `Content`); `ServerInfo`/
  `Implementation`/`CallToolResult` are `#[non_exhaustive]` — use
  constructors/builders (`ServerInfo::new(..).with_server_info(..)
  .with_instructions(..)`). `#[tool_handler]` defaults to `Self::tool_router()`
  (rebuild per call); pass `#[tool_handler(router = self.tool_router)]` to
  use the field.
- **Never `String::truncate` at a byte offset** on upstream/user content —
  panics mid-UTF-8-char, and panic=abort kills the whole instance. Use the
  template's `truncation_boundary`.
- **Bound every buffer**: request bodies, outbound response bodies, anything
  collected from a stream (`http_body_util::Limited`). The template does
  this for the transport; do it for anything you add.
- **Deadline every await that a peer controls** — the bridge gives outbound
  fetch a 30s deadline; if you add other host I/O, race it with
  `bridge::timeout`.
- **tokio::select in shell scripts**: a bare `wait` in e2e scripts waits on
  the background *servers* too — always `wait "${PIDS[@]}"`.
- **wasmtime flags**: `-Sp3,cli,http` (p3 alone fails: missing wasi:cli;
  without `http` outbound client import fails to link).
- **Host guard vs ingress**: unset `MCP_ALLOWED_HOSTS` = localhost-only →
  Desktop ingress requests (Host `<name>.localhost`) get 403. Set it in the
  workload env. `host` entry matches any port; `host:port` exact; `*`
  disables.
- **wasi-otel feature**: leave OFF for wasmtime and Desktop 0.5.x — the
  rc.2 WIT signatures currently mismatch (`tracing.on-start`), instantiation
  fails. Baseline stderr JSON logging always works.
- **Desktop workload names**: never reuse/overwrite an existing workload
  name — list first (`cosmonic_list_workloads`), pick a fresh name.
- **Registry addressing**: the built-in registry is
  `oci.localhost:8200/...` with `insecure: true` (on Windows
  `oci.localhost.cosmonic.sh:8200/...` — bare `*.localhost` names don't
  resolve there); bare refs go to docker.io and fail. It ships inside
  Cosmonic Desktop as a system workload — never deploy your own registry.
- **Big upstream JSON**: outbound responses are capped at 4 MiB — pick
  API endpoints that return bounded payloads (e.g. prefer per-concept over
  full-dump endpoints) and paginate/limit where the API supports it.
- **Test assertions on errors**: `ErrorData::invalid_params(msg, _)` reaches
  the client as JSON-RPC `{"error":{"code":-32602,"message":msg}}` — assert on
  `-32602` or a substring of YOUR message, never the literal `"invalid_params"`
  (that string never appears on the wire). Tool-level errors
  (`CallToolResult::error`) instead appear as `"isError":true` inside a
  `result`.
- **Reuse the shared harness**: `scripts/mcp_e2e_lib.sh` provides the
  framework tests (protocol, spec enforcement, robustness, Host guard) so each
  server's `e2e.sh` only writes tool cases. Set `FIRST_TOOL_{NAME,ARGS,EXPECT}`
  for the concurrency test before calling `framework_tests`.

### Numeric-trap discipline (pure-compute tools) — from after-effects-mcp

On `wasm32-wasip2` with `panic=abort`, ANY panic aborts the whole instance —
a remotely-triggerable DoS. Release builds do NOT set `overflow-checks`, so
integer overflow *wraps silently* (a correctness bug), but a few operations
panic unconditionally regardless. For every tool doing arithmetic on
client-supplied numbers:
- **Integer `/` and `%` panic on a zero divisor, always.** A validated-positive
  float divisor can still round/cast to `0` (e.g. `fps=0.4` → `round() as u64`
  → `0`). Validate the *derived integer*, not just the input float.
- **`as` casts from float saturate** (NaN→0, huge→MAX since Rust 1.45) — safe
  from traps but can produce absurd values; validate range.
- **Use `checked_mul`/`checked_add`** on unbounded `u64` inputs and return
  `invalid_params` on overflow, rather than letting release wrap silently.
- **Re-check the *output* is finite**: finite inputs can produce `±inf`/`NaN`
  (`1e308 - -1e308`), and `serde_json` serializes those as `null`. Error out
  or clamp instead of emitting a null value.
- **Never interpolate a non-finite float into generated code text** — it
  formats as `NaN`/`inf`, which is invalid in the target language. There's no
  string-injection risk from an `f64` (Display only yields numeric tokens),
  but substitute a default for non-finite values.
- The reference AE server is a *live-app bridge* (wasi:keyvalue queue + a
  polling ExtendScript panel); a Wasm component can't drive a GUI app, so this
  example distills the reference's domain knowledge (easing constants, ADBE
  effect match-names, the three color conventions) into pure-compute tools.
  When a reference targets a live host app, port its *knowledge*, not its
  transport.
- **Unknown enum variant** in a params struct fails deserialization and rmcp
  surfaces it as a **tool-level error** (`isError:true`), NOT JSON-RPC
  `-32602`. A missing/ill-typed *whole* params object is `-32602`. Assert
  accordingly.

### Domain-math correctness (from premiere-mcp)

- **Test against published reference vectors**, not just roundtrips. Drop-frame
  timecode has canonical checkpoints (frame 1800 → `00:01:00;02`, 17982 →
  `00:10:00;00`, 107892 → `01:00:00;00` at 29.97); assert those exact values so
  a subtly-wrong-but-self-consistent implementation can't pass.
- **Reject impossible inputs the domain defines as invalid**, not just
  malformed ones (e.g. drop-frame labels `;00`/`;01` don't exist at non-tenth
  minutes; `drop_frame` is meaningless at 24/25/exact-30). Validate against the
  domain rules, and make the derived-integer guards (`is_multiple_of`,
  fractional-rate checks) do double duty as trap prevention.
- `saturating_*` is the right tool when a pathological-but-in-type input
  (near `u64::MAX`) should degrade to a huge-but-sane answer rather than wrap;
  `checked_*` + error when the input is genuinely out of range. Prefer one of
  the two over bare `+`/`*` on any client-influenced integer.
- clippy on this toolchain wants `x.is_multiple_of(n)` over `x % n == 0`.
