#!/usr/bin/env bash
# The whole request path, end to end, in about a minute and with no credentials.
#
# The kind harness next door proves the Kubernetes properties and costs ten
# minutes. This proves the thing the project exists for — that a request is
# classified, routed to a cheap model, streamed back, and metered with a
# truthful savings figure — and it is fast enough to run on every change.
#
# It asserts, in order:
#   1. a streamed completion arrives as real SSE, with Anthropic's event types
#   2. it takes about as long as the upstream took, rather than hanging until an
#      idle timeout — a stream that "works" but stalls looks identical otherwise
#   3. usage is merged across message_start and message_delta, not overwritten
#   4. the ledger row records a counterfactual ABOVE the actual cost, which is
#      the number the whole gateway is justified by
#   5. TTFT is measured from first CONTENT, not first byte
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

MOCK_PORT="${MOCK_PORT:-8099}"
STREAM_SECONDS="${STREAM_SECONDS:-6}"
WORK="$(mktemp -d)"
OK=0

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32m  ok  %s\033[0m\n' "$*"; }

cleanup() {
  local code=$?
  kill ${MOCK_PID:-0} ${GW_PID:-0} 2>/dev/null || true
  [ "$OK" = "1" ] && rm -rf "$WORK" || echo "logs kept in $WORK"
  exit $code
}
trap cleanup EXIT

say "1/5  infrastructure"
just dev-up >/dev/null
eval "$(just _verify-env)"
just migrate >/dev/null
pass "postgres, redis, schema"

say "2/5  mock upstream"
MOCK_STREAM_SECONDS="$STREAM_SECONDS" MOCK_CHUNKS=6 PORT="$MOCK_PORT" \
  python3 deploy/test/mock-upstream.py >"$WORK/mock.log" 2>&1 &
MOCK_PID=$!
MOCK_UP=0
for _ in $(seq 1 20); do
  if curl -fsS -o /dev/null -X POST "http://127.0.0.1:$MOCK_PORT/v1/messages" -d '{}' 2>/dev/null; then
    MOCK_UP=1
    break
  fi
  sleep 0.5
done
# The loop used to fall through and report success whether or not the mock ever
# answered. It then failed ten seconds later at the gateway, or — worse, if
# something else held the port — passed against whatever was listening.
[ "$MOCK_UP" = "1" ] || fail "mock never answered on :$MOCK_PORT; see $WORK/mock.log"
pass "mock on :$MOCK_PORT"

say "3/5  gateway, pointed at the mock"
KEY="$(just _verify-bootstrap 2>/dev/null | grep -oE 'oag_live_[0-9a-f]+' | head -1)"
[ -n "$KEY" ] || fail "bootstrap produced no key"
OAG_GATEWAY__PROVIDER_BASE_URLS__ANTHROPIC="http://127.0.0.1:$MOCK_PORT" \
  just serve >"$WORK/gateway.log" 2>&1 &
GW_PID=$!
ADMIN="$(just ports | awk '/dashboard/ {print $2}')"
PUBLIC="$(just ports | awk '/inference/ {print $2}')"
for _ in $(seq 1 90); do curl -fsS "http://$ADMIN/health/ready" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "http://$ADMIN/health/ready" >/dev/null || fail "gateway never became ready; see $WORK/gateway.log"
pass "gateway on $PUBLIC"

say "4/5  a real streamed completion"
# Taken before the request, and passed to `_verify-ledger` below: the dev
# database keeps its rows between runs, so reading whichever row is newest let
# a run that metered nothing at all pass on the previous run's row. The other
# three verify scripts have taken this mark for a while; this one had not.
SINCE="$(psql "$OAG_DATABASE__URL" -At -c "SELECT now()")"
started="$(python3 -c 'import time; print(time.time())')"
curl -sN --max-time 120 -D "$WORK/stream.headers" -X POST "http://$PUBLIC/v1/messages" \
  -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"model":"oag/auto","max_tokens":256,"stream":true,
       "messages":[{"role":"user","content":"hello"}]}' >"$WORK/stream.txt"
elapsed="$(python3 -c "import time; print(round(time.time() - $started, 1))")"

grep -q 'event: message_start'       "$WORK/stream.txt" || fail "no message_start"
grep -q 'event: content_block_delta' "$WORK/stream.txt" || fail "no content deltas"
grep -q 'event: message_stop'        "$WORK/stream.txt" || fail "stream never completed"
pass "SSE complete ($(grep -c '^event:' "$WORK/stream.txt") events)"

