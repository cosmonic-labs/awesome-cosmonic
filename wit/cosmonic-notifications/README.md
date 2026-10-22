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

Calls carry no caller identity. Trusted wiring binds `notifier` and `events` to
one caller scope, which owns ids, tags, rate budget and queued results across
invocations. A terminal component provider instance serves exactly one scope.

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
provider writes the result to `sent.response`, even if the future is held but
unread. If the future was dropped, the result is queued instead. Await the
future or drop it; one held unread is neither delivered nor queued. Dropping the
future never withdraws the notification; `close` does. Delivery is best effort,
not durable.

| Response | Meaning |
| --- | --- |
| `activated`, `action`, `reply` | The user answered. |
| `dismissed` | The user dismissed it (requires `dismissal` support). |
| `expired` | The deadline passed, or a dismissal wasn't observable. Don't resend automatically. |
| `closed` | `close`, tag replacement, or orderly shutdown. No user decision; resending is fine. |
| `other` | Anything else. Don't assume an answer or resend. |

`timeout` is a `wasi:clocks` `duration`: `none` uses the provider default, zero
expires immediately, and a finite maximum bounds every wait.

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

`get-features` reports what the service supports. Providers adapt rather than
fail: they truncate extra actions and long text, fall back to normal urgency,
and treat navigation targets as `callback` without navigation support. Without
inline reply, input becomes a same-id button that returns `action(id)`, or
`send` returns `not-supported` if no button slot remains after the listed
actions, which keep precedence.

Ids and tags are never truncated. Providers accept at least 64 UTF-8 bytes and
reject invalid or oversized values with `invalid-argument`.

## Errors

While the service is unavailable, `send` and `subscribe` return `unavailable`.
`close` still settles a pending id as `closed`, and the provider withdraws the
native notification once it can. Declined platform authorization is
`unavailable`; caller revocation is `access-denied`. Error strings are
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
