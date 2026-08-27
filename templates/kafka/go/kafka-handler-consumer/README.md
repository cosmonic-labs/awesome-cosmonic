# kafka-handler-consumer (Go)

Same pattern and manifests as `../../rust/kafka-handler-consumer` — see `../../README.md`
for when to choose it. Implementation lives in
`gen/export_*/wit_bindings.go` (the stub file is yours to edit).

## Build

```sh
go install github.com/bytecodealliance/componentize-go@latest
componentize-go -d wit -w kafka-handler-consumer build -o kafka_handler_consumer.wasm   # run inside gen/
```

See `../README.md` for the toolchain details (patched Go, async support) and
the one generator bug this repo patches automatically.
