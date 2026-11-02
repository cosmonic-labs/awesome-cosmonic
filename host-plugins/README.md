# Host plugins

Cosmonic Desktop and Cosmonic Control run workloads on
[wasmCloud](https://github.com/wasmCloud/wasmCloud) hosts.
[Host plugins](https://wasmcloud.com/docs/overview/hosts/plugins/) extend a host
with a new capability: an implementation of a WIT interface (a typed contract
between a component and whatever serves it) that workloads are linked to at
runtime. They come in two flavors, split by how the plugin is built:

- [`native/`](native/): Rust code implementing the `HostPlugin` trait, compiled
  into the host binary.
- [`component/`](component/): plugins built as WebAssembly components and
  loaded into a host at runtime.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for requirements.
