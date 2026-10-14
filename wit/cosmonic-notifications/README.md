# cosmonic:notifications 0.3.0

Portable user notifications supplied by a host or a WebAssembly component.
Windows, macOS, Linux, web embeddings, and virtual providers share the contract.
Providers control attribution through trusted wiring; notifications carry no
caller identity or backend name.

Import `notifier` and `events` through `notification-consumer` to consume the
service. Export them through `notification-provider` to implement it. A component
can declare a private world importing and exporting these interfaces to filter
or forward requests. No resource handles or wrapper world are required.

## Caller isolation

Trusted wiring binds `notifier` and `events` to the same stable caller scope.
Ids, replacement tags, rate budget, and queued results belong to that scope
across consumer invocations. Ids provide correlation because resource handles
cannot outlive the sending instance.

A host provider can derive scope from trusted workload metadata. A terminal
component provider instance serves exactly one scope: the assembler instantiates
separate providers for separate scopes and keeps their state alive across
consumer invocations, or supplies separate persistent stores. The provider never
infers caller identity from notification data. Connecting unrelated scopes to
one terminal instance violates isolation; this API provides no caller identity
with which that instance could separate them. A forwarding component preserves
isolation by using a dedicated downstream binding for its scope.

Queued responses do not invoke a consumer. The embedding supplies an external
trigger, such as a scheduled invocation or application event, to drain them.
A future handler interface could add provider-driven invocation without changing
this pull contract; it is not part of this release.

## Support and adaptation

`get-features` returns the composed service's effective support: availability,
actions, inline reply, urgency, dismissal, navigation, and `max-actions` (at most
8, and zero without actions). Independent booleans allow combinations such as
inline reply without buttons. `actions` describes support and `max-actions`
bounds the current request; providers keep the two consistent.

Discovery never displays UI. An unavailable service still returns
`available: false`; an unreachable provider returns an error. Permission has no
query. First send may request platform authorization; declined authorization
maps to `unavailable`, and caller revocation maps to `access-denied`. Availability
does not encode why a service is unavailable or prescribe a retry policy.

