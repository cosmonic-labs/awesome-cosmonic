# cron-scheduler

A host component plugin that implements [`cosmonic:cron@0.1.0`](../../../wit/cosmonic-cron/): it calls a workload's `handler` export on crontab schedules, on fixed intervals, and once at a chosen time. The workload holds no timer and runs no loop, so its components only exist while a firing is being handled.

The plugin is a WebAssembly component (`wasm32-wasip2`, WASI 0.3 interfaces). All of its timing comes from `wasi:clocks@0.3.0`: it sleeps on `monotonic-clock.wait-for`, measures intervals with `monotonic-clock.now`, and matches crontab expressions against `system-clock.now`.

**Experimental.** Host component plugins are opt-in in wasmCloud (the `host-component-plugins` build feature), and `cosmonic:cron` is at 0.1.0.

## Using it from a workload

There are two ways to be scheduled, and a workload can use both.

**Declare schedules in the workload's manifest.** The component only exports `cosmonic:cron/handler`:

```yaml
hostInterfaces:
  - namespace: cosmonic
    package: cron
    version: "0.1.0"
    interfaces: [handler]
    config:
      schedule.nightly-report: "0 2 * * *"
      interval.poll-inbox: 30s
      timezone: America/New_York
```

```rust
impl Guest for Component {
    async fn handle(firing: Firing) -> Result<(), HandlerError> {
        match firing.name.as_str() {
            "nightly-report" => build_report(firing.scheduled_at).await,
            "poll-inbox" => poll().await,
            _ => Ok(()),
        }
    }
}
```

**Register schedules at run time.** List `scheduler` as well, and the component can add and remove its own:

```yaml
    interfaces: [handler, scheduler]
```

```rust
// Every five seconds, until it is unregistered.
scheduler::register("poll-orders".into(), Schedule::Interval(5_000_000_000)).await?;
scheduler::unregister("poll-orders".into()).await?;

// Once, ten minutes from now. It is removed after it fires.
scheduler::register("check-back".into(), Schedule::OnceAfter(600_000_000_000)).await?;

// What is held for the workload, and when each will next fire.
for held in scheduler::list_registrations().await? {
    println!("{} is next due at {:?}", held.name, held.next);
}
```

A registered schedule is a crontab (`Schedule::Cron`), an interval, or one of two that fire once: `OnceAfter` a delay on the monotonic clock, or `OnceAt` an instant on the system clock. Declared schedules are always repeating.

**Register calendar schedules with typed fields.** `register-calendar` accepts selectors and named weekday and month flags:

```rust
scheduler::register_calendar("business-hours".into(), CalendarSchedule {
    minutes: vec![CalendarPart::Every(15)],
    hours: vec![CalendarPart::Range(CalendarRange { start: 9, end: 17 })],
    days_of_month: None,
    weekdays: Some(Weekdays::MONDAY | Weekdays::TUESDAY | Weekdays::WEDNESDAY
        | Weekdays::THURSDAY | Weekdays::FRIDAY),
    months: None,
    timezone: Some("America/New_York".into()),
}).await?;
```

Selectors accept a wildcard, one value, an inclusive range, a step from the field's minimum, or a stepped range; lists combine selections. Minutes are 0-59, hours 0-23, and days of month 1-31. Empty selectors or flags, reversed ranges, out-of-range values, and zero steps are rejected. Restricted day-of-month and weekday fields match with OR, as in POSIX cron. Selecting all seven weekdays is equivalent to leaving weekdays unset.

Both registration functions share names, permissions, limits, retries, and overlap protection. Listings return typed schedules as `Schedule::Cron` with a normalized expression; passing that expression back to `register` preserves the registration. Typed selections are normalized by value, so reordering or duplicating them changes nothing.

The host matches a `hostInterfaces` entry against one component at a time, so the component that imports `scheduler` must be the one that exports `handler`.

[`tests/fixtures/ticker`](tests/fixtures/ticker/) is a complete component that does both.

### Binding configuration

| Key | Meaning | Default |
| --- | --- | --- |
| `schedule.<name>` | Declares a crontab schedule. Five fields: minute, hour, day of month, month, day of week. | |
| `interval.<name>` | Declares an interval schedule. A period such as `500ms`, `30s` or `1h30m` (units `ms`, `s`, `m`, `h`, `d`). | |
| `timezone` | IANA zone that every `schedule.*` in the entry is read in. | UTC |
| `max-registrations` | Schedules the workload may hold through `scheduler.register`. Declared schedules do not count. | `32` |
| `min-interval` | Shortest interval period and shortest `once-after` delay the workload may use. Cannot be set below `10ms`. | `1s` |
| `retry.max-attempts` | Deliveries of one firing when the handler keeps answering `retry`, counting the first. | `3` |
| `retry.backoff` | Delay before the first redelivery. Doubles for each one after. | `1s` |

An unknown key or an unreadable value fails the bind, and the workload does not deploy. So does an entry that declares schedules or grants `scheduler` without listing `handler`. If a workload is bound with several `cosmonic:cron` entries, each `timezone` covers its own entry's `schedule.*` keys, and each of the other keys may be set by only one entry.

