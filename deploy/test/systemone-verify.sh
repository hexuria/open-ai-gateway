#!/usr/bin/env bash
# System One end to end, against a mock Jev, with no TypeSafe key.
#
# The Rust suite drives the same route with the real SDK against an in-process
# mock. This drives the built binary, configured the way an operator configures
# it — `account add --provider jev`, `provider_base_urls.jev` — and reads the
# ledger with psql rather than trusting the code that wrote it. In order:
#
#   1. a route with no Jev key refuses plainly: 503 `system_one_not_configured`,
#      and nothing reaches Jev
#   2. an answer comes back byte-identical to what Jev sent, with Jev's
#      `x-typesafe-request-id` and the gateway's `x-oag-*` beside it
#   3. the ledger has the row: `jev/jev-latest`, the tokens Jev reported, the
#      key that served
#   4. `/jev/v1/models` passes Jev's listing through
#   5. a 500 on one Jev key is served by the other: two keys at the mock for
#      one request, and the failing one benched
#   6. a chat request naming Jev's model is refused and never reaches Jev
#   7. `deploy/test/api/systemone.hurl` passes against this gateway
#
# Needs Python 3 and psql. hurl for stage 7, which says so when it is missing.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

MOCK_PORT="${MOCK_PORT:-8097}"
WORK="$(mktemp -d)"
OK=0
ROUTE="systemone-$RANDOM"
FAILOVER_ROUTE="systemone-failover-$RANDOM"
BARE_ROUTE="systemone-bare-$RANDOM"
# Fixtures, not credentials: the mock matches on the literal string and there is
# no Jev on the other end of any of them.
LIVE_SECRET="FAKE-JEV-KEY-FOR-TESTS"
DEAD_SECRET="FAKE-JEV-KEY-ONE-OF-TWO"
SPARE_SECRET="FAKE-JEV-KEY-TWO-OF-TWO"

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32m  ok  %s\033[0m\n' "$*"; }

seen() { curl -fsS "http://127.0.0.1:$MOCK_PORT/_seen" 2>/dev/null || echo "?"; }
# Distinct keys the mock has been shown since the last call; reading clears it.
credentials() {
  curl -fsS "http://127.0.0.1:$MOCK_PORT/_credentials?reset=1" 2>/dev/null | grep -c . || true
}

cleanup() {
  local code=$?
  kill ${MOCK_PID:-0} ${GW_PID:-0} 2>/dev/null || true
  # `just serve` runs the gateway two processes down, and killing `just` leaves
  # it listening. The port was free when `serve` chose it, so whatever listens
  # there now is the gateway this run started.
  if [ -n "${PUBLIC:-}" ]; then
    lsof -ti "tcp:${PUBLIC##*:}" -sTCP:LISTEN 2>/dev/null | xargs kill 2>/dev/null || true
  fi
  [ "$OK" = "1" ] && rm -rf "$WORK" || echo "logs kept in $WORK"
  exit $code
}
trap cleanup EXIT

command -v psql >/dev/null || fail "psql is required to read the ledger"

say "1/8  infrastructure"
if [ -z "${OAG_DATABASE__URL:-}" ]; then
  just dev-up >/dev/null
fi
eval "$(just _verify-env)"
just migrate >/dev/null
pass "postgres, redis, schema"

say "2/8  mock Jev"
MOCK_FAIL_FOR_KEY="$DEAD_SECRET=500" PORT="$MOCK_PORT" \
  python3 deploy/test/mock-systemone.py >"$WORK/mock.log" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 20); do
  [ "$(seen)" = "0" ] && break
  sleep 0.5
done
[ "$(seen)" = "0" ] || fail "mock Jev never became ready on :$MOCK_PORT; see $WORK/mock.log"
pass "mock Jev on :$MOCK_PORT"

