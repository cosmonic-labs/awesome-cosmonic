# wasmcloud:nats@0.1.0 is async-only WASI P3. componentize-go auto-installs a
# patched Go (golang/go#76775) for async worlds; stock Go cannot emit the
# component-model concurrency ABI.
#
# `-w` is passed explicitly so componentize-go targets this world only. Without
# it, a componentize-go.toml discovered from a dependency can merge another
# world in — the wasmCloud Go SDK's default world, for instance, mandates a
# wasi:http/incoming-handler export this component has no reason to provide.
WORLD := cosmonic:fan-out/fan-out@0.1.0
OUT   := fan-out.wasm

.PHONY: build bindings verify clean

build: bindings
	componentize-go -d ./wit -w "$(WORLD)" build -o $(OUT)

bindings:
	componentize-go -d ./wit -w "$(WORLD)" bindings --format
	go mod tidy

# A component that builds but exports nothing is a real failure mode here.
verify: build
	wasm-tools component wit $(OUT) | grep -q 'export wasmcloud:nats/core-handler@0.1.0'
	wasm-tools print $(OUT) | grep -qE 'async-lift|task-return'
	@echo "OK: exports core-handler@0.1.0, async-lifted"

clean:
	rm -f $(OUT) go.sum
	rm -rf wasmcloud_nats_* wit_exports.go