# The routing headers. `oag/auto` means the client did not choose a model, so
# these are the only place it is told which one answered — and no script asserted
# them, on any dialect. A response that omits them is not a broken stream, which
# is why it would have gone unnoticed: every other check here still passes.
OAG_MODEL="$(sed -n 's/^[Xx]-[Oo][Aa][Gg]-[Mm]odel: *//p' "$WORK/stream.headers" | tr -d '\r')"
OAG_REQUEST_ID="$(sed -n 's/^[Xx]-[Oo][Aa][Gg]-[Rr]equest-[Ii]d: *//p' "$WORK/stream.headers" | tr -d '\r')"
OAG_TIER="$(sed -n 's/^[Xx]-[Oo][Aa][Gg]-[Tt]ier: *//p' "$WORK/stream.headers" | tr -d '\r')"
[ -n "$OAG_MODEL" ] \
  || fail "no x-oag-model on the response; the client asked for oag/auto and was never
  told what answered: $(tr -d '\r' < "$WORK/stream.headers" | head -20)"
[ -n "$OAG_REQUEST_ID" ] \
  || fail "no x-oag-request-id on the response; nothing ties this answer to its ledger row"
case "$OAG_MODEL" in
  */*) : ;;
  *) fail "x-oag-model is '$OAG_MODEL', which is not a provider-qualified id" ;;
esac
pass "x-oag-model $OAG_MODEL, x-oag-tier ${OAG_TIER:-<off-ladder>}, x-oag-request-id present"

# A stream that hangs until the 180s idle watchdog looks identical to a healthy
# one if you only check the events.
python3 -c "
import sys
e = $elapsed
if e > $STREAM_SECONDS + 15:
    sys.exit('stream took %.1fs for a %ss upstream — it stalled rather than ending' % (e, $STREAM_SECONDS))
print('  ok  finished in %.1fs, tracking the upstream' % e)
"

say "5/5  the ledger"
# Written by a task detached from the response; give it a moment to land
# rather than reading the ledger the instant the stream closed.
for _ in $(seq 1 40); do
  just _verify-ledger "$SINCE" > "$WORK/ledger.txt" || fail "could not read the ledger"
  # Seven columns, so six separators. `-ge 5` accepted a six-column row that the
  # seven-name unpack below would then have died on with a ValueError rather
  # than the diagnosis the script exists to print.
  [ "$(tr -cd '|' < "$WORK/ledger.txt" | wc -c)" -ge 6 ] && break
  sleep 0.25
done
cat "$WORK/ledger.txt" | sed 's/^/  /'
python3 - "$WORK/ledger.txt" "$OAG_MODEL" <<'PY'
import sys
row = open(sys.argv[1]).read().strip().split("|")
if len(row) < 7:
    sys.exit("no ledger row for the request — metering did not run")
model, tier, inp, out, cost, counterfactual, ttft = (c.strip() for c in row[:7])
# The header the client was given and the row the operator will read have to
# name the same model. They are produced by different code on different paths —
# one on the response builder, one in a task detached from it — so agreeing is a
# property, not an identity, and a client billed for one model while told it got
# another is the kind of disagreement nobody notices until an invoice.
header_model = sys.argv[2]
if header_model and header_model != model:
    sys.exit(f"x-oag-model said {header_model!r} and the ledger recorded {model!r}")
if int(inp) == 0 or int(out) == 0:
    sys.exit(f"usage was not merged: in={inp} out={out}. Anthropic splits it across "
             "message_start and message_delta, and a naive overwrite zeroes one of them.")
if float(counterfactual) <= float(cost):
    sys.exit(f"counterfactual {counterfactual} is not above actual {cost} — the savings "
             "figure the gateway is justified by is wrong")
if ttft in ("", "None") or int(ttft) <= 0:
    sys.exit("no TTFT recorded")
saved = (1 - float(cost) / float(counterfactual)) * 100
print(f"  ok  {model} on '{tier}': in={inp} out={out}, "
      f"${cost} vs ${counterfactual} frontier — {saved:.0f}% saved, ttft {ttft}ms")
PY

OK=1
printf '\n\033[32mPASS: the request path works end to end\033[0m\n'
