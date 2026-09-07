#!/usr/bin/env bash
# Trip a per-credential circuit breaker, then refuse without hitting upstream.
#
# Breakers are wired and unit-tested; this is the missing end-to-end: five
# consecutive failures open the breaker, and the next request is NoCredential
# rather than another call to an upstream that is already known to be failing.
#
# 408 is used on purpose. 5xx / 529 fail over and cool the account down for 30s
# via the scheduler, so the breaker never sees five failures. 408 is
# RetrySameAccount: one inbound request records `same_account_retries + 1`
# (default 3) failures, and the second trips the threshold of 5 partway through
# its own retries — the fifth failure opens the breaker, and the retry loop
# re-checks it before sending again, so the sixth attempt never leaves. Five
# POSTs, not six. The third request is then refused locally without reaching
# selection at all.
#
# That mid-request stop is finding G7: the probe was claimed once above the
# retry loop and never re-checked, so a credential that had this moment tripped
# its breaker still received every remaining retry — the exact traffic a breaker
# exists to stop, aimed at the credential it had just called unhealthy.
#
# Stage 5 is the other half of the same seam, and was covered nowhere: failover
# between two credentials. A breaker refusing a dead account is only useful if a
# live account beside it still serves, and every script so far ran one account
# per route — so a gateway that never failed over would have passed all of them.
# It gets its own route and its own key, because stages 1-4 need the opposite
# topology: an account with no alternative.
#
# Needs no credentials. The mock is the Python one, not aimock: the count of
# POSTs is the assertion, and its failures are deterministic.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

MOCK_PORT="${MOCK_PORT:-8098}"
WORK="$(mktemp -d)"
OK=0
ROUTE="breaker-$RANDOM"
FAILOVER_ROUTE="failover-$RANDOM"
# Fixtures, not credentials: the mock matches on the literal string and there is
# no provider on the other end of any of them.
DEAD_SECRET="FAKE-CREDENTIAL-FOR-TESTS"
FAILOVER_SECRET="FAKE-CREDENTIAL-ONE-OF-TWO"
LIVE_SECRET="FAKE-CREDENTIAL-TWO-OF-TWO"

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32m  ok  %s\033[0m\n' "$*"; }

seen() {
  curl -fsS "http://127.0.0.1:$MOCK_PORT/_seen" 2>/dev/null || echo "?"
}

# Distinct credentials the mock has been shown since the last call, as hashes:
# reading clears the list, so the count belongs to one request rather than to
# the whole run. Cumulative was the first version of this and it was wrong —
# "more than before" is true whenever a later stage merely uses a different
# account, which is not a failover.
credentials() {
  # `|| true`, not `|| echo 0`: `grep -c` prints its count AND exits 1 when the
  # count is zero, so the fallback appended a second zero and the diagnostic
  # below read "saw 0\n0 distinct credential(s)".
  curl -fsS "http://127.0.0.1:$MOCK_PORT/_credentials?reset=1" 2>/dev/null | grep -c . || true
}

cleanup() {
  local code=$?
  kill ${MOCK_PID:-0} ${GW_PID:-0} 2>/dev/null || true
  [ "$OK" = "1" ] && rm -rf "$WORK" || echo "logs kept in $WORK"
  exit $code
}
trap cleanup EXIT

say "1/5  infrastructure"
if [ -z "${OAG_DATABASE__URL:-}" ]; then
  just dev-up >/dev/null
fi
eval "$(just _verify-env)"
just migrate >/dev/null
pass "postgres, redis, schema"

say "2/5  mock that fails two named credentials and serves the rest"
# 408 for the account the breaker stages use, and 529 for whichever of stage 5's
# two accounts the scheduler happens to try first. The two statuses are not
# interchangeable: 408 is RetrySameAccount, which is what lets one inbound
# request record enough failures to open a breaker, while 5xx moves to another
# account — the behaviour stage 5 is about. And stage 5's failing credential is
# named by which one went first rather than in advance, because the scheduler's
# choice between two equal accounts is its own business: naming one here made
# the check pass on the run where it guessed right and fail on the next.
MOCK_FAIL_FOR_KEY="$DEAD_SECRET=408" MOCK_FAIL_FIRST_CREDENTIAL=529 PORT="$MOCK_PORT" \
  python3 deploy/test/mock-upstream.py >"$WORK/mock.log" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 20); do
  [ "$(seen)" = "0" ] && break
  sleep 0.5
done
[ "$(seen)" = "0" ] || fail "mock never became ready; see $WORK/mock.log"
pass "mock on :$MOCK_PORT (seen=$(seen))"

say "3/5  gateway, two isolated routes: one dead account, and one of each"
KEY="$(
  cargo run --quiet -p oag -- admin init --email breaker@localhost --route "$ROUTE" 2>/dev/null \
    | grep -oE 'oag_live_[0-9a-f]+' | head -1
)"
[ -n "$KEY" ] || fail "init produced no key"
cargo run --quiet -p oag -- admin catalog seed >/dev/null
cargo run --quiet -p oag -- admin account add --name "breaker-$ROUTE" --provider anthropic \
  --secret "$DEAD_SECRET" --route "$ROUTE" >/dev/null