say "3/8  three routes: one Jev key, two Jev keys, none"
key_for() {
  cargo run --quiet -p oag -- admin init --email "$1@localhost" --route "$2" 2>/dev/null \
    | grep -oE 'oag_live_[0-9a-f]+' | head -1
}
KEY="$(key_for systemone "$ROUTE")"
FAILOVER_KEY="$(key_for systemone-failover "$FAILOVER_ROUTE")"
BARE_KEY="$(key_for systemone-bare "$BARE_ROUTE")"
[ -n "$KEY" ] && [ -n "$FAILOVER_KEY" ] && [ -n "$BARE_KEY" ] || fail "init produced no key"
cargo run --quiet -p oag -- admin catalog seed >/dev/null
cargo run --quiet -p oag -- admin account add --name "jev-$ROUTE" --provider jev \
  --secret "$LIVE_SECRET" --route "$ROUTE" >/dev/null
# Priority decides which of two keys is tried first — lower wins outright — so
# the failing one is reached first on every run, rather than on the runs where
# the scheduler happened to pick it.
cargo run --quiet -p oag -- admin account add --name "dead-$FAILOVER_ROUTE" --provider jev \
  --secret "$DEAD_SECRET" --route "$FAILOVER_ROUTE" --priority 0 >/dev/null
cargo run --quiet -p oag -- admin account add --name "spare-$FAILOVER_ROUTE" --provider jev \
  --secret "$SPARE_SECRET" --route "$FAILOVER_ROUTE" --priority 1 >/dev/null

OAG_GATEWAY__PROVIDER_BASE_URLS__JEV="http://127.0.0.1:$MOCK_PORT" \
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
pass "gateway on $PUBLIC"

QUESTIONS='{"state":{"document":"I was charged twice. Please fix this ASAP."},"model":"jev-latest","questions":{"billing":{"type":"noul","instructions":"Is this ticket about billing?"},"tone":{"type":"choice","instructions":"What is the customer'"'"'s tone?","criteria":{"calm":null,"frustrated":null,"angry":null}},"urgency":{"type":"score","instructions":"How urgent is this ticket?","criteria":["can wait","this week","today"]}}}'

ask() { # key, output prefix
  curl -sS --max-time 30 -D "$WORK/$2.headers" -o "$WORK/$2.body" -w "%{http_code}" \
    -X POST "http://$PUBLIC/jev/v1/systemone" \
    -H "authorization: Bearer $1" -H 'content-type: application/json' \
    --data-binary "$QUESTIONS"
}
header() { # file, name
  grep -i "^$2:" "$WORK/$1.headers" | head -1 | cut -d: -f2- | tr -d ' \r'
}

say "4/8  a route with no Jev key refuses, plainly"
before="$(seen)"
code="$(ask "$BARE_KEY" bare)"
[ "$code" = "503" ] || fail "no Jev key: expected 503, got $code $(cat "$WORK/bare.body")"
grep -q '"system_one_not_configured"' "$WORK/bare.body" \
  || fail "503 but not system_one_not_configured: $(cat "$WORK/bare.body")"
grep -q 'System One is not configured on this route' "$WORK/bare.body" \
  || fail "the refusal does not say what is missing: $(cat "$WORK/bare.body")"
[ "$(seen)" = "$before" ] || fail "a refused request reached Jev"
pass "503 system_one_not_configured, nothing sent to Jev"

say "5/8  an answer, byte for byte, and its ledger row"
since="$(psql "$OAG_DATABASE__URL" -At -c "SELECT now()")"
code="$(ask "$KEY" answer)"
[ "$code" = "200" ] || fail "answer: expected 200, got $code $(cat "$WORK/answer.body")"
curl -fsS "http://127.0.0.1:$MOCK_PORT/_last" -o "$WORK/jev.body"
cmp -s "$WORK/answer.body" "$WORK/jev.body" \
  || fail "the answer is not Jev's bytes: $(diff <(xxd "$WORK/jev.body") <(xxd "$WORK/answer.body") | head -5)"
[ -n "$(header answer x-typesafe-request-id)" ] || fail "Jev's request id was not passed through"
[ "$(header answer x-oag-model)" = "jev/jev-latest" ] \
  || fail "x-oag-model is '$(header answer x-oag-model)', expected jev/jev-latest"
