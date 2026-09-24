#!/usr/bin/env bash
# End-to-end test: boot `wash dev`, push and pull with a real OCI client
# (oras), and assert the distribution-spec endpoints behave. Runs locally and
# in CI alike — the registry is a pure reactor with no service sidecar, so it
# works under `wash dev` in a headless runner.
#
# Usage:
#   tests/integration.sh                 # boots its own wash dev
#   REGISTRY=127.0.0.1:8080 tests/integration.sh --no-boot   # use a running one

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REGISTRY="${REGISTRY:-127.0.0.1:8080}"
ADDR="http://${REGISTRY}"
BOOT=1
[[ "${1:-}" == "--no-boot" ]] && BOOT=0

WASM="${ROOT}/target/wasm32-wasip2/release/oci_registry.wasm"
DEV_PID=""
TMP="$(mktemp -d)"

cleanup() {
  [[ -n "$DEV_PID" ]] && kill "$DEV_PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"; }
need oras
need curl

# ----- build + boot ---------------------------------------------------------

echo ">> building component"
( cd "$ROOT" && cargo build --target wasm32-wasip2 --release >/dev/null )
[[ -f "$WASM" ]] || fail "component not built at $WASM"

if [[ "$BOOT" == "1" ]]; then
  echo ">> booting wash dev"
  # wash dev requires the hostPath volume (.wash/config.yaml) to pre-exist.
  mkdir -p "$ROOT/.cache/registry-data"
  ( cd "$ROOT/components/registry" && wash dev --non-interactive >"$TMP/dev.log" 2>&1 ) &
  DEV_PID=$!
fi

echo ">> waiting for registry at $ADDR"
for i in $(seq 1 90); do
  if curl -fsS "$ADDR/v2/" >/dev/null 2>&1; then break; fi
  sleep 1
  [[ "$i" == "90" ]] && { cat "$TMP/dev.log" 2>/dev/null; fail "registry never came up"; }
done

# ----- assertions -----------------------------------------------------------

echo ">> /v2/ API version"
curl -fsS -i "$ADDR/v2/" | grep -qi "docker-distribution-api-version: registry/2.0" \
  && pass "advertises registry/2.0" || fail "missing api-version header"

echo ">> oras push"
REPO="test/oci-registry"
TAG="ci"
# oras rejects absolute file paths, so push from the artifact's directory.
( cd "$(dirname "$WASM")" && oras push --plain-http "$REGISTRY/$REPO:$TAG" \
    --artifact-type application/wasm \
    "$(basename "$WASM"):application/wasm" >/dev/null 2>&1 ) \
  && pass "pushed $REPO:$TAG" || fail "oras push failed"

echo ">> oras pull + byte-identity"
( cd "$TMP" && oras pull --plain-http "$REGISTRY/$REPO:$TAG" >/dev/null 2>&1 )
PULLED="$(find "$TMP" -name oci_registry.wasm | head -1)"
[[ -n "$PULLED" ]] || fail "pull produced no artifact"
if command -v sha256sum >/dev/null 2>&1; then HASH=sha256sum; else HASH="shasum -a 256"; fi
A="$($HASH "$WASM" | awk '{print $1}')"
B="$($HASH "$PULLED" | awk '{print $1}')"
[[ "$A" == "$B" ]] && pass "pulled bytes match ($A)" || fail "digest mismatch: $A != $B"

echo ">> catalog lists the repo"
curl -fsS "$ADDR/v2/_catalog" | grep -q "\"$REPO\"" \
  && pass "catalog contains $REPO" || fail "catalog missing $REPO"

echo ">> tags list"
curl -fsS "$ADDR/v2/$REPO/tags/list" | grep -q "\"$TAG\"" \
  && pass "tag $TAG listed" || fail "tag missing"

echo ">> HEAD manifest returns digest"
curl -fsS -I "$ADDR/v2/$REPO/manifests/$TAG" | grep -qi "docker-content-digest:" \
  && pass "manifest HEAD has content-digest" || fail "no content-digest on HEAD"

echo ">> unknown blob 404 with BLOB_UNKNOWN"
code=$(curl -s -o "$TMP/err.json" -w '%{http_code}' \
  "$ADDR/v2/$REPO/blobs/sha256:$(printf '0%.0s' {1..64})")
[[ "$code" == "404" ]] && grep -q "BLOB_UNKNOWN" "$TMP/err.json" \
  && pass "404 + BLOB_UNKNOWN envelope" || fail "expected 404 BLOB_UNKNOWN, got $code"

echo ">> health endpoint"
curl -fsS "$ADDR/healthz" | grep -q '"status":"ok"' \
  && pass "healthz ok" || fail "healthz not ok"

echo ""
echo "ALL INTEGRATION CHECKS PASSED"
