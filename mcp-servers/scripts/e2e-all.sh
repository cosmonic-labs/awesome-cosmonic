#!/usr/bin/env bash
# Run every server's e2e suite in turn and summarize.
#
#   scripts/e2e-all.sh [--no-build] [name ...]
#
# Each suite is hermetic (wasmtime + a local fixture) except the ones marked
# Desktop-only in their README (postgres-mcp), which need Cosmonic Desktop
# running. Exit status is non-zero if any suite fails.
set -u
cd "$(dirname "$0")/.."
NOBUILD=""
[ "${1:-}" = "--no-build" ] && { NOBUILD="--no-build"; shift; }
if [ $# -gt 0 ]; then SERVERS=("$@"); else
  SERVERS=()
  for d in */; do [ -f "$d/scripts/e2e.sh" ] && SERVERS+=("${d%/}"); done
fi
OK=(); BAD=()
for s in "${SERVERS[@]}"; do
  echo "=================== $s ==================="
  if (cd "$s" && ./scripts/e2e.sh $NOBUILD); then OK+=("$s"); else BAD+=("$s"); fi
done
echo
echo "passed: ${OK[*]:-none}"
echo "failed: ${BAD[*]:-none}"
[ ${#BAD[@]} -eq 0 ]
