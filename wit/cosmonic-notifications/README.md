# cosmonic:notifications 0.3.0

Portable user notifications supplied by a host or a WebAssembly component.
Native Windows, macOS, and Linux services, web embeddings, and virtual providers
share the same contract. Providers control attribution through trusted wiring;
notifications carry no caller identity or backend name. Action and body targets
(`deep-link`, `url`, `callback`) let a provider with navigation handle clicks; a
provider without it treats them as `callback`.

Import `notifier` and `events` through `notification-consumer` to consume the
service. Export them through `notification-provider` to implement it. A component
can also declare its own world that imports and exports these interfaces to act
as a filter, router, or virtual service. The assembler wires the scope for its
downstream imports and for the callers it serves.

## Contract

Calls carry no caller identity, so trusted wiring must bind `notifier` and
`events` to the same stable caller scope and preserve it across component
instances. A host-native provider can derive the scope from its own workload
metadata; a component provider receives it from its wiring. The provider scopes
ids, replacement tags, rate budget, and queued responses to that scope, so a
response to a notification sent by one invocation is collected by a later one
wired to it. The embedding defines what a scope is; Desktop maps it to a
workload. Ids, not handles, carry the correlation because handles die with their
instance.

`features` reports the exposed service's effective capabilities: whether it is
`available`, and whether it supports actions, inline reply, urgency, dismissal,
and navigation, plus `max-actions` (at most 8, and 0 without actions). It is a
side-effect-free snapshot and never displays UI. An unavailable service still
returns it; only an unreachable remote provider returns an error. Permission has
no query. A provider that needs platform
authorization requests it on the first `send`; a user who declines gets
`unavailable`, and a per-caller revocation gets `access-denied`.

The provider adapts rather than fails: extra actions are truncated, inline input
becomes a same-id `callback` button when actions are supported and a `max-actions`
slot remains (the button counts toward the limit; otherwise the input is dropped
and logged; the click arrives as `action(id)` with no text), oversized display text is truncated at a character boundary, and urgency
falls back to normal. Identifiers are never adapted. Providers accept at least
64 UTF-8 bytes for action ids, input ids, and tags, return `invalid-argument` for
any they will not accept, and keep accepted values exactly. Only tags are trimmed,
before the length check. Ids contain at least one non-whitespace character and no
control characters, and an id that collides with another action or the input also
returns `invalid-argument`. `not-supported` is reserved for requests that cannot be
adapted.

### Delivery

`send` accepts a notification and returns a `sent` record: its `id` and a
`response` future. One delivery mechanism has two ways to consume the answer.

- **The future.** A caller that stays alive awaits `response`.
- **The stream.** If nobody holds the future when the notification settles, the
  response goes to the scope's queue, and `events.subscribe` streams it as an
  `event` with the same `id`.

A response goes to the future when its reader is still attached as the
notification settles, and otherwise to the queue. A caller that drops the future
or abandons its wait therefore still gets the answer from the stream. A reader
that fails mid-await can lose the response; delivery is best effort. Dropping the
future never withdraws the notification; `close` does.
Activation, action, reply, dismissal, expiration, or closure settles an accepted
notification once while the provider remains live. A missed deadline produces
`expired`, and so does a dismissal when `features.dismissal` is false, so a caller
may resend. `close`, replacement by tag, and orderly shutdown produce `closed`,
which a caller should not resend on its own. Platforms that must opt in to observe dismissal,
such as macOS notification categories, enable it for every notification they
display and report `dismissal: true`. The queue is bounded and overflow drops the
oldest events. A stream removes an event only when its reader takes it, so events
unread at drop stay queued. Open streams compete for events. With `follow` false,
`subscribe` ends the stream after the backlog queued when it opened; with `follow`
true it then continues with live events. Abrupt provider loss may lose responses;
delivery is not durable.

The response deadline uses the provider's default for none or zero and clamps to
its maximum, both provider-defined. Native display lifetime remains best effort.
Closing an unknown or completed id is a no-op. While the service is unavailable,
`send` and `subscribe` return `unavailable` and `close` succeeds. `limit-exceeded`
covers the send budget, pending notifications, and open streams, and is retryable
after backoff. Failure strings are diagnostic and must not be parsed.

## Usage

- **Wait for an answer:** call `send` and await `sent.response`. It resolves with
  the user's response, or `expired` at the deadline.
- **Answer later:** call `send`, keep `sent.id`, and drop the future. A later
  invocation calls `events.subscribe(false)`, reads the stream to its end, and
  matches `event.id` to the ids it kept. Ids still pending are not in it yet.
- **Follow continuously:** a long-lived component calls `events.subscribe(true)`
  and reads until it drops the stream.
- **Withdraw:** call `close` with an id. Its response resolves as `closed`.
- **Adapt to the service:** call `features` first when the caller needs to
  choose between actions, inline input, and plain text.

## WASI architecture review

| Pattern | Decision |
| --- | --- |
| P3 async | Every operation declares `async func`, including discovery for remote or composed services. Rust `async fn` alone would not change a synchronous WIT ABI. Awaiting a future or reading a stream suspends through native async calls. |
| Resources | None. A resource handle cannot outlive its component instance, and the required use case is a short-lived component whose response is collected by a later one. Numeric ids are scope-local correlation tokens, not authority. |
| Naming | Lowercase kebab-case WIT names; noun records; verb operations. `supported-features` describes effective service behavior. `error-code`, `access-denied`, `invalid-argument`, and `not-supported` follow familiar WASI spellings. |
| Errors | `error-code` is a variant with `other(option<string>)`, following WASI sockets and HTTP. Use it for unclassified failures; optional detail is diagnostic. |
| Enums and variants | WIT has no Rust-style `non_exhaustive` annotation. `other` permits new failure meanings within an existing case; it does not make new discriminants compatible. `urgency` and `response` stay closed because their meanings are finite. Adding a case changes the WIT type and requires a breaking contract release (for example, 0.4.0 before 1.0). |
| Futures | `sent.response` is a native `future<response>`: one value for one notification. It lets a live caller await its answer without polling or a pollable resource. |
| Streams | `events.subscribe` returns a native `stream<event>`, so a long-lived component follows responses as they settle (`follow` true) and a short-lived one reads the settled backlog to its end (`follow` false). Backpressure comes from the stream: the provider writes only to a ready reader and re-queues what a dropped reader did not take. |

These choices use the [Component Model WIT specification](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md)
and [native concurrency model](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Concurrency.md).
The error patterns are illustrated in [WASI 0.3 sockets](https://github.com/WebAssembly/WASI/blob/v0.3.0/proposals/sockets/wit/types.wit)
and [HTTP](https://github.com/WebAssembly/WASI/blob/v0.3.0/proposals/http/wit/types.wit).
WASI also permits nonblocking synchronous getters; this package consistently uses
async operations so component providers can query downstream services.

## Publishing

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
  with concise instructions for ownership, authorization, and typed errors.
- As an assembler, I replace a native service with a virtual provider without
  rebuilding the consumer, then interpose a forwarding or filtering component.
- As an implementer, I preserve actions and replies and report effective support
  across Windows, macOS, Linux, and web environments.
- As a test author, I verify isolation, replacement, close, expiry, overflow,
  denial, unavailable service, and unsupported interactions without native UI.
- As a consumer, I await bounded operations through the P3 ABI and collect queued
  responses from any later invocation wired to the same caller scope.