### What the plugin guarantees

The contract is documented on the interfaces in [`wit/cosmonic-cron`](../../../wit/cosmonic-cron/). In short:

- One schedule never overlaps itself. A firing that comes due while the previous one is still being handled is skipped, and the next delivered firing reports how many were skipped in `missed`. This holds by name, so replacing a schedule, or removing it and registering it again, waits for the firing in flight. Different schedules are delivered concurrently.
- A firing is not stored. If the workload is not running when it comes due, it is skipped and counted in `missed`.
- A handler that answers `retry` gets the same firing again, with `attempt` one higher, until `retry.max-attempts`, or until the schedule's next firing is due or came due while the handler was running. A repeating firing abandoned after `retry`, including an undeliverable retry, counts in the next delivery's `missed`. A handler that answers `failed`, traps or times out is not called again for that firing.
- Each firing has an opaque `id` for deduplication, stable across retries and distinct across clock corrections, re-registration, and plugin restarts. The plugin combines a random incarnation namespace with a firing counter. `scheduled-at` remains the due time and may repeat after a clock correction.
- An interval is a fixed rate on the monotonic clock. A late wake-up does not shift later firings.
- A crontab firing's `scheduled-at` is the instant the expression matched, not the time the handler ran.
- `list-registrations` gives each schedule's `next` firing as of the call, so a component need not parse a crontab to say when it runs. For a crontab or `once-at` schedule it is the `scheduled-at` that firing will carry. For an interval or `once-after` schedule it is the monotonic mark read off the system clock at that moment. It is `none` for a one-shot whose firing is already being handled. While a handler runs past its period, `next` is the occurrence after the ones being skipped.
- A schedule that fires once is removed when its firing has been handled or given up on. It is retried like any other firing, with nothing to cut the retries short. An instant that has already passed is refused. Its handler may register the same name again to be called once more.
- If a bound workload is bound again, registered schedules the new binding does not permit are removed: all of them if `scheduler` is no longer granted, any interval below a raised `min-interval`, the newest beyond a lowered `max-registrations`, and registrations whose names become declared schedules. The workload is not told directly; the plugin logs each removal. A declared replacement remains visible in `list-registrations`. Lowering `retry.max-attempts` also discards queued retries past the new limit: repeating firings count in `missed`, and spent one-shots are removed.
- `missed` is a lower bound. It counts firings skipped behind a running handler or an unreachable workload, and firings abandoned after `retry`. It does not count what came due while the plugin itself was down: schedules are rebuilt from the present after a restart.

### Limits of this implementation

