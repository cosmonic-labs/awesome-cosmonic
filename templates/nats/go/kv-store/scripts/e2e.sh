#!/usr/bin/env bash
# End-to-end smoke: drive one message through the deployed component and assert
# the effect. Mirrors the liveness gate used by the campaign harness — if this
# passes, the component is bound and the grant chain is correct.
set -uo pipefail
NS=${NS:-default}
RUN=${RUN:-e2e-$$}
NATS=${NATS:-"kubectl -n wasmcloud exec deploy/natsbox -- nats --server nats://nats:4222"}

echo "== driving one message on kv.run"
$NATS pub kv.run "run=$RUN;pad=" || { echo "publish failed"; exit 1; }
sleep 5

echo "== asserting the effect"
$NATS stream get RECEIPTS --last-for "done.kv-worker.$RUN" >/dev/null 2>&1 \
  && echo "PASS: receipt on done.kv-worker.$RUN" \
  || { echo "FAIL: no receipt — check the host log for a trap or a denied grant"; exit 1; }
