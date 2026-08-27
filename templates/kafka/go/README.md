# Go templates — toolchain notes

These templates target the same WIT worlds as the Rust twins, built with
**standard Go + [componentize-go](https://github.com/bytecodealliance/componentize-go)**
(v0.4.1 tested, Go 1.25+). componentize-go downloads a patched Go toolchain
with component-model **async** support on first build — TinyGo tops out at
WASI P2 and cannot bind `cosmonic:kafka@0.3.0`'s async interfaces.

## Workflow

```sh
cd <template>/gen
go mod tidy
componentize-go -d ../wit -w <world> build -o ../<name>.wasm
```

Bindings were generated with:

```sh
componentize-go -d wit -w <world> bindings -o gen --generate-stubs --format
python3 ../fix-bindgen-int8.py gen     # see below
```

The file `gen/export_*/wit_bindings.go` was generated as a stub and is the
**implementation point** — edit it freely; regenerating without
`--generate-stubs` leaves it alone.

## Pitfalls found by the k8s test campaign (fixes baked in)

1. **WIT deps must come from the registry** (`wash wit fetch`), never copied from the wasmCloud
   repo's test fixtures: a fixture-WIT Go build deploys and then hangs `workload_start`
   forever with no error (k8s-perf FINDINGS.md K9).
2. **Go's GC collects host resources you stop referencing.** A service that holds a `Consumer`
   only during setup exits "successfully" mid-run when the GC drops it and the record stream
   ends (ERRORS.md E13). Idiom, used in these templates: `defer resource.Drop()` immediately
   after acquiring any resource the rest of the function depends on.

## Known generator bug (patched here)

wit-bindgen 0.59 (via componentize-go v0.4.1) lowers enum discriminants as
`int8(int32(N))`. `cosmonic:kafka`'s `error-code` enum has ~350 cases, so any
value above 127 is a Go compile-time constant overflow. `fix-bindgen-int8.py`
rewrites those constants to their two's-complement value after each
`bindings` run. Upstream issue material: the canonical ABI stores these
discriminants as u8; the generator should emit an unsigned store.
