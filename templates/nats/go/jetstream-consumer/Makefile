# wasmcloud:nats@0.1.0 is async-only WASI P3. componentize-go auto-installs a
# patched Go (golang/go#76775) for async worlds; stock Go cannot emit the
# component-model concurrency ABI.
#
# `-w` is passed explicitly so componentize-go targets this world only. Without
# it, a componentize-go.toml discovered from a dependency can merge another
# world in — the wasmCloud Go SDK's default world, for instance, mandates a
# wasi:http/incoming-handler export this component has no reason to provide.
WORLD := cosmonic:jetstream-consumer/jetstream-consumer@0.1.0
OUT   := jetstream-consumer.wasm

.PHONY: build bindings verify clean

build: bindings
	componentize-go -d ./wit -w "$(WORLD)" build -o $(OUT)

# componentize-go starts with `go list`, which needs a go.sum entry for the
# runtime package; go.sum is committed, and the download is a no-op then, but on
# a machine that never fetched the module it is exactly what the error says to
# run. `bindings` may also rewrite go.mod's pkg version to the one its generator
# was built against (v0.2.2 at the pinned commit) — commit what it writes.
bindings:
	go mod download go.bytecodealliance.org/pkg
	componentize-go -d ./wit -w "$(WORLD)" bindings --format
	go mod tidy

# A component that builds but exports nothing is a real failure mode here.
verify: build
	wasm-tools component wit $(OUT) | grep -q 'export wasmcloud:nats/jetstream-handler@0.1.0'
	wasm-tools print $(OUT) | grep -qE 'async-lift|task-return'
	@echo "OK: exports jetstream-handler@0.1.0, async-lifted"

clean:
	rm -f $(OUT)
	rm -rf wasmcloud_nats_* wit_exports.go
