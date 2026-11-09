# Host component plugins

Capabilities built as
[WebAssembly components](https://wasmcloud.com/docs/runtime/creating-host-component-plugins/)
and loaded into a host at runtime. Because the plugin is itself a component, you
ship, version, and sandbox it like any other component, instead of rebuilding
the host.

Support is opt-in: the host has to be built with the `host-component-plugins`
Cargo feature (this repository's CI builds `wash` from source with it). Check
the docs for the current state before depending on one.

One directory per hosted plugin. See [CONTRIBUTING.md](../../CONTRIBUTING.md)
for requirements.

- [`cron-scheduler`](cron-scheduler/): Calls workloads on crontab schedules,
  on fixed intervals, and once at a chosen time. CI publishes tested builds to
  `ghcr.io/cosmonic-labs/host-plugins/cron-scheduler` with `latest` and
  `sha-<first 12 commit characters>` tags.
