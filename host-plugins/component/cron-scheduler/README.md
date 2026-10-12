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
```

A registered schedule is a crontab (`Schedule::Cron`), an interval, or one of two that fire once: `OnceAfter` a delay on the monotonic clock, or `OnceAt` an instant on the system clock. Declared schedules are always repeating.

The host matches a `hostInterfaces` entry against one component at a time, so the component that imports `scheduler` must be the one that exports `handler`.

[`e2e/ticker`](e2e/ticker/) is a complete component that does both.

### Binding configuration

| Key | Meaning | Default |
| --- | --- | --- |
| `schedule.<name>` | Declares a crontab schedule. Five fields: minute, hour, day of month, month, day of week. | |
| `interval.<name>` | Declares an interval schedule. A period such as `500ms`, `30s` or `1h30m` (units `ms`, `s`, `m`, `h`, `d`). | |
| `timezone` | IANA zone that every `schedule.*` in the entry is read in. | UTC |
| `max-registrations` | Schedules the workload may hold through `scheduler.register`. Declared schedules do not count. | `32` |
| `min-interval` | Shortest interval period and shortest `once-after` delay the workload may use. Cannot be set below `10ms`. | `1s` |
| `max-lifetime` | How long a registered schedule is held before the plugin removes it. | no limit |
| `max-firings` | How many firings a registered schedule delivers before the plugin removes it. Retries are not counted. | no limit |
| `retry.max-attempts` | Deliveries of one firing when the handler keeps answering `retry`, counting the first. | `3` |
| `retry.backoff` | Delay before the first redelivery. Doubles for each one after. | `1s` |

An unknown key or an unreadable value fails the bind, and the workload does not deploy.

`max-lifetime` and `max-firings` are the operator's bound on a schedule that a component registers and then stops tending. They never apply to declared schedules. Registering a name again with the schedule it already has does not reset either one. A schedule removed this way is not announced to the workload; the plugin logs it, and it is gone from `list-registrations`.

### What the plugin guarantees

The contract is documented on the interfaces in [`wit/cosmonic-cron`](../../../wit/cosmonic-cron/). In short:

- One schedule never overlaps itself. A firing that comes due while the previous one is still being handled is skipped, and the next delivered firing reports how many were skipped in `missed`. Different schedules are delivered concurrently.
- A firing is not stored. If the workload is not running when it comes due, it is skipped and counted in `missed`.
- A handler that answers `retry` gets the same firing again, with `attempt` one higher, until `retry.max-attempts` or until the schedule's next firing is due. A handler that answers `failed`, traps or times out is not called again for that firing.
- An interval is a fixed rate on the monotonic clock. A late wake-up does not shift later firings.
- A crontab firing's `scheduled-at` is the instant the expression matched, not the time the handler ran.
- A schedule that fires once is removed when its firing has been handled or given up on. It is retried like any other firing, with nothing to cut the retries short. An instant that has already passed is refused, as is one the workload's `max-lifetime` would end first.

### Limits of this implementation

- **Registered schedules live in memory.** If the plugin restarts, the host replays each workload's bind, which rebuilds the declared schedules, but schedules made with `scheduler.register` are gone. Register again when the component starts; registering a schedule that is already held changes nothing.
- **No coordination between hosts.** Each host's plugin schedules the workloads bound on that host. A workload running on three hosts is called three times per firing.
- **Crontab parsing is [croner](https://docs.rs/croner) 4**, restricted to five fields. It also accepts `@hourly`-style nicknames and the `L`, `#` and `W` modifiers, which are outside what `cosmonic:cron` promises. In a zone with daylight saving, croner's rules apply to local times that are skipped or repeated.
- **The time zone database is compiled in** (chrono-tz 0.10), so new zone rules need a rebuild.
- While a crontab schedule is pending the loop wakes at least every 30 seconds to re-read the system clock, so a firing can be up to 30 seconds late after the system clock is set forward.

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

The plugin logs to stderr with the prefix `cron-scheduler:` when a workload binds or unbinds, when a schedule is registered or removed, and when a delivery does not succeed.

## Test

The schedule logic has no dependency on a clock or on the WIT bindings, so its unit tests run on the host:

```sh
cargo test
```

The end-to-end test loads the plugin into `wash dev` beside the ticker workload and checks declared and registered schedules, every refusal, the registration limit, retry, failure, overlap, a handler that traps, one-shot schedules, `max-lifetime` and `max-firings`, and a crontab firing. It takes between 40 seconds and a minute and a half, depending on how soon the next minute starts:

```sh
WASH=/path/to/wash-with-plugins e2e/run.sh
```

Not covered end to end: several workloads on one host (unit-tested in [`src/engine.rs`](src/engine.rs)) and a plugin restart.

## Layout

- [`src/spec.rs`](src/spec.rs): parsing crontab expressions, periods, names and binding configuration.
- [`src/engine.rs`](src/engine.rs): which schedules each workload holds, when each is due, and what a finished delivery does.
- [`src/plugin.rs`](src/plugin.rs): the WIT bindings, the `scheduler` export, the lifecycle hooks and the dispatch loop.
- [`e2e/`](e2e/): the ticker workload and the end-to-end script.
