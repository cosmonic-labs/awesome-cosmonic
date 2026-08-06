# Host Plugins

Cosmonic Control schedules workloads onto [wasmCloud](https://github.com/wasmCloud/wasmCloud)
hosts. [Host plugins](https://wasmcloud.com/docs/overview/hosts/plugins) extend
a host with an implementation of a WIT world, linked to workloads at runtime.
Two flavors, split by how the plugin is built:

- [`native/`](native/): Rust, implementing the `HostPlugin` trait, compiled
  into the host binary.
- [`component/`](component/): built as a Wasm component and loaded into a host
  at runtime.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for requirements.
