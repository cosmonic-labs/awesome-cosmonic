# Native host plugins

Rust implementations of the
[`HostPlugin` trait](https://wasmcloud.com/docs/runtime/creating-host-plugins),
compiled into the host binary. Choose this flavor when a capability needs direct
access to host resources (filesystem, network, hardware) or has to run with the
host's privileges.

No plugins are hosted here yet. One directory per hosted plugin. See
[CONTRIBUTING.md](../../CONTRIBUTING.md) for requirements.
