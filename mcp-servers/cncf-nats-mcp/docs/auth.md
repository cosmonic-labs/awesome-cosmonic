# Authorization options for MCP servers on Cosmonic Desktop

The 2026-07-28 MCP specification hardens authorization: OAuth 2.1 with RFC
9207 issuer validation, Client ID Metadata Documents (replacing Dynamic
Client Registration), and an `application_type` for desktop/CLI clients.
`rmcp`'s `auth` feature implements the client side but pulls `reqwest` +
`oauth2`, which don't compile to WASI — and more fundamentally, a sandboxed
per-request component is a poor place to run an OAuth authorization server.

This template ships with `mcp.ai/auth-type: none` and relies on the deployment
boundary. Below are the options for when that isn't enough, in rough order of
recommendation for the Desktop use-case.

## Option A — Platform-terminated auth (gateway pattern)

The Cosmonic ingress (or a dedicated auth middleware component in front of the
handler chain) terminates OAuth: it validates the `Authorization: Bearer`
token, enforces scopes, and forwards the request to the MCP component with
identity context headers (e.g. `x-mcp-subject`, `x-mcp-scopes`).

- **Component's job**: read identity from request extensions/headers (rmcp
  injects HTTP `Parts` into the tool `RequestContext`), enforce per-tool
  authorization. Zero crypto in the guest.
- **Fits Desktop**: local ingress is already the trust boundary; workloads are
  only reachable through it. The host could centralize token validation for
  every MCP workload, with per-workload audience/scope config in the Workload
  manifest (e.g. a `mcp.ai/auth-type: oauth` label driving ingress behavior).
- **wasip3 bonus**: `wasi:http@0.3` has a `middleware` world (imports *and*
  exports `handler`) purpose-built for composing an auth filter in front of
  this component without touching its code.

## Option B — In-component token validation (JWT/JWKS)

The component validates bearer JWTs itself: pure-Rust JOSE crates (`jsonwebtoken`,
`biscuit`) compile to wasm32-wasip2; the JWKS is fetched through
`bridge::outbound` (issuer host on `allowedHosts`) and cached per instance.

- **Pros**: no platform dependency; works on any wasi:http host; end-to-end
  auditable in the template.
- **Cons**: every MCP server re-implements validation; key cache is
  per-instance (cold fetch per instance under scale-out); protected-resource
  metadata (RFC 9728) must also be served by the component.
- **When**: components deployed to hosts where the ingress can't be trusted to
  terminate auth, or multi-tenant scenarios needing defense in depth.

## Option C — Host capability for identity (forward-looking)

A `wasmcloud:identity`-style host interface (or a wasi-proposal successor)
hands the component a *validated* identity for the current request — the host
does OAuth, the component asks "who is calling?". This is the component-model
answer (capability, not code), pairs naturally with per-workload policy in the
Workload manifest, and would let `mcp.ai/auth-type` labels drive host behavior
declaratively. Doesn't exist today; sketching it here as the direction that
matches how Desktop already handles secrets and `allowedHosts`.

## Recommendation

Default the template to **A** (platform-terminated, identity via headers),
with the tool-level authorization hook demonstrated in code once the Desktop
ingress defines its identity-header contract. Offer **B** as an opt-in module
for standalone deployments. Track **C** upstream.
