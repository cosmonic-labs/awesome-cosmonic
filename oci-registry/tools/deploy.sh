#!/usr/bin/env bash
# Apply the registry Workload to a running Cosmonic Desktop daemon and wait for
# it to come up. Builds the apply JSON from a few variables (the durable,
# portable artifact is deploy/workload.yaml; this script is the local-dev
# convenience that also creates the hostPath dir the runtime requires).
#
# Usage:
#   tools/deploy.sh                 # deploy ghcr image, data in ~/.cosmonic/oci-registry-data
#   IMAGE=ghcr.io/you/oci-registry:0.1.0 DATA_DIR=/path tools/deploy.sh
set -euo pipefail

NAME="${NAME:-oci-registry}"
NAMESPACE="${NAMESPACE:-default}"
IMAGE="${IMAGE:-ghcr.io/liamrandall/oci-registry:0.1.0}"
HOSTNAME_="${HOST:-oci-registry.localhost}"
DATA_DIR="${DATA_DIR:-$HOME/.cosmonic/oci-registry-data}"
SOCK="${COSMONIC_SOCK:-$HOME/Library/Application Support/Cosmonic/cosmonicd.sock}"

[[ -S "$SOCK" ]] || { echo "cosmonicd socket not found at $SOCK — is Cosmonic Desktop running?" >&2; exit 1; }

# The runtime requires the hostPath volume to already exist.
mkdir -p "$DATA_DIR"

read -r -d '' BODY <<JSON || true
{
  "apiVersion": "runtime.wasmcloud.dev/v1alpha1",
  "kind": "Workload",
  "metadata": { "name": "$NAME", "namespace": "$NAMESPACE",
    "labels": { "app.kubernetes.io/name": "oci-registry", "app.kubernetes.io/version": "0.1.0" } },
  "spec": {
    "hostInterfaces": [
      { "namespace": "wasi", "package": "http", "interfaces": ["incoming-handler"],
        "config": { "host": "$HOSTNAME_" } }
    ],
    "volumes": [ { "name": "data", "hostPath": { "path": "$DATA_DIR" } } ],
    "components": [
      { "name": "$NAME", "image": "$IMAGE", "poolSize": 1,
        "localResources": {
          "allowedHosts": [],
          "volumeMounts": [ { "name": "data", "mountPath": "/data" } ],
          "environment": { "config": { "REGISTRY_ROOT": "/data" } }
        } }
    ]
  }
}
JSON

echo ">> applying $NAME ($IMAGE), data at $DATA_DIR"
curl -sS --unix-socket "$SOCK" -X POST http://d/v1/workloads \
  -H 'content-type: application/json' --data-binary "$BODY" >/dev/null

echo ">> waiting for it to run"
for i in $(seq 1 60); do
  state=$(curl -s --unix-socket "$SOCK" "http://d/v1/workloads/$NAMESPACE/$NAME" \
    | sed -n 's/.*"state":"\([a-z]*\)".*/\1/p' | head -1)
  case "$state" in
    running) echo ">> running"; break ;;
    failed|crashloop) echo "!! workload $state" >&2; exit 1 ;;
  esac
  sleep 2
done

# Discover the daemon's HTTP ingress address for the hint below.
ING=$(curl -s --unix-socket "$SOCK" http://d/v1/host | sed -n 's/.*"httpAddr":"\([^"]*\)".*/\1/p')
ING="${ING:-127.0.0.1:8200}"
cat <<EOF

deployed. reach it via the Host header:

  curl -H 'Host: $HOSTNAME_' http://$ING/v2/
  oras push --plain-http --resolve '$HOSTNAME_:80:$ING' $HOSTNAME_/myimage:tag ./component.wasm

EOF
