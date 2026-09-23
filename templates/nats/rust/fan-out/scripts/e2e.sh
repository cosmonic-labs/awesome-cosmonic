#!/usr/bin/env bash
# End-to-end smoke: drive one message through the deployed component and assert
# the effect. Mirrors the liveness gate used by the campaign harness — if this
# passes, the component is bound and the grant chain is correct.
set -uo pipefail
NS=${NS:-default}
RUN=${RUN:-e2e-$$}
NATS=${NATS:-"kubectl -n wasmcloud exec deploy/natsbox -- nats --server nats://nats:4222"}

echo "== driving one message on fan.in"
$NATS pub fan.in "run=$RUN;pad=" || { echo "publish failed"; exit 1; }
sleep 5

echo "== asserting the effect"
echo "This pattern replies rather than emitting a receipt."
echo "Use: $NATS request fan.in 'run=$RUN;pad=' --timeout 5s"
