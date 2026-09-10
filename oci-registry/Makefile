# oci-registry — single entry point for build, test, and dev.
# The same targets run locally and in CI; nothing is CI-only.

.PHONY: help build test test-unit test-integration dev clean fmt lint stats push

SHELL := /usr/bin/env bash
.SHELLFLAGS := -eu -o pipefail -c

WASM := target/wasm32-wasip2/release/oci_registry.wasm
DEV_ADDR ?= 127.0.0.1:8080
# Where `wash oci push` should send images (override for a remote registry).
REGISTRY ?= $(DEV_ADDR)

help: ## Show this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  %-16s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ----- build -----------------------------------------------------------------

build: ## Build the registry component (wasm32-wasip2)
	cargo build --target wasm32-wasip2 --release
	@echo "built $(WASM) ($$(du -h $(WASM) | cut -f1))"

# ----- test ------------------------------------------------------------------

test: test-unit test-integration ## Run unit + integration tests

test-unit: ## Host-target unit/integration tests (no wasm runtime)
	cargo test --workspace

test-integration: build ## Boot wash dev and exercise the live registry with oras
	bash tests/integration.sh

# ----- dev -------------------------------------------------------------------

dev: build ## Run the registry under wash dev at $(DEV_ADDR)
	mkdir -p .cache/registry-data
	cd components/registry && wash dev

stats: build ## Print the component wasm size
	@du -h $(WASM)

# ----- publish ---------------------------------------------------------------

push: build ## Push the component to $(REGISTRY) (override REGISTRY=...)
	wash oci push --insecure $(REGISTRY)/oci-registry:dev $(WASM)

deploy: ## Deploy the Workload to a running Cosmonic Desktop (pulls from GHCR)
	bash tools/deploy.sh

undeploy: ## Remove the Workload from Cosmonic Desktop (keeps on-disk data)
	bash tools/undeploy.sh

# ----- ergonomics ------------------------------------------------------------

fmt: ## cargo fmt
	cargo fmt --all

lint: ## clippy with warnings denied
	cargo clippy --workspace --all-targets -- -D warnings

clean: ## Remove build artifacts and local registry data
	cargo clean
	rm -rf .cache