[ -n "$(header answer x-oag-request-id)" ] || fail "no x-oag-request-id"
pass "200, byte-identical to Jev's answer, request ids on both sides"

row=""
for _ in $(seq 1 40); do
  row="$(psql "$OAG_DATABASE__URL" -At -F'|' -c "SELECT u.model_id, u.input_tokens, u.output_tokens, \
      a.name, u.status FROM usage_event u JOIN account a ON a.id = u.account_id \
      WHERE u.request_id = '$(header answer x-oag-request-id)' AND u.occurred_at >= '$since'")"
  [ -n "$row" ] && break
  sleep 0.25
done
[ "$row" = "jev/jev-latest|42|3|jev-$ROUTE|200" ] \
  || fail "ledger row is '$row', expected 'jev/jev-latest|42|3|jev-$ROUTE|200'"
pass "ledger: $row"

say "6/8  Jev's model listing, passed through"
code="$(curl -sS --max-time 30 -D "$WORK/models.headers" -o "$WORK/models.body" -w "%{http_code}" \
  "http://$PUBLIC/jev/v1/models" -H "authorization: Bearer $KEY")"
[ "$code" = "200" ] || fail "models: expected 200, got $code $(cat "$WORK/models.body")"
grep -q '"jev-latest"' "$WORK/models.body" || fail "no jev-latest in $(cat "$WORK/models.body")"
[ "$(header models x-typesafe-request-id)" = "models-1" ] || fail "the listing lost Jev's request id"
pass "200, Jev's listing"

say "7/8  a 500 on one Jev key is served by the other"
credentials >/dev/null
code="$(ask "$FAILOVER_KEY" failover)"
[ "$code" = "200" ] || fail "failover: expected 200, got $code $(cat "$WORK/failover.body")"
curl -fsS "http://127.0.0.1:$MOCK_PORT/_last" -o "$WORK/jev.body"
cmp -s "$WORK/failover.body" "$WORK/jev.body" || fail "the failover answer is not Jev's bytes"
used="$(credentials)"
[ "$used" = "2" ] \
  || fail "the mock saw $used distinct key(s) for this request, expected 2: the failing key
  first, then the spare. One means the failing key was never tried, which proves nothing."
benched="$(psql "$OAG_DATABASE__URL" -At -c "SELECT cooldown_until > now() FROM account \
  WHERE name = 'dead-$FAILOVER_ROUTE'")"
[ "$benched" = "t" ] || fail "the failing key was not benched (cooldown_until > now() is '$benched')"
pass "500 on one key, answered by the other; the failing key sits out"

say "8/8  chat never reaches Jev; the hurl file passes"
before="$(seen)"
code="$(curl -sS --max-time 30 -o "$WORK/chat.body" -w "%{http_code}" \
  -X POST "http://$PUBLIC/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"jev/jev-latest","messages":[{"role":"user","content":"hi"}]}')"
[ "$code" = "400" ] || fail "chat naming jev/jev-latest: expected 400, got $code $(cat "$WORK/chat.body")"
grep -q '/jev/v1/systemone' "$WORK/chat.body" \
  || fail "the refusal does not say where System One is served: $(cat "$WORK/chat.body")"
[ "$(seen)" = "$before" ] || fail "a chat request reached Jev"
pass "400 no_viable_model, pointing at /jev/v1/systemone; Jev untouched"

if command -v hurl >/dev/null; then
  hurl --test --variable "host=http://$PUBLIC" --variable "api_key=$KEY" \
    deploy/test/api/systemone.hurl >"$WORK/hurl.log" 2>&1 \
    || { cat "$WORK/hurl.log"; fail "deploy/test/api/systemone.hurl did not pass"; }
  pass "deploy/test/api/systemone.hurl"
else
  printf '\033[33m  skipped  deploy/test/api/systemone.hurl: hurl is not installed\033[0m\n'
fi

OK=1
printf '\n\033[32mPASS: System One answers through the gateway, byte for byte, metered and failed over\033[0m\n'
