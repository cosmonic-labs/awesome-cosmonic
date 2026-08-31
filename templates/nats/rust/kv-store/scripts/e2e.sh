#!/usr/bin/env bash
# End-to-end smoke for the deployed KV Store Client: drive one message through the
# component and assert the effect. If this passes, the component is bound to
# NATS and its grant chain is right.
#
# Runs against the NATS that Cosmonic Desktop dials by default
# (nats://127.0.0.1:4222) through a local `nats` CLI
# (`brew install nats-io/nats-tools/nats`). Point it elsewhere with NATS_URL,
# or swap the whole command for a cluster:
#   NATS="kubectl -n wasmcloud exec deploy/natsbox -- nats --server nats://nats:4222" ./scripts/e2e.sh
#
# Needs: the KV bucket (appkv; override BUCKET) and the RECEIPTS stream (done.>).
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

BUCKET=${BUCKET:-appkv}
need_stream RECEIPTS 'done.>'
$NATS kv info "$BUCKET" >/dev/null 2>&1 || fail "no KV bucket $BUCKET — create it: nats kv add $BUCKET"
for op in put get cas del; do
  id=$RUN-$op
  echo "== op=$op on 10 keys $RUN-0..9 in $BUCKET (trigger on kv.run, run=$id)"
  $NATS pub kv.run "run=$id;bucket=$BUCKET;op=$op;ops=10;size=64;prefix=$RUN;pad=" >/dev/null \
    || fail "publish failed — is nats-server reachable at ${NATS_URL:-nats://127.0.0.1:4222}?"
  body=$(wait_receipt "done.kv-worker.$id") \
    || fail "no receipt on done.kv-worker.$id within ${WAIT}s — is nats-kv-store running? Check its logs: a bucket outside bucket-allow fails with 'open bucket $BUCKET failed', and a trigger without bucket= is rejected before any receipt."
  echo "   receipt: $body"
  [ "$body" = "op=$op;ok=10;err=0" ] || fail "$op did not complete cleanly: $body"
done
echo "PASS: kv-store put/get/cas/del 10 keys in $BUCKET (and cleaned them up)"
