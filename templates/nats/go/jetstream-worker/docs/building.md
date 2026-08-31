# Building this template

```bash
make build     # -> jetstream-worker.wasm
make verify    # assert the export and the async ABI are really there
```

That is the whole build. This template uses **componentize-go's own generated
bindings** directly, so it needs no SDK and no vendored WASI deps - its world
imports only `wasmcloud:nats@0.1.0`.

## Why `-w` is passed explicitly

componentize-go discovers `componentize-go.toml` files from your module *and
its dependencies*, and merges the worlds they name. That is convenient until a
dependency names a world you did not want: the wasmCloud Go SDK's default
world (`wasmcloud:component-go/wasip2@0.2.0`) mandates a
`wasi:http/incoming-handler` export, and merging it fails a NATS component with

```
failed to find export of interface `wasi:http/incoming-handler@0.2.8` function `handle`
```

Passing `-w` pins the build to this template's world and avoids the surprise.

## If you switch to the wasmCloud Go SDK

`go.wasmcloud.dev/component` gives you a friendlier surface - `nats.Message`
with plain `string`/`[]Header` fields instead of `Option[...]`, `error` returns
instead of `Result[Unit, string]`, and typed errors. It costs about 1.8% in
component size and was not measurably slower.

Four things to know before you do:

1. **Use `component/v0.1.3` or later.** It is the first release that matches
   the landed `wasmcloud:nats` ABI (denied-resource as a variant,
   `already-settled`/`ack-owned-by-host`, `kv.keys` with a filter) and it
   ships the `sleep` package. A v0.1.2 component fails to bind outright:
   "expected variant of 17 cases, found 15 cases".
2. **Base your world on `include wasmcloud:component-go/headless@0.2.0`** -
   new in v0.1.3, it carries the CLI imports a Go runtime needs and no HTTP
   export. Do not follow the handler packages' doc examples that say
   `wasip3@0.2.0`: that world mandates a `wasi:http/service@0.3.0` export
   (the SDK's own `doc.go` says so), per the error above.
3. **`componentize-go build` only - never `bindings`.** The SDK ships its own
   generated bindings, and the `bindings` subcommand overwrites `go.mod`
   (renames the module, drops the SDK requirement) with exit 0.
4. **Passing `-w` then drops the WIT paths componentize-go discovered**, so the
   `wasi:cli` deps have to be vendored into `wit/deps/` by hand. The module
   cache is read-only, so the copy needs `chmod -R u+w`.

## Verifying

A Go component that builds but exports nothing is a real failure mode: the
standalone `wit-bindgen-go` generator silently drops every `async func` and
exits 0, producing a component with no handler at all. `make verify` and the CI
workflow both check for it.
