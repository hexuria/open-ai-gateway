#!/usr/bin/env bash
# Native OpenAI and Gemini adapters, end to end, with no provider keys.
#
# `just verify` only ever talks Anthropic. These two adapters have `build()`
# unit tests and have never parsed a real stream. aimock speaks both dialects
# on one port; dummy secrets satisfy the SDKs.
#
# Needs Node (`npx`). Does not replace the Python mock used by `just verify`
# and `just verify-k8s`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

AIMOCK_PORT="${AIMOCK_PORT:-4010}"
AIMOCK_VERSION="${AIMOCK_VERSION:-1.39.0}"
WORK="$(mktemp -d)"
OK=0
ROUTE="dialects-$RANDOM"

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32m  ok  %s\033[0m\n' "$*"; }

cleanup() {
  local code=$?
  kill ${AIMOCK_PID:-0} ${GW_PID:-0} 2>/dev/null || true
  [ "$OK" = "1" ] && rm -rf "$WORK" || echo "logs kept in $WORK"
  exit $code
}
trap cleanup EXIT

command -v npx >/dev/null || fail "npx is required (install Node) to run dialect verification"

say "1/5  infrastructure"
if [ -z "${OAG_DATABASE__URL:-}" ]; then
  just dev-up >/dev/null
fi
eval "$(just _verify-env)"
just migrate >/dev/null
pass "postgres, redis, schema"

say "2/5  aimock @$AIMOCK_VERSION"
# Fetch the package BEFORE the readiness clock starts.
#
# `npx --yes` downloads on first use, and every CI runner is a cold cache. A
# cold fetch of this package was measured at 33s against a readiness budget of
# 30s — so the wait was timing npm's throughput rather than whether the server
# came up, and the job failed or passed on how busy the registry was that
# minute. It had been winning that race until it did not.
#
# Pulling the download out front makes the timeout below mean what it says.
# Failure here is not fatal on its own: if the fetch genuinely cannot happen,
# the readiness check reports it with the log, which is a better message than a
# bare non-zero from a prefetch.
npx --yes --package "@copilotkit/aimock@$AIMOCK_VERSION" llmock --help \
  >"$WORK/aimock-fetch.log" 2>&1 || true

npx --yes --package "@copilotkit/aimock@$AIMOCK_VERSION" llmock \
  -p "$AIMOCK_PORT" \
  -f "$REPO_ROOT/deploy/test/aimock/fixtures.json" \
  -h 127.0.0.1 \
  >"$WORK/aimock.log" 2>&1 &
AIMOCK_PID=$!
# 60s. Generous on purpose: the download is already done by here, so this is
# only process start, and a budget that is merely adequate is one that fails on
# a slow runner for no reason anybody can act on.
for _ in $(seq 1 120); do
  curl -fsS "http://127.0.0.1:$AIMOCK_PORT/health" >/dev/null 2>&1 && break
  sleep 0.5
done
if ! curl -fsS "http://127.0.0.1:$AIMOCK_PORT/health" >/dev/null; then
  # Print the log rather than pointing at it. "see $WORK/aimock.log" is fine on
  # a laptop and useless in CI, where the file is on a runner that no longer
  # exists by the time anyone reads the failure — which is exactly when you most
  # need to know whether npx could not fetch the package, the port was taken, or
  # the fixtures would not parse.
  printf '\n\033[31m-- aimock.log --\033[0m\n'
  cat "$WORK/aimock.log" 2>/dev/null || echo "(no log was written at all)"
  printf '\033[31m-- end --\033[0m\n'
  fail "aimock never became ready on :$AIMOCK_PORT"
fi
pass "aimock on :$AIMOCK_PORT"

say "3/5  catalog, accounts, gateway"
KEY="$(
  cargo run --quiet -p oag -- admin init --email dialects@localhost --route "$ROUTE" 2>/dev/null \
    | grep -oE 'oag_live_[0-9a-f]+' | head -1
)"
[ -n "$KEY" ] || fail "init produced no key"
cargo run --quiet -p oag -- admin catalog seed >/dev/null
cargo run --quiet -p oag -- admin catalog seed --from "$REPO_ROOT/deploy/test/aimock/catalog.json" >/dev/null
cargo run --quiet -p oag -- admin account add --name "openai-$ROUTE" --provider openai \
  --secret sk-mock --route "$ROUTE" >/dev/null
cargo run --quiet -p oag -- admin account add --name "gemini-$ROUTE" --provider gemini \
  --secret mock --route "$ROUTE" >/dev/null

OAG_GATEWAY__PROVIDER_BASE_URLS__OPENAI="http://127.0.0.1:$AIMOCK_PORT/v1" \
OAG_GATEWAY__PROVIDER_BASE_URLS__GEMINI="http://127.0.0.1:$AIMOCK_PORT/v1beta" \
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

