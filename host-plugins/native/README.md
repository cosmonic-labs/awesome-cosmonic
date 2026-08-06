# Native Host Plugins

Rust implementations of the [`HostPlugin` trait](https://wasmcloud.com/docs/runtime/creating-host-plugins),
linked into the host binary. The right choice when a capability needs direct
host resources (filesystem, network, hardware) or has to run with the host's
privileges.

One directory per hosted plugin. See [CONTRIBUTING.md](../../CONTRIBUTING.md)
for requirements.
