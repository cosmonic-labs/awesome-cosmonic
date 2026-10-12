#!/usr/bin/env bash
# End-to-end test: load the plugin into a real host, deploy the ticker workload
# beside it, and check that schedules fire the way cosmonic:cron says they do.
#
#   e2e/run.sh
#   WASH=/path/to/wash e2e/run.sh
#
# Needs a wash built with the `host-component-plugins` feature, plus curl. It
# takes up to a minute and a half, most of it waiting for a crontab minute to
# come round.
set -uo pipefail

WASH=${WASH:-wash}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BASE=http://127.0.0.1:8787
LOG=$(mktemp -t cron-scheduler-e2e.XXXXXX)
DEV_PID=
FAILED=0

stop_host() {
  [ -n "$DEV_PID" ] || return 0
  kill -INT "$DEV_PID" 2>/dev/null
  wait "$DEV_PID" 2>/dev/null
  DEV_PID=
}
trap stop_host EXIT

pass() { printf '  ok    %s\n' "$1"; }
fail() {
  printf '  FAIL  %s\n' "$1"
  [ $# -gt 1 ] && printf '        %s\n' "$2"
  FAILED=1
}
check() { if "${@:2}"; then pass "$1"; else fail "$1"; fi; }

# Expect a POST to answer with a status and a body containing some text.
expect() {
  local got
  got=$(curl -s -X POST -w '\n%{http_code}' "$BASE$2")
  local status=${got##*$'\n'} body=${got%$'\n'*}
  if [ "$status" = "$3" ] && [[ "$body" == *"$4"* ]]; then
    pass "$1"
  else
    fail "$1" "expected $3 containing '$4', got $status '$body'"
  fi
}

# Lines the ticker logged for one schedule.
fired() { grep -c "ticker: fired name=$1 " "$LOG" || true; }
# Wait until the log holds a line matching a pattern, or give up.
wait_for() {
  local pattern=$1 limit=$2 waited=0
  until grep -q -- "$pattern" "$LOG"; do
    sleep 0.2
    waited=$((waited + 1))
    if [ "$waited" -ge $((limit * 5)) ]; then return 1; fi
  done
  return 0
}

echo "building the plugin and the ticker workload"
(cd "$ROOT" && "$WASH" build >/dev/null 2>&1) || { echo "plugin build failed"; exit 1; }
(cd "$ROOT/e2e/ticker" && "$WASH" build >/dev/null 2>&1) || { echo "ticker build failed"; exit 1; }

echo "starting the host (log: $LOG)"
(cd "$ROOT/e2e/ticker" && exec "$WASH" dev) >"$LOG" 2>&1 &
DEV_PID=$!
STARTED=$(date +%s)
if ! curl -sf --retry 60 --retry-delay 1 --retry-all-errors -o /dev/null "$BASE/schedules"; then
  echo "the host never served the workload:"
  tail -20 "$LOG"
  exit 1
fi

echo "declared schedules"
check "the plugin's dispatch loop started" grep -q 'cron-scheduler: dispatch loop started' "$LOG"
LISTED=$(curl -s "$BASE/schedules")
check "the binding's interval is listed as declared" \
  grep -q $'^heartbeat\tdeclared\tinterval 5000ms$' <<<"$LISTED"
check "the binding's crontab is listed as declared" \
  grep -q $'^minutely\tdeclared\tcron \* \* \* \* \*$' <<<"$LISTED"
check "a declared interval fires without the workload registering anything" \
  wait_for 'ticker: fired name=heartbeat ' 8

echo "registering at run time"
# The loop has just begun a five-second sleep for the next heartbeat. A
# registration has to cut that short, or these firings arrive late.
expect "register an interval" '/register?name=poll&every-ms=500' 200 ok
sleep 1.7
POLLS=$(fired poll)
if [ "$POLLS" -ge 2 ]; then
  pass "a new schedule wakes the sleeping loop ($POLLS firings in 1.7s)"
else
  fail "a new schedule wakes the sleeping loop" "$POLLS firings in 1.7s, expected at least 2"
fi
expect "registering the same schedule again is accepted" '/register?name=poll&every-ms=500' 200 ok
check "a registered schedule is listed as registered" \
  grep -q $'^poll\tregistered\tinterval 500ms$' <<<"$(curl -s "$BASE/schedules")"

echo "refusals"
expect "a name with a space" '/register?name=bad%20name&every-ms=1000' 400 invalid-name
expect "a period under min-interval" '/register?name=fast&every-ms=100' 400 invalid-schedule
expect "a malformed crontab" '/register?name=c&cron=not+a+cron' 400 invalid-schedule
expect "a crontab that never matches" '/register?name=c&cron=0+0+30+2+*' 400 invalid-schedule
expect "an unknown time zone" '/register?name=c&cron=0+9+*+*+*&tz=Mars/Phobos' 400 invalid-schedule
expect "a schedule kind the plugin lacks" '/register?name=o&other=every+full+moon' 400 unsupported
expect "replacing a declared schedule" '/register?name=heartbeat&every-ms=1000' 403 access-denied
expect "removing a declared schedule" '/unregister?name=heartbeat' 403 access-denied

echo "limits"
expect "a crontab in a named zone" '/register?name=ny&cron=0+9+*+*+mon-fri&tz=America/New_York' 200 ok
check "the zone is kept in the listing" \
  grep -q $'^ny\tregistered\tcron 0 9 \* \* mon-fri in America/New_York$' <<<"$(curl -s "$BASE/schedules")"
expect "a third registration" '/register?name=third&every-ms=60000' 200 ok
expect "a fourth, past max-registrations" '/register?name=fourth&every-ms=60000' 429 limit-exceeded
expect "unregister" '/unregister?name=third' 200 ok
expect "unregister again" '/unregister?name=third' 200 ok
expect "unregister the crontab" '/unregister?name=ny' 200 ok
# Registered and then left alone, for the max-lifetime check further down.
expect "register a schedule and forget it" '/register?name=linger&every-ms=60000' 200 ok

echo "unregistering stops the calls"
expect "unregister the interval" '/unregister?name=poll' 200 ok
sleep 0.3
BEFORE=$(fired poll)
sleep 1.5
AFTER=$(fired poll)
if [ "$BEFORE" = "$AFTER" ]; then
  pass "no firing after unregister"
else
  fail "no firing after unregister" "$((AFTER - BEFORE)) more arrived"
fi

echo "retry"
expect "register a handler that asks for a retry" '/register?name=retry-job&every-ms=4000' 200 ok
wait_for 'name=retry-job .* attempt=3' 9
ATTEMPTS=$(grep 'ticker: fired name=retry-job ' "$LOG" | head -3)
check "the firing is delivered three times" \
  test "$(awk '{print $NF}' <<<"$ATTEMPTS" | tr '\n' ' ')" = "attempt=1 attempt=2 attempt=3 "
check "every delivery carries the same scheduled-at" \
  test "$(awk '{print $4}' <<<"$ATTEMPTS" | sort -u | wc -l | tr -d ' ')" = 1
check "the backoff starts at retry.backoff" grep -q "'retry-job' .* will be retried in 500ms: attempt 1 of 3" "$LOG"
check "the backoff doubles" grep -q "'retry-job' .* will be retried in 1s: attempt 2 of 3" "$LOG"
expect "unregister it" '/unregister?name=retry-job' 200 ok

echo "failure"
expect "register a handler that fails" '/register?name=fail-job&every-ms=1000' 200 ok
check "the failure is logged with the handler's detail" \
  wait_for "'fail-job' .* failed: this schedule always fails" 4
sleep 1.2
check "a failed firing is not redelivered" test -z "$(grep 'name=fail-job ' "$LOG" | grep -v 'attempt=1$')"
check "the schedule carries on after a failure" test "$(fired fail-job)" -ge 2
expect "unregister it" '/unregister?name=fail-job' 200 ok

echo "overlap"
BEATS=$(fired heartbeat)
expect "register a handler slower than its period" '/register?name=slow-job&every-ms=1000' 200 ok
# Fires at 1s and runs until 3.5s, so the firings at 2s and 3s are skipped and
# the one at 4s reports them.
check "skipped firings are counted in the next one" wait_for 'name=slow-job .* missed=2 ' 8
check "no two deliveries of one schedule overlap" \
  test -z "$(grep -E 'ticker: (fired|finished) name=slow-job' "$LOG" | awk '{print $2}' | uniq -d)"
expect "unregister it" '/unregister?name=slow-job' 200 ok
check "other schedules kept firing while it ran" test "$(fired heartbeat)" -gt "$BEATS"

echo "a handler that traps"
expect "register a handler that panics" '/register?name=trap-job&every-ms=1000' 200 ok
check "the trap reaches the plugin as an error, not a crash" wait_for "'trap-job' .* failed" 5
expect "unregister it" '/unregister?name=trap-job' 200 ok
check "the plugin is still serving" curl -sf -o /dev/null "$BASE/schedules"
check "the plugin was not restarted" test "$(grep -c 'dispatch loop started' "$LOG")" = 1

echo "one-shot schedules"
held() { curl -s "$BASE/schedules" | grep -c "^$1"$'\t' || true; }
expect "register a delay" '/register?name=once-job&once-after-ms=1000' 200 ok
check "a pending one-shot is listed" \
  grep -q $'^once-job\tregistered\tonce-after 1000ms$' <<<"$(curl -s "$BASE/schedules")"
expect "registering it again is accepted" '/register?name=once-job&once-after-ms=1000' 200 ok
check "it fires" wait_for 'ticker: fired name=once-job ' 4
sleep 2.2
check "it fires exactly once" test "$(fired once-job)" = 1
check "it is removed once it has fired" test "$(held once-job)" = 0
AT=$(($(date +%s) + 3))
expect "register an instant" "/register?name=at-job&once-at=$AT" 200 ok
check "it fires with that instant as scheduled-at" wait_for "ticker: fired name=at-job scheduled-at=$AT " 6
expect "an instant in the past" "/register?name=late&once-at=$((AT - 3600))" 400 invalid-schedule
expect "a delay under min-interval" '/register?name=soon&once-after-ms=100' 400 invalid-schedule
expect "a delay past max-lifetime" '/register?name=far&once-after-ms=60000' 400 invalid-schedule
expect "a one-shot whose handler asks for a retry" '/register?name=retry-once&once-after-ms=1000' 200 ok
check "it is redelivered up to the third attempt" wait_for 'name=retry-once .* attempt=3' 6
sleep 0.5
check "it was delivered three times in all" test "$(fired retry-once)" = 3
check "it is removed after the last attempt" test "$(held retry-once)" = 0

echo "operator limits on a registration"
check "a forgotten schedule is removed at max-lifetime" \
  wait_for "'linger' .* was removed, it was held for the workload's max-lifetime" 14
check "it never fired" test "$(fired linger)" = 0
check "it is no longer listed" test "$(held linger)" = 0
expect "register a fast poll" '/register?name=capped&every-ms=500' 200 ok
check "it is removed at max-firings" \
  wait_for "'capped' .* was removed, it delivered the workload's max-firings" 8
check "it fired exactly max-firings times" test "$(fired capped)" = 10
check "it is no longer listed" test "$(held capped)" = 0
check "declared schedules are not subject to either limit" test "$(held heartbeat)" = 1

echo "crontab (waiting for the minute to turn)"
if wait_for 'ticker: fired name=minutely ' 65; then
  AT=$(grep 'ticker: fired name=minutely ' "$LOG" | head -1 | sed 's/.*scheduled-at=\([0-9]*\).*/\1/')
  check "a declared crontab fires" true
  check "scheduled-at is the minute boundary ($AT)" test $((AT % 60)) = 0
else
  fail "a declared crontab fires" "no firing within 65s"
fi

echo "interval rate"
ELAPSED=$(($(date +%s) - STARTED))
BEATS=$(fired heartbeat)
if [ "$BEATS" -ge $((ELAPSED / 5 - 2)) ] && [ "$BEATS" -le $((ELAPSED / 5 + 1)) ]; then
  pass "the 5s heartbeat fired $BEATS times in ${ELAPSED}s"
else
  fail "the 5s heartbeat keeps its rate" "$BEATS firings in ${ELAPSED}s"
fi
GAPS=$(grep 'ticker: fired name=heartbeat ' "$LOG" | sed 's/.*scheduled-at=\([0-9]*\).*/\1/' |
  awk 'NR > 1 { print $1 - prev } { prev = $1 }' | sort -u | tr '\n' ' ')
check "consecutive heartbeats are 5s apart (gaps: $GAPS)" test "$GAPS" = "5 "
check "no heartbeat was skipped" test -z "$(grep 'name=heartbeat ' "$LOG" | grep -v 'missed=0 ')"

echo "shutdown"
stop_host
check "the workload's schedules are dropped when it stops" grep -q 'cron-scheduler: unbound workload' "$LOG"

if [ "$FAILED" = 0 ]; then
  echo "PASS"
  rm -f "$LOG"
else
  echo "FAIL (host log kept at $LOG)"
fi
exit "$FAILED"