# The ledger row is written by a task detached from the response, so the reply
# can reach this script before the row exists — and did, on a slow CI runner.
# Every read therefore waits for a row newer than a mark taken just before the
# request, instead of reading whichever row is newest: that also means the
# second request's check cannot pass on the first request's row.
mark() { psql "$OAG_DATABASE__URL" -At -c "SELECT now()"; }
# Abandoned and lost attempts are excluded: since 0014 contracted the ledger key
# onto `(request_id, attempt)` they land beside the served row, and by design
# they carry a counterfactual of zero — which the assertion below reads as "the
# savings figure is wrong". The row these checks are about is the one the client
# was served.
ledger() {
  local like="$1" since="$2" row=""
  for _ in $(seq 1 40); do
    row="$(psql "$OAG_DATABASE__URL" -At -F'|' -c "SELECT model_id, tier, input_tokens, output_tokens, \
        cost_usd, counterfactual_usd \
        FROM usage_event WHERE model_id LIKE '$like' AND occurred_at >= '$since' \
          AND selection_reason NOT IN ('abandoned', 'lost') \
        ORDER BY occurred_at DESC LIMIT 1")" || return 1
    [ -n "$row" ] && break
    sleep 0.25
  done
  printf '%s\n' "$row"
}

assert_ledger() {
  local like="$1" since="$2"
  local row
  row="$(ledger "$like" "$since")" || fail "could not read the ledger for $like"
  python3 - "$row" <<'PY'
import sys
row = sys.argv[1].strip().split("|")
if len(row) < 6:
    sys.exit("no ledger row — metering did not run")
model, tier, inp, out, cost, counterfactual = (c.strip() for c in row[:6])
if int(inp) == 0 or int(out) == 0:
    sys.exit(f"usage was not recorded: in={inp} out={out} for {model}")
if float(counterfactual) <= float(cost):
    sys.exit(f"counterfactual {counterfactual} is not above actual {cost}")
print(f"  ok  {model} on '{tier}': in={inp} out={out}, ${cost} vs ${counterfactual} frontier")
PY
}

say "4/5  OpenAI Chat Completions"
since="$(mark)"
curl -sS --max-time 30 -o "$WORK/openai.json" -w "%{http_code}" \
  -X POST "http://$PUBLIC/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"hello"}],"stream":false}' \
  >"$WORK/openai.status"
[ "$(cat "$WORK/openai.status")" = "200" ] \
  || fail "openai non-stream: HTTP $(cat "$WORK/openai.status") $(cat "$WORK/openai.json")"
grep -q 'dialect mock' "$WORK/openai.json" || fail "openai body had no fixture text: $(cat "$WORK/openai.json")"
assert_ledger 'openai%' "$since"
pass "openai non-stream + ledger"

curl -sN --max-time 30 -X POST "http://$PUBLIC/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"hello"}],"stream":true}' \
  >"$WORK/openai.sse"
grep -q '^data: ' "$WORK/openai.sse" || fail "openai stream had no data frames"
grep -q 'dialect mock\|\[DONE\]' "$WORK/openai.sse" \
  || fail "openai stream did not complete: $(head -c 400 "$WORK/openai.sse")"
pass "openai stream"

say "5/5  Gemini generateContent"
since="$(mark)"
curl -sS --max-time 30 -o "$WORK/gemini.json" -w "%{http_code}" \
  -X POST "http://$PUBLIC/v1beta/models/gemini-2.0-flash:generateContent" \
  -H "x-goog-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}' \
  >"$WORK/gemini.status"
[ "$(cat "$WORK/gemini.status")" = "200" ] \
  || fail "gemini non-stream: HTTP $(cat "$WORK/gemini.status") $(cat "$WORK/gemini.json")"
grep -q 'dialect mock' "$WORK/gemini.json" || fail "gemini body had no fixture text: $(cat "$WORK/gemini.json")"
assert_ledger 'gemini%' "$since"
pass "gemini non-stream + ledger"

curl -sN --max-time 30 -X POST "http://$PUBLIC/v1beta/models/gemini-2.0-flash:streamGenerateContent" \
  -H "x-goog-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}' \
  >"$WORK/gemini.sse"
grep -q '^data: ' "$WORK/gemini.sse" || fail "gemini stream had no data frames"
python3 - "$WORK/gemini.sse" <<'PY' || fail "gemini stream text did not reassemble"
import json, sys
text = []
for line in open(sys.argv[1]):
    if not line.startswith("data: "):
        continue
    payload = line[6:].strip()
    if payload in ("", "[DONE]"):
        continue
    body = json.loads(payload)
    for c in body.get("candidates", []):
        for p in c.get("content", {}).get("parts", []):
            if isinstance(p.get("text"), str):
                text.append(p["text"])
joined = "".join(text)
if joined != "dialect mock":
    sys.exit(f"reassembled {joined!r}, expected 'dialect mock'")
PY
pass "gemini stream"

OK=1
printf '\n\033[32mPASS: OpenAI and Gemini adapters work against aimock\033[0m\n'
