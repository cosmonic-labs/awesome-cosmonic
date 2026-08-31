#!/usr/bin/env bash
# End-to-end smoke for the deployed Request / Reply: drive one message through the
# component and assert the effect. If this passes, the component is bound to
# NATS and its grant chain is right.
#
# Runs against the NATS that Cosmonic Desktop dials by default
# (nats://127.0.0.1:4222) through a local `nats` CLI
# (`brew install nats-io/nats-tools/nats`). Point it elsewhere with NATS_URL,
# or swap the whole command for a cluster:
#   NATS="kubectl -n wasmcloud exec deploy/natsbox -- nats --server nats://nats:4222" ./scripts/e2e.sh
#
# Needs: nothing beyond the running workload — the assertion is the reply itself.
set -uo pipefail
RUN=${RUN:-e2e-$$}
NATS=${NATS:-"nats --server ${NATS_URL:-nats://127.0.0.1:4222}"}
WAIT=${WAIT:-15}   # seconds to wait for the effect

fail() { echo "FAIL: $*" >&2; exit 1; }

BODY="run=$RUN;pad="
echo "== nats request svc.echo (run=$RUN, ${#BODY} bytes)"
reply=$($NATS request svc.echo "$BODY" --timeout 5s --raw 2>&1) \
  || fail "no reply within 5s ($reply) — is nats-request-reply running and subscribed to svc.echo? Core NATS has no error channel, so a handler error leaves the caller to time out: check the workload logs."
echo "   reply: $reply"
[ "$reply" = "echo:${#BODY}" ] || fail "expected 'echo:${#BODY}', got '$reply'"
echo "PASS: request-reply answered svc.echo with the request's byte count"
