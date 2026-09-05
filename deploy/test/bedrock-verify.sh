#!/usr/bin/env bash
# Bedrock adapter against VidaiMock's event-stream encoder.
#
# The bundled VidaiMock Bedrock path is Converse-shaped and streamed empty
# `{"bytes":""}` frames. `deploy/test/vidaimock/` overlays Anthropic event JSON
# into encode_chunk, which wraps `{"bytes":"<base64>"}` with real CRC32 — an
# independent encoder our decoder has never seen.
#
# Needs Docker. Dummy IAM pair (`AKIATEST:…`); VidaiMock does not check SigV4.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

VIDAIMOCK_PORT="${VIDAIMOCK_PORT:-8100}"
VIDAIMOCK_IMAGE="${VIDAIMOCK_IMAGE:-ghcr.io/vidaiuk/vidaimock:latest@sha256:8eb48a3f3016aa0baf105737fc688a59980a267cbfa8c74c501fde915cc138b1}"
WORK="$(mktemp -d)"
OK=0
ROUTE="bedrock-$RANDOM"
CONTAINER="oag-vidaimock-$$"

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32m  ok  %s\033[0m\n' "$*"; }

cleanup() {
  local code=$?
  kill ${GW_PID:-0} 2>/dev/null || true
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  [ "$OK" = "1" ] && rm -rf "$WORK" || echo "logs kept in $WORK"
  exit $code
}
trap cleanup EXIT

command -v docker >/dev/null || fail "docker is required to run Bedrock verification"

say "1/4  infrastructure"
if [ -z "${OAG_DATABASE__URL:-}" ]; then
  just dev-up >/dev/null
fi
eval "$(just _verify-env)"
just migrate >/dev/null
pass "postgres, redis, schema"

say "2/4  VidaiMock with Anthropic-on-Bedrock overlay"
docker run -d --name "$CONTAINER" -p "$VIDAIMOCK_PORT:8100" \
  -v "$REPO_ROOT/deploy/test/vidaimock:/overrides:ro" \
  "$VIDAIMOCK_IMAGE" \
  --host 0.0.0.0 --port 8100 --config-dir /overrides \
  >"$WORK/docker.cid"
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$VIDAIMOCK_PORT/health" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "http://127.0.0.1:$VIDAIMOCK_PORT/health" >/dev/null \
  || fail "vidaimock never became ready; docker logs $CONTAINER"
pass "vidaimock on :$VIDAIMOCK_PORT"

say "3/4  catalog, bedrock account, gateway"
KEY="$(
  cargo run --quiet -p oag -- admin init --email bedrock@localhost --route "$ROUTE" 2>/dev/null \
    | grep -oE 'oag_live_[0-9a-f]+' | head -1
)"
[ -n "$KEY" ] || fail "init produced no key"
cargo run --quiet -p oag -- admin catalog seed >/dev/null
cargo run --quiet -p oag -- admin catalog seed --from "$REPO_ROOT/deploy/test/vidaimock/catalog.json" >/dev/null
cargo run --quiet -p oag -- admin account add --name "bedrock-$ROUTE" --provider bedrock \
  --secret 'AKIATEST:wJalrXUtnFEMI/K7MDENG' --route "$ROUTE" >/dev/null

OAG_GATEWAY__PROVIDER_BASE_URLS__BEDROCK="http://127.0.0.1:$VIDAIMOCK_PORT" \
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
ledger() {
  local since="$1" row=""
  for _ in $(seq 1 40); do
    row="$(psql "$OAG_DATABASE__URL" -At -F'|' -c "SELECT model_id, tier, input_tokens, output_tokens, \
        cost_usd, counterfactual_usd \
        FROM usage_event WHERE model_id LIKE 'bedrock%' AND occurred_at >= '$since' \
        ORDER BY occurred_at DESC LIMIT 1")" || return 1
    [ -n "$row" ] && break
    sleep 0.25
  done
  printf '%s\n' "$row"
}

assert_ledger() {
  local since="$1"
  local row
  row="$(ledger "$since")" || fail "could not read the bedrock ledger"
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

MODEL='bedrock/anthropic.claude-haiku-4-5-v1:0'

say "4/4  Bedrock invoke + invoke-with-response-stream through the gateway"
since="$(mark)"
curl -sS --max-time 30 -o "$WORK/whole.json" -w "%{http_code}" \
  -X POST "http://$PUBLIC/v1/messages" \
  -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL\",\"max_tokens\":32,\"stream\":false,
       \"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}" \
  >"$WORK/whole.status"
[ "$(cat "$WORK/whole.status")" = "200" ] \
  || fail "non-stream: HTTP $(cat "$WORK/whole.status") $(cat "$WORK/whole.json")"
grep -q 'bedrock mock' "$WORK/whole.json" || fail "non-stream body: $(cat "$WORK/whole.json")"
assert_ledger "$since"
pass "non-stream invoke + ledger"

since="$(mark)"
curl -sN --max-time 30 -X POST "http://$PUBLIC/v1/messages" \
  -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL\",\"max_tokens\":32,\"stream\":true,
       \"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}" \
  >"$WORK/stream.txt"
grep -q 'event: content_block_delta' "$WORK/stream.txt" \
  || fail "stream had no content deltas: $(head -c 500 "$WORK/stream.txt")"
grep -q 'bedrock' "$WORK/stream.txt" \
  || fail "stream had no overlay text: $(head -c 500 "$WORK/stream.txt")"
grep -q 'event: message_delta\|event: message_stop' "$WORK/stream.txt" \
  || fail "stream never completed: $(tail -c 400 "$WORK/stream.txt")"
assert_ledger "$since"
pass "stream invoke-with-response-stream + ledger"

OK=1
printf '\n\033[32mPASS: Bedrock decoder ate VidaiMock event-stream frames\033[0m\n'
