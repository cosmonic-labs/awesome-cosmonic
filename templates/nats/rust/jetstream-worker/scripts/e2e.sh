#!/usr/bin/env bash
# End-to-end smoke for the deployed JetStream Pull Worker: drive one message through the
# component and assert the effect. If this passes, the component is bound to
# NATS and its grant chain is right.
#
# Runs against the NATS that Cosmonic Desktop dials by default
# (nats://127.0.0.1:4222) through a local `nats` CLI
# (`brew install nats-io/nats-tools/nats`). Point it elsewhere with NATS_URL,
# or swap the whole command for a cluster:
#   NATS="kubectl -n wasmcloud exec deploy/natsbox -- nats --server nats://nats:4222" ./scripts/e2e.sh
#
# Needs: the LOAD stream (load.>), a durable pull consumer on it (pull-workers, filter load.pull.>), and the RECEIPTS stream (done.>). Override STREAM / CONSUMER to match yours.
set -uo pipefail
RUN=${RUN:-e2e-$$}
NATS=${NATS:-"nats --server ${NATS_URL:-nats://127.0.0.1:4222}"}
WAIT=${WAIT:-15}   # seconds to wait for the effect

fail() { echo "FAIL: $*" >&2; exit 1; }
# wait_receipt SUBJECT [GLOB...] — print the newest message body on SUBJECT in
# the RECEIPTS stream once one exists (matching one of the GLOBs, if given);
# fail after $WAIT seconds.
wait_receipt() {
  local subject=$1; shift
  local deadline=$((SECONDS + WAIT)) json body g
  while [ "$SECONDS" -lt "$deadline" ]; do
    if json=$($NATS stream get RECEIPTS --last-for "$subject" --json 2>/dev/null); then
      body=$(printf '%s' "$json" | jq -r '.data' | base64 -d 2>/dev/null)
      if [ $# -eq 0 ]; then printf '%s\n' "$body"; return 0; fi
      # shellcheck disable=SC2254  # the GLOBs are meant to glob
      for g in "$@"; do case $body in $g) printf '%s\n' "$body"; return 0;; esac; done
    fi
    sleep 0.5
  done
  return 1
}
need_stream() { $NATS stream info "$1" >/dev/null 2>&1 || fail "no $1 stream — create it: nats stream add $1 --subjects '$2' --defaults"; }

STREAM=${STREAM:-LOAD}
CONSUMER=${CONSUMER:-pull-workers}
need_stream RECEIPTS 'done.>'
$NATS consumer info "$STREAM" "$CONSUMER" >/dev/null 2>&1 \
  || fail "no pull consumer $CONSUMER on $STREAM — create it: nats consumer add $STREAM $CONSUMER --pull --filter 'load.pull.>' --ack explicit --deliver all --defaults"
echo "== queueing one message on load.pull.msg (run=$RUN)"
$NATS pub load.pull.msg "run=$RUN;pad=" >/dev/null || fail "publish failed — is nats-server reachable at ${NATS_URL:-nats://127.0.0.1:4222}?"
echo "== triggering the worker on pull.run (stream=$STREAM consumer=$CONSUMER batch=10 rounds=3)"
$NATS pub pull.run "run=$RUN;stream=$STREAM;consumer=$CONSUMER;batch=10;rounds=3;timeoutms=1000;pad=" >/dev/null || fail "publish failed"
echo "== waiting for the total= receipt on done.js-pull.$RUN"
body=$(wait_receipt "done.js-pull.$RUN" "total=*" "*error=*") \
  || fail "no total= receipt on done.js-pull.$RUN within ${WAIT}s — is nats-jetstream-worker running? Check its logs. Per-round receipts so far: $($NATS stream view RECEIPTS --subject "done.js-pull.$RUN" --raw 2>/dev/null | tr '\n' ' ')"
echo "   receipt: $body"
case $body in
  "error=open;"*) fail "the worker could not open $STREAM/$CONSUMER: $body — subject-allow must include the consumer's filter (load.pull.>) next to stream-allow $STREAM";;
  *"error=fetch;"*) fail "a fetch failed: $body — see nats-tuning.md §2.4 (consumer limits, or message handles still held)";;
  "total="*) ;;
  *) fail "unexpected receipt: $body";;
esac
total=${body#total=}
[ "$total" -ge 1 ] 2>/dev/null \
  || fail "the worker fetched nothing (total=$total) — is $CONSUMER's filter load.pull.>, and did the queued message land in $STREAM? (nats stream info $STREAM)"
echo "PASS: jetstream-worker fetched and acked $total message(s) from $STREAM/$CONSUMER and published done.js-pull.$RUN"
