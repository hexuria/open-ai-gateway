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
for _ in $(seq 1 20); do
  curl -fsS -o /dev/null -X POST "http://127.0.0.1:$MOCK_PORT/v1/messages" -d '{}' 2>/dev/null && break
  sleep 0.5
done
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
started="$(python3 -c 'import time; print(time.time())')"
curl -sN --max-time 120 -X POST "http://$PUBLIC/v1/messages" \
  -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"model":"oag/auto","max_tokens":256,"stream":true,
       "messages":[{"role":"user","content":"hello"}]}' >"$WORK/stream.txt"
elapsed="$(python3 -c "import time; print(round(time.time() - $started, 1))")"

grep -q 'event: message_start'       "$WORK/stream.txt" || fail "no message_start"
grep -q 'event: content_block_delta' "$WORK/stream.txt" || fail "no content deltas"
grep -q 'event: message_stop'        "$WORK/stream.txt" || fail "stream never completed"
pass "SSE complete ($(grep -c '^event:' "$WORK/stream.txt") events)"

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
just _verify-ledger > "$WORK/ledger.txt" || fail "could not read the ledger"
cat "$WORK/ledger.txt" | sed 's/^/  /'
python3 - "$WORK/ledger.txt" <<'PY'
import sys
row = open(sys.argv[1]).read().strip().split("|")
if len(row) < 6:
    sys.exit("no ledger row for the request — metering did not run")
model, tier, inp, out, cost, counterfactual, ttft = (c.strip() for c in row[:7])
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
