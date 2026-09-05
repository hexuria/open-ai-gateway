#!/usr/bin/env bash
# OpenAI Chat Completions client → Anthropic upstream, through the hub.
#
# `just verify` talks Anthropic on both sides. `just verify-dialects` talks
# OpenAI to an OpenAI mock. Neither exercises the translation the hub exists
# for: a Chat Completions client routed onto the cheap Anthropic account.
# The Python mock is Anthropic-only, so a 200 that looks like OpenAI can only
# have come from the codecs.
#
# Needs no Node, no Docker, no credentials. Same mock as `just verify`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

MOCK_PORT="${MOCK_PORT:-8097}"
STREAM_SECONDS="${STREAM_SECONDS:-2}"
CHUNKS="${CHUNKS:-4}"
WORK="$(mktemp -d)"
OK=0
ROUTE="translate-$RANDOM"

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

say "1/5  infrastructure"
if [ -z "${OAG_DATABASE__URL:-}" ]; then
  just dev-up >/dev/null
fi
eval "$(just _verify-env)"
just migrate >/dev/null
pass "postgres, redis, schema"

say "2/5  Anthropic mock"
MOCK_STREAM_SECONDS="$STREAM_SECONDS" MOCK_CHUNKS="$CHUNKS" PORT="$MOCK_PORT" \
  python3 deploy/test/mock-upstream.py >"$WORK/mock.log" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 20); do
  [ "$(seen)" = "0" ] && break
  sleep 0.5
done
[ "$(seen)" = "0" ] || fail "mock never became ready; see $WORK/mock.log"
pass "mock on :$MOCK_PORT"

say "3/5  gateway, one anthropic account on an isolated route"
KEY="$(
  cargo run --quiet -p oag -- admin init --email translate@localhost --route "$ROUTE" 2>/dev/null \
    | grep -oE 'oag_live_[0-9a-f]+' | head -1
)"
[ -n "$KEY" ] || fail "init produced no key"
cargo run --quiet -p oag -- admin catalog seed >/dev/null
cargo run --quiet -p oag -- admin account add --name "translate-$ROUTE" --provider anthropic \
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
  local since="$1" row=""
  for _ in $(seq 1 40); do
    row="$(psql "$OAG_DATABASE__URL" -At -F'|' -c "SELECT model_id, tier, input_tokens, output_tokens, \
        cost_usd, counterfactual_usd \
        FROM usage_event WHERE model_id LIKE 'anthropic%' AND occurred_at >= '$since' \
          AND selection_reason NOT IN ('abandoned', 'lost') \
        ORDER BY occurred_at DESC LIMIT 1")" || return 1
    [ -n "$row" ] && break
    sleep 0.25
  done
  printf '%s\n' "$row"
}

assert_ledger() {
  local want_out="$1" since="$2"
  local row
  row="$(ledger "$since")" || fail "could not read the ledger"
  python3 - "$row" "$want_out" <<'PY'
import sys
row = sys.argv[1].strip().split("|")
want_out = int(sys.argv[2])
if len(row) < 6:
    sys.exit("no ledger row — metering did not run")
model, tier, inp, out, cost, counterfactual = (c.strip() for c in row[:6])
if not model.startswith("anthropic/"):
    sys.exit(f"expected an anthropic model, got {model}")
if int(inp) != 100:
    sys.exit(f"usage was not merged: in={inp} out={out} for {model}")
if int(out) != want_out:
    sys.exit(f"output_tokens={out}, expected {want_out} for {model}")
if float(counterfactual) <= float(cost):
    sys.exit(f"counterfactual {counterfactual} is not above actual {cost}")
print(f"  ok  {model} on '{tier}': in={inp} out={out}, ${cost} vs ${counterfactual} frontier")
PY
}

say "4/5  Chat Completions client, Anthropic upstream"
since="$(mark)"
curl -sS --max-time 30 -o "$WORK/openai.json" -w "%{http_code}" \
  -X POST "http://$PUBLIC/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"oag/auto","messages":[{"role":"user","content":"hello"}],"stream":false}' \
  >"$WORK/openai.status"
[ "$(cat "$WORK/openai.status")" = "200" ] \
  || fail "non-stream: HTTP $(cat "$WORK/openai.status") $(cat "$WORK/openai.json")"
python3 - "$WORK/openai.json" <<'PY' || fail "non-stream body was not Chat Completions"
import json, sys
body = json.load(open(sys.argv[1]))
if body.get("type") == "message":
    sys.exit("got an Anthropic Messages body — translation did not run")
if body.get("object") != "chat.completion":
    sys.exit(f"object={body.get('object')!r}, expected chat.completion")
