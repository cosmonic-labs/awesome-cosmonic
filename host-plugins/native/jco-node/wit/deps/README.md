# Vendored WIT

Third-party WIT this plugin implements or depends on.
Each keeps its own license.

- `jco-node-0.1.0/`:
  the `jco:node@0.1.0` contract,
  copied unchanged from [bytecodealliance/jco](https://github.com/bytecodealliance/jco)
  `packages/jco/lib/wit/builtin/jco-node-0.1.0/`
  at commit `035d5abe3a036f38b36492996bc2eded15ab1582`.
  Apache-2.0 WITH LLVM-exception, the Jco project's license.
- `wasi-io-0.2.12/`:
  `wasi:io@0.2.12`, from Jco's `builtin/0.2.12` snapshot of
  [WebAssembly/WASI](https://github.com/WebAssembly/WASI) tag `v0.2.12`.
  Covered by the WASI license in [`WASI-LICENSE.md`](WASI-LICENSE.md).
- `wasi-tls-0.2.0-draft/`:
  `wasi:tls@0.2.0-draft`, as shipped in the `wasmtime-wasi-tls` 48.0.9 crate
  (from [WebAssembly/wasi-tls](https://github.com/WebAssembly/wasi-tls)),
  with its `@unstable(feature = tls)` gates, so these bindings match the crate's.
  Covered by the WASI license in [`WASI-LICENSE.md`](WASI-LICENSE.md).

Jco's own copy of `wasi:tls@0.2.0-draft` drops the gates
and adds `is-available: func() -> bool`.
The plugin links that function by hand (see `src/plugin.rs`),
so components built by Jco instantiate.