- **Registered schedules live in memory.** If the plugin restarts, the host replays each workload's bind, which rebuilds the declared schedules, but schedules made with `scheduler.register` are gone. Register again when the component starts; registering a schedule that is already held changes nothing. A workload that may not be started for a while after a restart can declare an `interval` and register its schedules again from that handler.
- **No coordination between hosts.** Each host's plugin schedules the workloads bound on that host. A workload running on three hosts is called three times per firing.
- **Crontab parsing is [croner](https://docs.rs/croner) 4**, restricted to five fields. Beyond what `cosmonic:cron` promises it also accepts `@hourly`-style nicknames, `?` in a day field, `7` for Sunday, and the `L`, `#` and `W` modifiers. An expression that uses any of them may be refused, or read differently, by another implementer.
- **Daylight saving is croner's rule**, which `cosmonic:cron` leaves to the implementer. A schedule at a fixed time of day fires exactly once on every day, including the two on which the clocks change: a local time that is skipped fires at the first moment after the gap (`30 2 * * *` fires at 03:00), and one that occurs twice fires the first time only. A schedule that runs through the hour, such as `*/30 * * * *`, keeps its rate in elapsed time: nothing is added for the skipped hour, and the repeated hour is run through twice. Unit tests hold the parser to this.
- **The time zone database is compiled in** (chrono-tz 0.10), so new zone rules need a rebuild.
- While a crontab schedule is pending the loop wakes at least every 30 seconds to re-read the system clock, so a firing can be up to 30 seconds late after the system clock is set forward.
- **Clock steps follow Vixie cron's rule.** If the system clock moves back by three hours or less, crontab schedules wait for it to reach where it had been, so the repeated stretch is not run twice. A larger step back is taken as the clock being set, and they are scheduled again from the new time. After a step forward, or a long sleep, a schedule fires once for its most recent match, and `missed` stops counting at 100.
- **The log is sanitized.** Names, schedule text and handler error details come from workloads, so control characters are replaced and lines are cut at 512 characters.

## Other scheduler implementations

`cosmonic:cron` can also be implemented over a cloud scheduler or another timer service. The implementer maps schedules to backend triggers and tracks registration identity, running handlers, retries, and limits before calling the workload's `handler`. Duplicate or stale backend events must not become extra firings, and replacing or removing a registration must invalidate its pending backend events.

A backend can translate POSIX day matching into separate triggers and combine simultaneous occurrences into one firing. Provider time-zone rules may determine daylight saving behavior, which the interface leaves to the implementer. Subminute intervals need a finer timer when the provider only supports minute resolution. The current plugin is one implementation of the contract, with the limits listed above.

## Published component

CI publishes the plugin to `ghcr.io/cosmonic-labs/host-plugins/cron-scheduler`
after the build and host integration tests pass on `main`. `latest` follows
`main`; `sha-<first 12 commit characters>` identifies a build. A manual run of
the `cron-scheduler` workflow on `main` also publishes. Pull requests and
runs in forks only test and build.

Pull a published build to use it with the host commands below:

```sh
wash oci pull ghcr.io/cosmonic-labs/host-plugins/cron-scheduler:latest cron_scheduler.wasm
wash host --host-plugin id=cron-scheduler,file=cron_scheduler.wasm
```

For a pinned deployment, pull by the manifest digest shown in the workflow's
publish job summary. CI attaches GitHub build provenance to that digest. The
test ticker workload is included in CI artifacts but is not published to GHCR.
The host still needs the `host-component-plugins` feature described under [Run](#run).

## Build

Needs [`wash`](https://wasmcloud.com/docs/installation) and Rust with the `wasm32-wasip2` target. Tested with wash 2.10.3 and Rust 1.99.0.

```sh
wash build
```

The component lands at `target/wasm32-wasip2/release/cron_scheduler.wasm` (about 1.2 MB, most of it the time zone database). `cosmonic:cron` is read from [`wit/cosmonic-cron`](../../../wit/cosmonic-cron/) in this repository through [`wkg.toml`](wkg.toml); `wasi:clocks`, `wasi:cli` and `wasmcloud:host` are fetched from their registries and pinned in [`wkg.lock`](wkg.lock).

## Run

Load the component into a host built with the `host-component-plugins` feature:

```sh
wash host --host-plugin id=cron-scheduler,file=target/wasm32-wasip2/release/cron_scheduler.wasm
```

or, for `wash dev`, from the workload project's `.wash/config.yaml`:

```yaml
dev:
  plugins:
    - id: cron-scheduler
      file: path/to/cron_scheduler.wasm
  host_interfaces:
    - namespace: cosmonic
      package: cron
      version: "0.1.0"
      interfaces: [handler]
      config:
        interval.heartbeat: 5s
```

The released `wash` binaries are built without the feature. Build one from the [wasmCloud](https://github.com/wasmCloud/wasmCloud) repository:

```sh
cargo build -p wash --features host-component-plugins
```

The plugin logs to stderr with the prefix `cron-scheduler:` when a workload binds or unbinds, when a schedule is registered, changed or removed, and when a delivery does not succeed. A `register` or `unregister` that changes nothing is not logged, and a workload that cannot be called is reported when that starts, not once per firing.

## Test

CI checks formatting for both crates, runs native tests and Clippy, builds the plugin and test workload, and validates their required exports. Cargo dependencies and build outputs are cached for both crates.

A separate integration job runs the host test against those validated components. Its host is built from the pinned wasmCloud v2.10.3 commit `548bb21b65ac8cc03776f73656c4bf7be2f96992` with Rust 1.96.0 and `host-component-plugins`; the binary is cached by source revision, toolchain, platform, and build options. Failed host logs are uploaded as artifacts.

The schedule logic has no dependency on a clock or on the WIT bindings, so its unit tests run on the host:

```sh
cargo test
```

The end-to-end test, [`tests/e2e.rs`](tests/e2e.rs), builds the plugin and the ticker workload, loads both into `wash dev`, and checks declared and registered schedules, every refusal, the registration limit, retry, failure, overlap, a handler that traps, one-shot schedules, the `next` each listing gives, and a crontab firing. It is ignored by a plain `cargo test`, because it needs the `wash` described under [Run](#run) and takes between 40 seconds and a minute and a half, depending on how soon the next minute starts:

```sh
WASH=/path/to/wash-with-plugins cargo e2e
```

`cargo e2e` is an alias, set in [`.cargo/config.toml`](.cargo/config.toml), for `cargo test --test e2e -- --ignored --nocapture`. Set `CRON_E2E_PREBUILT=1` to use components already built in their normal output paths; CI uses this to test the artifacts produced by the build job.

Not covered end to end: several workloads on one host (unit-tested in [`src/engine.rs`](src/engine.rs)) and a plugin restart.

## Layout

- [`src/spec.rs`](src/spec.rs): parsing crontab expressions, periods, names and binding configuration.
- [`src/calendar.rs`](src/calendar.rs): validating typed calendar fields and compiling them into cron schedules.
- [`src/engine.rs`](src/engine.rs): which schedules each workload holds, when each is due, and what a finished delivery does.
- [`src/plugin.rs`](src/plugin.rs): the WIT bindings, the `scheduler` export, the lifecycle hooks and the dispatch loop.
- [`tests/e2e.rs`](tests/e2e.rs): the end-to-end test.
- [`tests/fixtures/ticker`](tests/fixtures/ticker/): the workload it deploys.
