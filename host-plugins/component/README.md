# Host Component Plugins

Capabilities built as [WebAssembly components](https://wasmcloud.com/docs/runtime/creating-host-component-plugins/)
and deployed into a host at runtime as trigger services with a capability
ingress, so you ship, version, and sandbox them like any other component.

Currently opt-in via the `host-component-plugins` feature, so check the docs
for the state of play before depending on one.

One directory per hosted plugin. See [CONTRIBUTING.md](../../CONTRIBUTING.md)
for requirements.

- [`cron-scheduler`](cron-scheduler/): Calls workloads on crontab schedules,
  fixed intervals, and once at a chosen time. CI publishes tested builds to
  `ghcr.io/cosmonic-labs/host-plugins/cron-scheduler` with `latest` and
  `sha-<first 12 commit characters>` tags.
