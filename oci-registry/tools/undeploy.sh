#!/usr/bin/env bash
# Remove the registry Workload from the Cosmonic Desktop daemon. Leaves the
# on-disk registry data untouched (delete DATA_DIR yourself to wipe it).
set -euo pipefail

NAME="${NAME:-oci-registry}"
NAMESPACE="${NAMESPACE:-default}"
SOCK="${COSMONIC_SOCK:-$HOME/Library/Application Support/Cosmonic/cosmonicd.sock}"

[[ -S "$SOCK" ]] || { echo "cosmonicd socket not found at $SOCK" >&2; exit 1; }

echo ">> deleting workload $NAMESPACE/$NAME"
curl -sS --unix-socket "$SOCK" -X DELETE "http://d/v1/workloads/$NAMESPACE/$NAME" >/dev/null \
  && echo ">> removed (on-disk data preserved)"