text = (body.get("choices") or [{}])[0].get("message", {}).get("content")
if text != "mock response":
    sys.exit(f"content={text!r}, expected 'mock response'")
usage = body.get("usage") or {}
if usage.get("prompt_tokens") != 100 or usage.get("completion_tokens") != 12:
    sys.exit(f"usage={usage}")
PY
grep -q 'POST /v1/messages' "$WORK/mock.log" \
  || fail "mock never saw /v1/messages — the adapter did not fire"
assert_ledger 12 "$since"
pass "non-stream translated + ledger"

since="$(mark)"
curl -sN --max-time 30 -X POST "http://$PUBLIC/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"oag/auto","messages":[{"role":"user","content":"hello"}],"stream":true}' \
  >"$WORK/openai.sse"
grep -q 'event: message_start' "$WORK/openai.sse" \
  && fail "stream was Anthropic SSE (passthrough), not Chat Completions"
grep -q 'data: \[DONE\]' "$WORK/openai.sse" || fail "stream never sent [DONE]: $(head -c 400 "$WORK/openai.sse")"
python3 - "$WORK/openai.sse" "$CHUNKS" <<'PY' || fail "stream did not reassemble"
import json, sys
want = int(sys.argv[2])
text = []
saw_chunk = False
finish = None
for line in open(sys.argv[1]):
    if not line.startswith("data: "):
        continue
    payload = line[6:].strip()
    if payload in ("", "[DONE]"):
        continue
    body = json.loads(payload)
    if body.get("object") != "chat.completion.chunk":
        sys.exit(f"object={body.get('object')!r}, expected chat.completion.chunk")
    saw_chunk = True
    for c in body.get("choices") or []:
        delta = c.get("delta") or {}
        if isinstance(delta.get("content"), str):
            text.append(delta["content"])
        if c.get("finish_reason"):
            finish = c["finish_reason"]
joined = "".join(text)
expected = "".join(f"chunk {i} " for i in range(want))
if not saw_chunk:
    sys.exit("no chat.completion.chunk frames")
if joined != expected:
    sys.exit(f"reassembled {joined!r}, expected {expected!r}")
if finish != "stop":
    sys.exit(f"finish_reason={finish!r}, expected stop")
PY
assert_ledger $((CHUNKS * 3)) "$since"
pass "stream translated + [DONE] + ledger"

# Two fields that are invisible in the reply: the gateway either renders them on
# the way out or it does not, and the client is told nothing either way. Both
# were dropped in silence until 2026-09-05 — a system message written as parts,
# and a reasoning level from a client whose dialect has no budget field.
say "5/5  fields the reply cannot show you"
before="$(wc -l <"$WORK/mock.log")"
curl -sS --max-time 30 -o "$WORK/fields.json" -w "%{http_code}" \
  -X POST "http://$PUBLIC/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"oag/auto","stream":false,"reasoning_effort":"high","messages":[
        {"role":"system","content":[{"type":"text","text":"Never write prose."}]},
        {"role":"user","content":"hello"}]}' \
  >"$WORK/fields.status"
[ "$(cat "$WORK/fields.status")" = "200" ] \
  || fail "HTTP $(cat "$WORK/fields.status") $(cat "$WORK/fields.json")"

tail -n "+$((before + 1))" "$WORK/mock.log" | grep '^mock-request: ' | tail -1 \
  | sed 's/^mock-request: //' >"$WORK/upstream.json"
[ -s "$WORK/upstream.json" ] || fail "the mock recorded no request; see $WORK/mock.log"

python3 - "$WORK/upstream.json" <<'FIELDS' || fail "a field was dropped on the way to the upstream"
import json, sys
req = json.load(open(sys.argv[1]))

# H1. The array form is legal here and used to be discarded, so the model was
# asked the bare question with no instructions at all.
system = req.get("system")
text = ""
if isinstance(system, str):
    text = system
elif isinstance(system, list):
    text = " ".join(b.get("text", "") for b in system if isinstance(b, dict))
if "Never write prose." not in text:
    sys.exit(f"the system message did not reach the upstream: system={system!r}")

# H3. This dialect speaks budgets, the client spoke levels, and the bridge only
# ever ran in the other direction.
thinking = req.get("thinking") or {}
if thinking.get("type") != "enabled" or not thinking.get("budget_tokens"):
    sys.exit(f"reasoning_effort did not become a thinking budget: thinking={thinking!r}")
print(f"  ok  system rendered, thinking budget {thinking['budget_tokens']}")
FIELDS
pass "system-as-parts and reasoning_effort both reached the upstream"

OK=1
printf '\n\033[32mPASS: OpenAI client over Anthropic upstream is translated, not passed through\033[0m\n'