Providers truncate extra actions and display text at character boundaries.
Without inline reply, input becomes a same-id callback button if buttons are
supported and a `max-actions` slot remains after the listed actions. Otherwise
it is dropped and logged. Unsupported urgency uses normal. The three urgency
levels follow the [freedesktop notification specification](https://specifications.freedesktop.org/notification/latest/urgency-levels.html);
urgency does not override this API's response deadline.

Action ids and input ids must be printable and nonblank, and unique across both.
Providers accept at least 64 UTF-8 bytes, reject invalid or oversized ids with
`invalid-argument`, and echo accepted ids exactly. Tags are trimmed before
validation, never truncated, and have the same 64-byte acceptance floor. `none`
disables replacement; a blank tag is invalid. `not-supported` is reserved for
requests that cannot be adapted.

Navigation is optional and governed by embedding policy. `deep-link` accepts
application routes, and `url` accepts HTTP or HTTPS. Without navigation support,
both behave as `callback`. Supporting navigation does not grant unrestricted
network access or bypass the embedding's route and URL policy.

`timeout: option<duration>` uses `wasi:clocks/types@0.3.0`, in nanoseconds.
`none` uses the provider's default; `some(0)` expires immediately. Larger values
are clamped to the provider's maximum. Display lifetime remains best effort.
Using a shared type requires the clocks package, but grants no clock operations.

## Delivery

`send` returns an id and `future<result<response, error-code>>`. Acceptance does
not guarantee visibility or successful delivery. A later delivery failure
resolves the result's error arm, rather than masquerading as expiration.

If a future reader is attached when the notification settles, its result goes
to the future; otherwise it goes to the scope's queue. The queued `event` carries
the same id and result, including delivery failures. Dropping the future never
withdraws the notification. Delivery is best effort: a reader failing mid-await
can lose its result, and abrupt provider loss can prevent any result arriving.

Activation, action, reply, observed dismissal, expiry, closure, or another
terminal outcome settles a notification once while the provider remains live.
A missed deadline or unobservable dismissal produces `expired`. `close`, tag
replacement, and orderly shutdown produce `closed`. `other(option<string>)`
covers additional terminal outcomes without adding a variant case. Consumers
must handle it without assuming activation or automatically resending; detail
strings are diagnostic, not a protocol.

`subscribe(follow)` returns a `subscription` with an event stream and a
`future<result<_, error-code>>` completion status. Read events to the end, then
await completion. Stream closure alone does not establish success. A finite
backlog completes with `ok`; loss of availability, revoked access, or delivery
failure closes the stream and completes with an error. A live subscription runs
until cancellation or failure. Dropping the event reader cancels the
subscription and completes with `ok`; dropping completion does not cancel the
stream. If the provider disappears before completion, success cannot be inferred.

Streams compete for queued events. Remove an event only when its reader takes
it; unread events stay queued when a reader drops. With `follow: false`, only
events queued when the subscription opened are eligible, and competing readers
may take them first. With `follow: true`, newly queued events are eligible too.
The queue is bounded and overflow drops its oldest events; successful completion
does not promise durable or lossless history.

Unknown or completed ids are a no-op for `close`. While unavailable, `send` and
`subscribe` return `unavailable`, while `close` succeeds. Caller access policy
still applies. `limit-exceeded` covers rate budgets and resource limits. `failed`
reports a failed delivery attempt or subscription; `other` covers unclassified
failures. Error strings must not be parsed, and errors do not prove that nothing
was displayed.

## Usage

- **Wait for an answer:** send and await `sent.response`; handle either the
  outcome or delivery error.
- **Answer later:** send, keep `sent.id`, and drop the future. A later invocation
  drains `subscribe(false).events`, then checks completion. Pending ids are not
  in the backlog yet.
- **Follow continuously:** drain `subscribe(true).events` and check completion
  when it closes.
- **Withdraw:** close an id; its result is `ok(closed)`.
- **Adapt:** query `get-features` before choosing interactions.

## WASI architecture review

| Pattern | Decision |
| --- | --- |
| P3 async | Every operation uses native `async func`; futures and streams suspend through the P3 ABI. |
| Resources | None. Scope-local ids correlate notifications across component instances. |
| Naming | Kebab-case names, noun records, verb-first operations, and established WASI error spellings. |
| Version metadata | Interfaces, types, operations, worlds, and world bindings carry `@since(version = 0.3.0)`. |
| Duration | Reuse WASI clocks duration; optional absence has one meaning, separate from zero. |
| Errors | Operation results, response futures, queued events, and subscription completion carry typed failures. |
| Variants | `response.other` and `error-code.other` admit unclassified meanings. WIT has no non-exhaustive variants; adding a discriminant still requires a breaking release. Urgency stays the three standard presentation hints. |
| Streams | Native streams supply backpressure; a completion future distinguishes normal completion from a reported failure. |
| Scope | A terminal provider instance serves one scope; the assembler supplies isolation and persistence. |
| Triggers | Short-lived consumers need an external invocation trigger. A handler can be added separately. |

The [Component Model WIT specification](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md)
and [native concurrency model](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Concurrency.md)
define the ABI. [WASI 0.3 HTTP](https://github.com/WebAssembly/WASI/blob/v0.3.0/proposals/http/wit/types.wit)
uses a completion future alongside a body stream, and
[WASI clocks](https://github.com/WebAssembly/WASI/blob/v0.3.0/proposals/clocks/wit/types.wit)
defines duration. These patterns support component providers without polling resources.

## Publishing

The build script fetches the digest-pinned clocks dependency and verifies that
fetching preserves the committed lockfile:

```sh
node .github/scripts/wit/build.mjs wit/cosmonic-notifications cosmonic:notifications
```

The artifact is `dist/notifications.wasm`, published at
`ghcr.io/cosmonic-labs/cosmonic/notifications:0.3.0` through
[`../wkg-registries.toml`](../wkg-registries.toml).

## User stories for the deferred example

- As a reader, I build a consumer and component provider from published WIT,
  with concise instructions for ownership, authorization, and typed errors.
- As an assembler, I replace a native service with a virtual provider, then
  interpose a forwarding or filtering component without rebuilding the consumer.
- As an implementer, I preserve actions and replies across Windows, macOS,
  Linux, and web environments and report effective support.
- As a test author, I verify separate-scope isolation, replacement, close,
  expiry, unknown outcomes, overflow, denial, and delivery failures without UI.
- As a consumer, I handle errors from both the response future and event results,
  and distinguish a drained backlog from a failed subscription.
- As an embedding author, I retain each provider's scope state and invoke
  short-lived consumers to collect queued results.

The application example is deferred.
