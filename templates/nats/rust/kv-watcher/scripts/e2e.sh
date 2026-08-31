#!/usr/bin/env bash
# End-to-end smoke for the deployed KV Watcher: drive one message through the
# component and assert the effect. If this passes, the component is bound to
# NATS and its grant chain is right.
#
# Runs against the NATS that Cosmonic Desktop dials by default
# (nats://127.0.0.1:4222) through a local `nats` CLI
# (`brew install nats-io/nats-tools/nats`). Point it elsewhere with NATS_URL,
# or swap the whole command for a cluster:
#   NATS="kubectl -n wasmcloud exec deploy/natsbox -- nats --server nats://nats:4222" ./scripts/e2e.sh
#
# Needs: the KV bucket it watches (appkv; override BUCKET) and the RECEIPTS stream (done.>).
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
KEY=e2e-$RUN
need_stream RECEIPTS 'done.>'
$NATS kv info "$BUCKET" >/dev/null 2>&1 || fail "no KV bucket $BUCKET — create it: nats kv add $BUCKET"
echo "== writing $BUCKET/$KEY (run=$RUN)"
$NATS kv put "$BUCKET" "$KEY" "run=$RUN;pad=" >/dev/null || fail "kv put failed — is nats-server reachable at ${NATS_URL:-nats://127.0.0.1:4222}?"
echo "== waiting for the watch receipt on done.kv-watch.$RUN"
body=$(wait_receipt "done.kv-watch.$RUN") \
  || fail "no receipt on done.kv-watch.$RUN within ${WAIT}s — is nats-kv-watcher running with kv-watches: $BUCKET:> and bucket-allow $BUCKET? Check its logs."
echo "   receipt: $body"
# op= renders as `Put` from the Go template and `KvOperation::Put` from the Rust
# one (Rust formats the enum with {:?}); accept both.
case $body in "bucket=$BUCKET;key=$KEY;op="*"Put;rev="*) ;; *) fail "unexpected receipt body: $body";; esac
echo "PASS: kv-watcher saw the put on $BUCKET/$KEY and published done.kv-watch.$RUN"
# The key is left in place on purpose: deleting it fires a second watch event
# (op=Delete carries no value, so its receipt would land on done.kv-watch.na).
