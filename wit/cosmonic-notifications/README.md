# cosmonic:notifications 0.3.0

Portable user notifications supplied by a host or a WebAssembly component.
Native Windows, macOS, and Linux services, web embeddings, and virtual providers
share the same contract. Providers control attribution through trusted wiring;
notifications carry no caller identity or backend name. Action and body targets
(`deep-link`, `url`, `callback`) are preserved so Desktop keeps its click
behavior; a provider without navigation treats them as `callback`.

Import `notifier` and `events` through `notification-consumer` to consume the
service. Export them through `notification-provider` to implement it. Components
can import and export the same interfaces to interpose a filter, router, or
virtual service.

## Contract

There is no session resource. The provider identifies the caller through trusted
wiring and scopes ids, replacement tags, rate budget, and response queues to the
calling workload. Short-lived components in one workload share that scope, so a
response to a notification posted by one invocation is collected by a later one.
Ids, not handles, carry that correlation because handles die with their instance.

`features` reports the exposed service's effective features and limits.
`status` reports availability and permission separately. Both are side-effect-free
snapshots; neither prompts. An embedding arranges permission requests through its
own interaction flow. The provider adapts rather than fails where Desktop did:
extra actions are truncated, inline input becomes a same-id button when actions
are supported and a `max-actions` slot remains (the button counts toward the limit;
otherwise the input is dropped and logged), oversized text is truncated at a
character boundary, and urgency falls back to normal.
`not-supported` is reserved for requests that cannot be adapted.

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
| Resources | None. A resource handle cannot outlive its component instance, and the required use case is a short-lived component whose response is collected by a later one. Numeric ids are workload-scoped correlation tokens, not authority. |
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
0.3.0 contract: replace imports, rename `capabilities` to `features`, collect
queued responses through `events.pull` as before, move every call to P3 async,
and handle `error-code` in place of `notify-error`. Targets, tags, and queue scope
are unchanged. The change does not alter previously published artifacts.

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
  responses in a later invocation from any later invocation of the same workload.
