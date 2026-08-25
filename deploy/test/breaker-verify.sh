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
# (default 3) failures, two inbound requests trip the threshold of 5, and the
# third is refused locally.
#
# Needs no credentials. The mock is the Python one, not aimock: the count of
# POSTs is the assertion, and MOCK_FAIL_STATUS is deterministic.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

MOCK_PORT="${MOCK_PORT:-8098}"
WORK="$(mktemp -d)"
OK=0
ROUTE="breaker-$RANDOM"

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32m  ok  %s\033[0m\n' "$*"; }

seen() {
  curl -fsS "http://127.0.0.1:$MOCK_PORT/_seen" 2>/dev/null || echo "?"
}

cleanup() {
  local code=$?
  kill ${MOCK_PID:-0} ${GW_PID:-0} 2>/dev/null || true
  [ "$OK" = "1" ] && rm -rf "$WORK" || echo "logs kept in $WORK"
  exit $code
}
trap cleanup EXIT

say "1/4  infrastructure"
if [ -z "${OAG_DATABASE__URL:-}" ]; then
  just dev-up >/dev/null
fi
eval "$(just _verify-env)"
just migrate >/dev/null
pass "postgres, redis, schema"

say "2/4  mock that always returns 408"
MOCK_FAIL_STATUS=408 PORT="$MOCK_PORT" \
  python3 deploy/test/mock-upstream.py >"$WORK/mock.log" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 20); do
  [ "$(seen)" = "0" ] && break
  sleep 0.5
done
[ "$(seen)" = "0" ] || fail "mock never became ready; see $WORK/mock.log"
pass "mock on :$MOCK_PORT (seen=$(seen))"

say "3/4  gateway, one anthropic account on an isolated route"
KEY="$(
  cargo run --quiet -p oag -- admin init --email breaker@localhost --route "$ROUTE" 2>/dev/null \
    | grep -oE 'oag_live_[0-9a-f]+' | head -1
)"
[ -n "$KEY" ] || fail "init produced no key"
cargo run --quiet -p oag -- admin catalog seed >/dev/null
cargo run --quiet -p oag -- admin account add --name "breaker-$ROUTE" --provider anthropic \
  --secret FAKE-CREDENTIAL-FOR-TESTS --route "$ROUTE" >/dev/null

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

say "4/4  two failures trip it; the third never reaches the mock"
code1="$(post)"
[ "$code1" = "408" ] || fail "first request: expected 408, got $code1 $(cat "$WORK/body.json")"
pass "first request 408 (seen=$(seen))"

code2="$(post)"
[ "$code2" = "408" ] || fail "second request: expected 408, got $code2 $(cat "$WORK/body.json")"
after_two="$(seen)"
[ "$after_two" = "6" ] || fail "expected 6 mock POSTs after two requests (2×3 attempts), got $after_two"
pass "second request 408, mock saw 6"

code3="$(post)"
after_three="$(seen)"
[ "$after_three" = "6" ] || fail "breaker did not hold: mock saw $after_three POSTs, expected 6"
[ "$code3" = "503" ] || fail "third request: expected 503 NoCredential, got $code3 $(cat "$WORK/body.json")"
grep -q 'no_credential' "$WORK/body.json" \
  || fail "third request was 503 but not no_credential: $(cat "$WORK/body.json")"
pass "third request 503 no_credential, mock still at 6"

OK=1
printf '\n\033[32mPASS: the breaker trips and the next request is refused locally\033[0m\n'
