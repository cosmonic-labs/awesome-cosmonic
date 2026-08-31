#!/usr/bin/env bash
# End-to-end smoke for the deployed Fan-Out: drive one message through the
# component and assert the effect. If this passes, the component is bound to
# NATS and its grant chain is right.
#
# Runs against the NATS that Cosmonic Desktop dials by default
# (nats://127.0.0.1:4222) through a local `nats` CLI
# (`brew install nats-io/nats-tools/nats`). Point it elsewhere with NATS_URL,
# or swap the whole command for a cluster:
#   NATS="kubectl -n wasmcloud exec deploy/natsbox -- nats --server nats://nats:4222" ./scripts/e2e.sh
#
# Needs: nothing beyond the running workload — the script subscribes to fan.work itself and counts. Override FANOUT (default 25).
set -uo pipefail
RUN=${RUN:-e2e-$$}
NATS=${NATS:-"nats --server ${NATS_URL:-nats://127.0.0.1:4222}"}
WAIT=${WAIT:-15}   # seconds to wait for the effect

fail() { echo "FAIL: $*" >&2; exit 1; }

FANOUT=${FANOUT:-25}
out=$(mktemp)
trap 'rm -f "$out"' EXIT
# Subscribe BEFORE publishing: core NATS delivers only to subscribers that exist
# at publish time, and the component republishes the instant it is invoked.
$NATS sub fan.work --count "$FANOUT" --raw >"$out" 2>/dev/null &
SUB=$!
sleep "${SUB_WARMUP:-1}"
echo "== publishing one message on fan.in (run=$RUN fanout=$FANOUT)"
$NATS pub fan.in "run=$RUN;fanout=$FANOUT;pad=" >/dev/null \
  || { kill "$SUB" 2>/dev/null; fail "publish failed — is nats-server reachable at ${NATS_URL:-nats://127.0.0.1:4222}?"; }
echo "== waiting for $FANOUT copies on fan.work"
deadline=$((SECONDS + WAIT))
while kill -0 "$SUB" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.5; done
kill "$SUB" 2>/dev/null; wait "$SUB" 2>/dev/null
got=$(grep -c "^run=$RUN;" "$out")
echo "   received: $got/$FANOUT"
[ "$got" -eq "$FANOUT" ] \
  || fail "expected $FANOUT copies of run=$RUN on fan.work, got $got — is nats-fan-out running with subject-allow fan.in,fan.work? A partial count under load means the subscription shed (nats-tuning.md §2.7). Note: the component publishes, it does not reply, so \`nats request fan.in\` always times out."
echo "PASS: 1 message on fan.in became $got on fan.work"
