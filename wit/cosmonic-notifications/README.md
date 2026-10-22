# cosmonic:notifications 0.3.0

Portable user notifications. A host or a component provides them, on Windows,
macOS, Linux, the web, or as a virtual provider.

## Worlds

```wit
world notification-consumer {
  import notifier;
  import events;
}

world notification-provider {
  export notifier;
  export events;
}
```

A filter or forwarder declares its own world that imports and exports both.

## Example

A build job asks whether to deploy, then exits.

1. It calls `send` with `deploy` and `cancel` actions, keeps `sent.id`, and drops
   `sent.response`.
2. The user clicks **Deploy**. Nobody holds the future, so the provider queues
   the result.
3. The next scheduled run calls `subscribe(false)`, reads an event with that id
   and `ok(action("deploy"))`, and deploys.

A caller that stays alive awaits `sent.response` instead.

## Caller scope

A caller scope is a group of component invocations that share notification ids,
replacement tags, a rate budget, and queued results. For example, a host can give
each workload its own scope so a later invocation can collect an earlier one's
results. Calls carry no identity and cannot select another scope.

The host or application connects `notifier` and `events` to the same scope. A
component that delivers notifications serves one scope per instance; the host or
application retains that instance or persists its state across invocations.

Queued results don't invoke the consumer. The embedding supplies the trigger,
such as a scheduled run.

## Delivery

```wit
send: async func(notification: notification) -> result<sent, error-code>;

record sent {
  id: notification-id,
  response: future<result<response, error-code>>,
}
```

Acceptance doesn't guarantee the notification was shown. When it settles, the
provider starts writing the result to `sent.response`. The write completes when
the reader receives it; a held but unread future keeps the write pending. If the
reader is dropped before that transfer completes, the same result is queued,
even if the notification has already settled. Dropping the future never withdraws
the notification; `close` does. Delivery is best effort, not durable.

| Response | Meaning |
| --- | --- |
| `activated`, `action`, `reply` | The user answered. |
| `dismissed` | The user dismissed it (requires `dismissal` support). |
| `expired` | The deadline passed, or a dismissal wasn't observable. Don't resend automatically. |
| `closed` | `close`, tag replacement, or orderly shutdown. No user decision; resending is fine. |
| `other` | Anything else. Don't assume an answer or resend. |

`timeout-ns` is a `wasi:clocks` `duration`, in nanoseconds: `none` uses the provider
default, zero expires immediately, and a finite maximum bounds every wait.

## Events

```wit
subscribe: async func(follow: bool) -> result<subscription, error-code>;

record subscription {
  events: stream<event>,
  completion: future<result<_, error-code>>,
}
```

Read `events` to the end, then await `completion`; a closed stream alone doesn't
mean success. `follow: false` reads the backlog queued at open, and `true` also
reads live results. Readers compete, so each event is taken at most once. Unread events
stay queued, and overflow drops the oldest.

## Adaptation

`get-features` returns an extensible feature list:

```wit
get-features: async func() -> result<list<supported-feature>, error-code>;
```

For example, `[actions(2), navigation]` allows two buttons and navigation, but no
inline reply, urgency hints, or dismissal reports. Each standard feature appears
at most once, in any order. `other("example:badge")` advertises a provider extension;
callers ignore names they do not understand. An empty list means no optional
features. An unavailable service returns `unavailable` instead. Availability and
support can change, so `send` may still fail after discovery succeeds.

Providers truncate actions to the `actions` limit (zero when absent), truncate
long text, fall back to normal urgency, and treat navigation behaviors as
`callback` without navigation support. Without inline reply, input becomes a
same-id button that returns `action(id)`, or `send` returns `not-supported` if no
button slot remains after the listed actions, which keep precedence.

A `notification-action` describes a labeled choice and its `behavior`:
`deep-link`, `url`, or `callback`. `default-behavior` applies to body clicks.
Application-specific actions use `callback` and are handled by the consumer.
These records and `text-input` describe requested presentation; the provider
owns rendering and platform UI.

Action and input ids and replacement tags are never truncated. Providers accept
at least 64 UTF-8 bytes and reject invalid or oversized values with
`invalid-argument`.

`tag` is a caller-chosen replacement key, such as `"build-status"`. Sending it
again closes the previous pending notification in the same scope and returns a
new `sent.id`. The tag is not that generated id, and replacing a notification
does not change an already completed result. `none` disables replacement; tags
are trimmed and blank tags are invalid.

## Errors

While the service is unavailable, `get-features`, `send`, and `subscribe` return
`unavailable`.
`close` still settles a pending id as `closed`, and the provider withdraws the
native notification once it can. Declined platform authorization is
`unavailable`; caller revocation is `access-denied`. `error-code.other` requires
a nonblank description,
using a generic explanation if the service provides none. Error strings are
diagnostic; don't parse them.

## Publishing

The build script fetches the digest-pinned clocks dependency and verifies that
fetching preserves the committed lockfile:

```sh
node .github/scripts/wit/build.mjs wit/cosmonic-notifications cosmonic:notifications
```

The artifact is `dist/notifications.wasm`, published at
`ghcr.io/cosmonic-labs/cosmonic/notifications:0.3.0` through
[`../wkg-registries.toml`](../wkg-registries.toml).

After the first publish, make the GHCR package public like the other packages.
Pull-request builds check the registry anonymously and skip the contents
comparison for a package they cannot see, so a private package never fails a
changed-contents-without-bump drift check.

`cosmonic:notify@0.2.0` remains published at
`ghcr.io/cosmonic-labs/cosmonic/notify` and receives no further releases; this
package supersedes it.
