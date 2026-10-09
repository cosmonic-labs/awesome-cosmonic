# cosmonic:notifications 0.3.0

Portable user notifications supplied by a host or a WebAssembly component.
Native Windows, macOS, and Linux services, web embeddings, and virtual providers
share the same contract. Providers control attribution through trusted wiring;
notifications contain no caller identity, backend name, URL, or Desktop route.
Consumers decide what to do with returned interactions.

Import `notifier` to consume the service. Export it through
`notification-provider` to implement it. `notification-wrapper` imports a named
`downstream` interface and exports `notifier`, letting an assembler interpose a
filter, router, or virtual service.

## Contract

`open` returns an owned `session`. Retain it for later calls; passing its handle
explicitly shares the session. Ids, replacement tags, and response queues are
session-scoped. Dropping it withdraws pending notifications and discards queued
responses. Handles cannot be serialized or recovered after provider loss.
A wrapper owns its downstream session and translates ids when needed.

`supported-features` reports the exposed service's effective features and limits.
`status` reports availability and permission separately. Both are side-effect-free
snapshots; neither prompts. An embedding arranges permission requests through its
own interaction flow. Required actions, inline input, and oversized payloads fail
with `not-supported`; urgency is a hint that may fall back to normal.

`post` accepts delivery and returns a correlation id; visibility is not guaranteed.
`pull` removes a bounded batch at most once, polls at zero, and clamps its wait.
`request` waits for one terminal response and never also queues it. Its cancellation
may lose that response. Activation, action, reply, dismissal, or expiration settles
an accepted notification once while the provider remains live. Close, replacement,
deadline, and orderly shutdown produce `expired`. Overflow drops the oldest events;
abrupt provider loss may lose responses. Delivery is not durable.

The response deadline uses the provider's default for none or zero and clamps to
its maximum. Native display lifetime remains best effort. Closing an unknown or
completed id is a no-op. Failure strings are diagnostic and must not be parsed.

## WASI architecture review

| Pattern | Decision |
| --- | --- |
| P3 async | Every operation declares `async func`, including discovery for remote or composed services. Rust `async fn` alone would not change a synchronous WIT ABI. `request` and bounded `pull` suspend through native async calls. |
| Resources | `session` is a noun-named, provider-owned resource with explicit cleanup. Methods borrow it; `open` transfers ownership. Numeric notification ids are session-local correlation tokens, not independent authority. A per-notification resource is unnecessary without an independently owned lifetime. |
| Naming | Lowercase kebab-case WIT names; noun resources and records; verb operations. `supported-features` describes effective service behavior. `error-code`, `access-denied`, `invalid-argument`, and `not-supported` follow familiar WASI spellings. |
| Errors | `error-code` is a variant with `other(option<string>)`, following WASI sockets and HTTP. Use it for unclassified failures; optional detail is diagnostic. |
| Enums and variants | WIT has no Rust-style `non_exhaustive` annotation. `other` permits new failure meanings within an existing case; it does not make new discriminants compatible. `urgency`, `permission`, and `response` stay closed because their meanings are finite. Adding a case changes the WIT type and requires a breaking contract release (for example, 0.4.0 before 1.0). `permission.unknown` means the provider cannot determine permission. |
| Streams and futures | A single async result needs neither an explicit future nor a pollable resource. Bounded queue retrieval supports later invocations. A native event stream would suit a continuous subscription, but would add stream ownership, cancellation, and backpressure rules beyond this contract. |

These choices use the [Component Model WIT specification](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md)
and [native concurrency model](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Concurrency.md).
The error patterns are illustrated in [WASI 0.3 sockets](https://github.com/WebAssembly/WASI/blob/v0.3.0/proposals/sockets/wit/types.wit)
and [HTTP](https://github.com/WebAssembly/WASI/blob/v0.3.0/proposals/http/wit/types.wit).
WASI also permits nonblocking synchronous getters; this package consistently uses
async operations so component providers can query downstream services.

## Migration and publishing

This package replaces `cosmonic:notify@0.2.0` in this catalog. It is a breaking
0.3.0 contract: replace imports, retain a session, remove navigation targets,
handle interactions in the consumer, use session methods for queues, and handle
`error-code`. The change does not alter previously published artifacts.

```sh
wasm-tools component wit wit/cosmonic-notifications
node .github/scripts/wit/build.mjs wit/cosmonic-notifications cosmonic:notifications
```

The release artifact is `dist/notifications.wasm`, published by the WIT workflow
at `ghcr.io/cosmonic-labs/cosmonic/notifications:0.3.0`.
[`../wkg-registries.toml`](../wkg-registries.toml) maps the `cosmonic` namespace
to this registry.

## User stories for the deferred example

The application example is deferred. Its acceptance criteria are:

- As a reader, I build a consumer and component provider from the published WIT,
  with concise instructions for ownership, permission, and typed errors.
- As an assembler, I replace a native service with a virtual provider without
  rebuilding the consumer, then interpose a forwarding or filtering component.
- As an implementer, I preserve actions and replies and report effective support
  across Windows, macOS, Linux, and web environments.
- As a test author, I verify isolation, replacement, close, expiry, drop, overflow,
  denial, unavailable service, and unsupported interactions without native UI.
- As a consumer, I await bounded operations through the P3 ABI and collect queued
  responses in a later invocation while an owner retains the session.