# The second route, with two accounts on it. Both exist before the gateway
# starts: an account added to a running gateway is picked up on a refresh
# interval, and a test that waits on one is a test that fails on a slow runner
# for a reason that has nothing to do with failover.
FAILOVER_KEY="$(
  cargo run --quiet -p oag -- admin init --email failover@localhost --route "$FAILOVER_ROUTE" 2>/dev/null \
    | grep -oE 'oag_live_[0-9a-f]+' | head -1
)"
[ -n "$FAILOVER_KEY" ] || fail "init produced no key for $FAILOVER_ROUTE"
cargo run --quiet -p oag -- admin account add --name "dead-$FAILOVER_ROUTE" --provider anthropic \
  --secret "$FAILOVER_SECRET" --route "$FAILOVER_ROUTE" >/dev/null
cargo run --quiet -p oag -- admin account add --name "live-$FAILOVER_ROUTE" --provider anthropic \
  --secret "$LIVE_SECRET" --route "$FAILOVER_ROUTE" >/dev/null

OAG_GATEWAY__PROVIDER_BASE_URLS__ANTHROPIC="http://127.0.0.1:$MOCK_PORT" \
  just serve >"$WORK/gateway.log" 2>&1 &
GW_PID=$!
for _ in $(seq 1 180); do
  grep -q '^  inference  http://' "$WORK/gateway.log" 2>/dev/null && break
  sleep 0.5
done
PUBLIC="$(sed -n 's|^  inference  http://||p' "$WORK/gateway.log" | tail -1)"
ADMIN="$(sed -n 's|^  dashboard  http://||p' "$WORK/gateway.log" | tail -1)"
[ -n "$PUBLIC" ] && [ -n "$ADMIN" ] || fail "gateway never printed its ports; see $WORK/gateway.log"
for _ in $(seq 1 90); do curl -fsS "http://$ADMIN/health/ready" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "http://$ADMIN/health/ready" >/dev/null || fail "gateway never became ready; see $WORK/gateway.log"
pass "gateway on $PUBLIC  route $ROUTE"

post() {
  curl -sS --max-time 30 -o "$WORK/body.json" -w "%{http_code}" \
    -X POST "http://$PUBLIC/v1/messages" \
    -H "x-api-key: $KEY" -H 'content-type: application/json' \
    -d '{"model":"oag/auto","max_tokens":32,"stream":false,
         "messages":[{"role":"user","content":"hello"}]}'
}

say "4/5  the breaker trips mid-retry; the next request never reaches the mock"
code1="$(post)"
[ "$code1" = "408" ] || fail "first request: expected 408, got $code1 $(cat "$WORK/body.json")"
pass "first request 408 (seen=$(seen))"

code2="$(post)"
[ "$code2" = "408" ] || fail "second request: expected 408, got $code2 $(cat "$WORK/body.json")"
after_two="$(seen)"
# Five, not six: the fifth failure opens the breaker and the retry loop asks it
# before sending again, so the second request stops one attempt short. Asserting
# six would be asserting that a tripped breaker still gets one more request.
[ "$after_two" = "5" ] \
  || fail "expected 5 mock POSTs after two requests (3, then 2 before the breaker opens), got $after_two"
pass "second request 408, mock saw 5 — the sixth attempt was stopped by the breaker"

code3="$(post)"
after_three="$(seen)"
[ "$after_three" = "5" ] || fail "breaker did not hold: mock saw $after_three POSTs, expected 5"
[ "$code3" = "503" ] || fail "third request: expected 503 NoCredential, got $code3 $(cat "$WORK/body.json")"
grep -q 'no_credential' "$WORK/body.json" \
  || fail "third request was 503 but not no_credential: $(cat "$WORK/body.json")"
pass "third request 503 no_credential, mock still at 5"

say "5/5  a 529 on one credential is served by the other"
before="$(seen)"
# Read to clear: everything the breaker stages showed the mock is discarded, so
# what the count means afterwards is "credentials this one request used".
credentials >/dev/null
code4="$(curl -sS --max-time 30 -o "$WORK/failover.json" -w "%{http_code}" \
  -X POST "http://$PUBLIC/v1/messages" \
  -H "x-api-key: $FAILOVER_KEY" -H 'content-type: application/json' \
  -d '{"model":"oag/auto","max_tokens":32,"stream":false,
       "messages":[{"role":"user","content":"hello"}]}')"
[ "$code4" = "200" ] \
  || fail "failover: expected 200 from the second credential, got $code4 $(cat "$WORK/failover.json")"
grep -q 'mock response' "$WORK/failover.json" \
  || fail "failover returned 200 with no answer in it: $(cat "$WORK/failover.json")"

# The 200 alone does not prove a failover happened — it would also be the answer
# if the gateway had picked the live account first and never touched the dead
# one. Two distinct credentials at the mock is the part that cannot happen
# without one.
after_creds="$(credentials)"
[ "$after_creds" = "2" ] \
  || fail "the mock saw $after_creds distinct credential(s) for this request, expected 2:
  one request has to reach the failing account and then the live one. One means it was
  served without the failing account ever being tried, which proves nothing."
[ "$(seen)" -gt "$before" ] || fail "the failover request never reached the mock"
pass "529 on one credential, served by the other (2 credentials for one request)"

OK=1
printf '\n\033[32mPASS: the breaker trips, and a dead credential fails over to a live one\033[0m\n'
