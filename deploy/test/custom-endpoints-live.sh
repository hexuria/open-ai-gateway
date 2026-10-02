#!/usr/bin/env bash
# Live end-to-end check of operator-registered endpoints: a real gateway
# process, the real CLI, and stand-in upstreams (deploy/test/mock-endpoints.py)
# for every dialect and platform: OpenAI/Anthropic/Gemini-shaped endpoints,
# System One hosts (incl. Merge Decisions' /v1/decisions), Merge's priced
# model list, Bedrock (Claude invoke + Converse, SigV4, event-stream), and
# Vertex (Gemini + Claude, service-account token minting). Azure is checked
# only for refusal here: a release build has no loopback stand-in for it; its
# request path is covered by crates/oag-server/tests/azure_endpoints.rs.
#
# Usage: deploy/test/custom-endpoints-live.sh [oag-binary]   (default: target/debug/oag)
# Needs Postgres (OAG_LIVE_PG, default postgres://oag:oag@127.0.0.1:5452) and
# Redis on 127.0.0.1:6399 (db 7). Uses ports 29280/29281 and 29301-29308, a
# throwaway database oag_live_it, and nothing else.
set -uo pipefail
W=$(cd "$(dirname "$0")/../.." && pwd)
B=${1:-$W/target/debug/oag}
D=$W/deploy/test
RUN=$(mktemp -d); chmod 700 "$RUN"
LOG=$RUN/upstream.jsonl; : > "$LOG"
PG=${OAG_LIVE_PG:-postgres://oag:oag@127.0.0.1:5452}; DB=oag_live_it
PASS=0; FAIL=0; FAILS=()
ok()   { PASS=$((PASS+1)); echo "  ok   $*"; }
bad()  { FAIL=$((FAIL+1)); FAILS+=("$*"); echo "  FAIL $*"; }
check(){ local name=$1; shift; if "$@"; then ok "$name"; else bad "$name"; fi; }

cleanup() {
  [ -f "$RUN/gw.pid" ] && kill "$(cat "$RUN/gw.pid")" 2>/dev/null
  for f in "$RUN"/mock-*.pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null; done
  psql "$PG/postgres" -qc "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null 2>&1
}
trap cleanup EXIT

export OAG_DATABASE__URL=$PG/$DB OAG_REDIS__URL=redis://127.0.0.1:6399/7
export OAG_SERVER__PUBLIC_ADDR=127.0.0.1:29280 OAG_SERVER__ADMIN_ADDR=127.0.0.1:29281
export OAG_SECURITY__SIGNING_SECRET=live-verify-signing-secret-0123456789abcdef
export OAG_SECURITY__CREDENTIAL_KEK=$(python3 -c 'import base64;print(base64.b64encode(b"k"*32).decode())')
export OAG_GATEWAY__CATALOG_REFRESH_INTERVAL=2
export OAG_GATEWAY__GCP_TOKEN_URL=http://127.0.0.1:29307/token
A() { "$B" admin "$@" 2>&1 | grep -v ' INFO ' ; }

echo "== setup"
psql "$PG/postgres" -qc "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null 2>&1
psql "$PG/postgres" -qc "CREATE DATABASE $DB" || exit 1
redis-cli -p 6399 -n 7 FLUSHDB >/dev/null 2>&1
"$B" migrate >/dev/null 2>&1 || { echo "migrate failed"; exit 1; }
i=0; for n in mockoai mockanth mockgem jevhost merge aws gcp mergedec; do
  port=$((29301+i)); i=$((i+1))
  python3 "$D/mock-endpoints.py" "$n" "$port" "$LOG" & echo $! > "$RUN/mock-$n.pid"
done
umask 077
A init --email live@localhost --route default | grep -oE 'oag_live_[A-Za-z0-9_-]+' | head -1 | tr -d '\n' > "$RUN/admin.key"
A key create --email live@localhost --route default --name live | grep -oE 'oag_live_[A-Za-z0-9_-]+' | head -1 | tr -d '\n' > "$RUN/infer.key"
ADMIN=$(cat "$RUN/admin.key"); KEY=$(cat "$RUN/infer.key")
[ -n "$ADMIN" ] && [ -n "$KEY" ] && ok "minted admin and inference keys" || { bad "key mint"; exit 1; }

echo "== endpoints (CLI)"
reg() { A endpoint add "$@" >"$RUN/last.out"; local rc=${PIPESTATUS[0]}; grep -q -iE "refus|error|invalid" "$RUN/last.out" && return 1; return 0; }
check "add mockoai (openai, bearer)"      reg --name mockoai  --dialect openai    --platform plain --base-url http://127.0.0.1:29301/v1 --auth bearer
check "add mockanth (anthropic, x_api_key)" reg --name mockanth --dialect anthropic --platform plain --base-url http://127.0.0.1:29302 --auth x_api_key
check "add mockgem (gemini, x_goog_api_key)" reg --name mockgem --dialect gemini --platform plain --base-url http://127.0.0.1:29303/v1beta --auth x_goog_api_key
check "add jevhost (system_one)"          reg --name jevhost  --dialect system_one --platform plain --base-url http://127.0.0.1:29304
check "add merge (openai)"                reg --name merge    --dialect openai    --platform plain --base-url http://127.0.0.1:29305/v1/openai --auth bearer
check "add bedrockeu (aws converse)"      reg --name bedrockeu --dialect bedrock_converse --platform aws --region eu-west-3 --base-url http://127.0.0.1:29306
check "add bedrockclaude (aws anthropic)" reg --name bedrockclaude --dialect anthropic --platform aws --region ap-northeast-2 --base-url http://127.0.0.1:29306
check "add vertexgem (gcp gemini)"        reg --name vertexgem --dialect gemini --platform gcp --region us-central1 --project liveproj --base-url http://127.0.0.1:29307
check "add vertexclaude (gcp anthropic)"  reg --name vertexclaude --dialect anthropic --platform gcp --region us-east5 --project liveproj --base-url http://127.0.0.1:29307
check "add mergedec (system_one, --path /v1/decisions)" reg --name mergedec --dialect system_one --platform plain --base-url http://127.0.0.1:29308 --path /v1/decisions
A endpoint add --name badazure --dialect openai --platform azure --base-url http://127.0.0.1:29399 > "$RUN/az.out"
check "azure endpoint at a loopback host is refused (release build has no stand-in)" grep -qiE "refus|azure|must" "$RUN/az.out"
A endpoint add --name sneaky --dialect anthropic --platform plain --base-url https://api.anthropic.com --auth bearer > "$RUN/cg.out"
check "compliance guard refuses a plain endpoint at api.anthropic.com" grep -qiE "refus|built-in|anthropic" "$RUN/cg.out"
A endpoint add --name meta --dialect openai --platform plain --base-url http://169.254.169.254/v1 > "$RUN/md.out"
check "metadata address refused" grep -qiE "refus|metadata|link" "$RUN/md.out"
check "endpoint list shows 10 endpoints" bash -c "[ \$(\"$B\" admin endpoint list 2>/dev/null | grep -cE '^(mockoai|mockanth|mockgem|jevhost|merge|bedrockeu|bedrockclaude|vertexgem|vertexclaude|mergedec)\\b') -ge 10 ]"

echo "== accounts"
acct() { local name=$1 prov=$2 secret=$3; shift 3; OAG_ACCOUNT_SECRET=$secret A account add --name "$name" --provider "$prov" --route default "$@" >"$RUN/acct.out"; ! grep -qiE "error|refus" "$RUN/acct.out"; }
check "account mockoai-good (priority 1: tried after the flaky one)" acct mockoai-good mockoai good-key-oai --priority 1
check "account mockoai-flaky"  acct mockoai-flaky mockoai flaky-key-oai
check "account mockanth"       acct mockanth-1 mockanth key-anth
check "account mockgem"        acct mockgem-1 mockgem key-gem
check "account jevhost"        acct jevhost-1 jevhost key-jev
check "account merge"          acct merge-1 merge key-merge
check "account mergedec"       acct mergedec-1 mergedec key-mergedec
check "account bedrockeu"      acct bedrockeu-1 bedrockeu "TESTACCESSKEY:TESTSECRETKEY"
check "account bedrockclaude"  acct bedrockclaude-1 bedrockclaude "TESTACCESSKEY:TESTSECRETKEY"
python3 - "$W/crates/oag-upstream/tests/fixtures/gcp-test-key.pem" > "$RUN/sa.json" <<'EOF'
import json,sys
print(json.dumps({"type":"service_account","project_id":"liveproj","private_key_id":"kid-live",
  "private_key":open(sys.argv[1]).read(),"client_email":"live@liveproj.iam.gserviceaccount.com",
  "token_uri":"http://127.0.0.1:29399/evil-token"}))
EOF
for e in vertexgem vertexclaude; do
  A account add --name "$e-1" --provider "$e" --route default --secret-file "$RUN/sa.json" > "$RUN/acct.out"
  check "account $e (service account JSON)" bash -c "! grep -qiE 'error|refus' '$RUN/acct.out' && ! grep -q 'BEGIN PRIVATE' '$RUN/acct.out'"
done

echo "== catalog"
cat_add() { A catalog add --id "$1" --upstream "$2" --input-per-mtok "$3" --output-per-mtok "$4" --context 128000 --max-output 8192 --tools >/dev/null; }
cat_add mockoai/oai-small oai-small 1 2
cat_add mockanth/claude-like claude-like 3 15
cat_add mockgem/gem-like gem-like 0.5 1.5
cat_add bedrockeu/mistral.mistral-large-2407-v1:0 mistral.mistral-large-2407-v1:0 2 6
cat_add bedrockclaude/anthropic.claude-sonnet-4-5-20250929-v1:0 anthropic.claude-sonnet-4-5-20250929-v1:0 3 15
cat_add vertexgem/gemini-3.8-flash gemini-3.8-flash 0.3 2.5
cat_add vertexclaude/claude-sonnet-4-5@20250929 claude-sonnet-4-5@20250929 3 15
cat_add jevhost/jev-latest jev-latest 0.04 0.01
cat_add mergedec/typesafe/jev-1.13 typesafe/jev-1.13 0.042 0.01
A endpoint sync merge --dry-run > "$RUN/sync-dry.out"
check "sync --dry-run writes nothing" bash -c "[ \$(psql '$PG/$DB' -Atc \"select count(*) from model_catalog where provider='merge'\") = 0 ]"
A endpoint sync merge > "$RUN/sync.out"
check "sync wrote exactly the 2 chat models (tts + deprecated skipped)" bash -c "[ \$(psql '$PG/$DB' -Atc \"select count(*) from model_catalog where provider='merge'\") = 2 ]"
check "sync priced GLM at the cheapest vendor (0.015/0.05)" bash -c "psql '$PG/$DB' -Atc \"select input_per_mtok||'/'||output_per_mtok from model_catalog where id='merge/zai/glm-5.3-flash'\" | grep -q '^0.015000/0.050000$'"
A endpoint sync merge > "$RUN/sync2.out"
check "second sync is idempotent (still 2 rows)" bash -c "[ \$(psql '$PG/$DB' -Atc \"select count(*) from model_catalog where provider='merge'\") = 2 ]"
A route tiers --route default cheap=merge/zai/glm-5.3-flash balanced=mockoai/oai-small strong=mockanth/claude-like > /dev/null

echo "== start gateway on 29280"
"$B" serve > "$RUN/gw.log" 2>&1 & echo $! > "$RUN/gw.pid"
for _ in $(seq 1 60); do curl -s -m 2 http://127.0.0.1:29281/health/ready | grep -q '"ready":true' && break; sleep 1; done
check "gateway ready" bash -c "curl -s -m 2 http://127.0.0.1:29281/health/ready | grep -q '\"ready\":true'"
sleep 3

chat() { curl -s -m 30 -H "Authorization: Bearer $KEY" -H 'content-type: application/json' http://127.0.0.1:29280/v1/chat/completions -d "{\"model\":\"$1\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":50${2:-}}"; }
says() { python3 -c "import json,sys;d=json.load(sys.stdin);c=d['choices'][0]['message']['content'];sys.exit(0 if '$1' in c else 1)" 2>/dev/null; }

echo "== chat through each endpoint (OpenAI-shaped client)"
for pair in "mockoai/oai-small:mockoai" "mockanth/claude-like:mockanth" "mockgem/gem-like:mockgem" \
            "merge/zai/glm-5.3-flash:merge" "bedrockeu/mistral.mistral-large-2407-v1:0:aws" \
            "bedrockclaude/anthropic.claude-sonnet-4-5-20250929-v1:0:aws" \
            "vertexgem/gemini-3.8-flash:gcp" "vertexclaude/claude-sonnet-4-5@20250929:gcp"; do
  model=${pair%:*}; want=${pair##*:}
  check "chat $model answered by $want" bash -c "$(declare -f chat says); KEY='$KEY'; chat '$model' | says 'from $want'"
done
check "oag/cheap routes to merge GLM" bash -c "$(declare -f chat says); KEY='$KEY'; chat oag/cheap | says 'from merge'"

echo "== cross-dialect and streaming"
check "Anthropic-shaped client to an OpenAI endpoint is translated" bash -c "curl -s -m 30 -H 'x-api-key: $KEY' -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' http://127.0.0.1:29280/v1/messages -d '{\"model\":\"mockoai/oai-small\",\"max_tokens\":50,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}' | grep -q 'from mockoai'"
check "Gemini-shaped client to Bedrock Converse is translated" bash -c "curl -s -m 30 -H 'x-goog-api-key: $KEY' -H 'content-type: application/json' 'http://127.0.0.1:29280/v1beta/models/bedrockeu/mistral.mistral-large-2407-v1:0:generateContent' -d '{\"contents\":[{\"role\":\"user\",\"parts\":[{\"text\":\"hi\"}]}]}' | grep -q 'from aws'"
check "stream: Bedrock ConverseStream event-stream relayed as SSE" bash -c "$(declare -f chat); KEY='$KEY'; chat 'bedrockeu/mistral.mistral-large-2407-v1:0' ',\"stream\":true' | grep -q 'from aws' && chat 'bedrockeu/mistral.mistral-large-2407-v1:0' ',\"stream\":true' | grep -q '\\[DONE\\]'"
check "stream: Vertex Claude streamRawPredict relayed" bash -c "$(declare -f chat); KEY='$KEY'; chat 'vertexclaude/claude-sonnet-4-5@20250929' ',\"stream\":true' | grep -q 'from gcp'"
check "stream: OpenAI endpoint relayed" bash -c "$(declare -f chat); KEY='$KEY'; chat mockoai/oai-small ',\"stream\":true' | grep -q 'from mockoai'"

echo "== failover (one key always 429)"
okc=0; for _ in $(seq 1 12); do chat mockoai/oai-small | says "from mockoai" && okc=$((okc+1)); done
check "12/12 requests succeed despite a 429 key" [ "$okc" = 12 ]
check "the 429 key was actually tried" bash -c "grep '\"upstream\": \"mockoai\"' '$LOG' | grep -q '\"flaky\": true'"

echo "== what the upstreams received"
py() { python3 - "$LOG" "$@"; }
check "mockanth got x-api-key, not Authorization" py <<'EOF'
import json,sys; rows=[json.loads(l) for l in open(sys.argv[1]) if '"mockanth"' in l and '"POST"' in l]
sys.exit(0 if rows and all(r["auth"]==["x-api-key"] for r in rows) else 1)
EOF
check "mockgem got x-goog-api-key only" py <<'EOF'
import json,sys; rows=[json.loads(l) for l in open(sys.argv[1]) if '"mockgem"' in l and '"POST"' in l]
sys.exit(0 if rows and all(r["auth"]==["x-goog-api-key"] for r in rows) else 1)
EOF
check "Bedrock calls SigV4-signed with each endpoint's own region" py <<'EOF'
import json,sys; rows=[json.loads(l) for l in open(sys.argv[1]) if '"upstream": "aws"' in l and '"POST"' in l]
regions={tuple(r["sigv4_scope"] or ()) for r in rows}
ok=rows and all(r["sigv4"] for r in rows) and ("eu-west-3","bedrock") in regions and ("ap-northeast-2","bedrock") in regions
sys.exit(0 if ok else 1)
EOF
check "Bedrock ARN-free model ids hit /model/<id>/converse|invoke" py <<'EOF'
import json,sys; paths=[json.loads(l)["path"] for l in open(sys.argv[1]) if '"upstream": "aws"' in l]
sys.exit(0 if any("/converse" in p for p in paths) and any("/invoke" in p for p in paths) else 1)
EOF
check "Vertex: token minted, model calls carry the minted bearer, paths per publisher" py <<'EOF'
import json,sys; rows=[json.loads(l) for l in open(sys.argv[1]) if '"upstream": "gcp"' in l]
tok=[r for r in rows if r["path"].endswith("/token")]; calls=[r for r in rows if not r["path"].endswith("/token")]
ok=tok and calls and all(r["bearer_is_minted"] for r in calls) \
   and any("/publishers/google/models/gemini-3.8-flash:generateContent" in r["path"] for r in calls) \
   and any("/publishers/anthropic/models/claude-sonnet-4-5%40" in r["path"] and r["path"].endswith(":rawPredict") for r in calls) \
   and all(r["anthropic_version"]=="vertex-2023-10-16" for r in calls if "anthropic" in r["path"] and r["path"].endswith(":rawPredict"))
sys.exit(0 if ok else 1)
EOF
check "Vertex token was cached (fewer mints than model calls)" py <<'EOF'
import json,sys; rows=[json.loads(l) for l in open(sys.argv[1]) if '"upstream": "gcp"' in l]
t=sum(r["path"].endswith("/token") for r in rows); c=len(rows)-t
sys.exit(0 if 0 < t < c else 1)
EOF
check "the SA JSON's token_uri was never contacted" bash -c "! grep -q 'evil-token' '$LOG'"

echo "== System One hosts"
ask() { curl -s -m 30 -H "Authorization: Bearer $KEY" -H 'content-type: application/json' http://127.0.0.1:29280/jev/v1/systemone -d "{\"model\":\"$1\",\"state\":\"user asked about pricing\",\"questions\":{\"q\":{\"type\":\"noul\",\"instructions\":\"is it about money?\"}}}"; }
check "System One via jevhost answers" bash -c "$(declare -f ask); KEY='$KEY'; ask jevhost/jev-latest | grep -q '\"answers\"'"
check "Merge Decisions via mergedec answers at /v1/decisions" bash -c "$(declare -f ask); KEY='$KEY'; ask mergedec/typesafe/jev-1.13 | grep -q '\"decision\"'"
check "decisions request went to /v1/decisions" bash -c "grep '\"upstream\": \"mergedec\"' '$LOG' | grep -q '/v1/decisions'"

echo "== listings, ledger, admin API"
curl -s -m 10 -H "Authorization: Bearer $KEY" http://127.0.0.1:29280/v1/models > "$RUN/models.json"
check "/v1/models lists models from every chat endpoint" python3 - "$RUN/models.json" <<'EOF'
import json,sys; ids={m["id"] for m in json.load(open(sys.argv[1]))["data"]}
need=["mockoai/oai-small","mockanth/claude-like","mockgem/gem-like","merge/zai/glm-5.3-flash","merge/deepseek/deepseek-v4-flash",
      "bedrockeu/mistral.mistral-large-2407-v1:0","vertexgem/gemini-3.8-flash","vertexclaude/claude-sonnet-4-5@20250929"]
miss=[n for n in need if n not in ids]; print("missing:",miss) if miss else None; sys.exit(1 if miss else 0)
EOF
check "every served chat call is in the ledger with a non-zero cost" bash -c "[ \$(psql '$PG/$DB' -Atc \"select count(*) from usage_event where cost_usd > 0 and split_part(model_id,'/',1) in ('mockoai','mockanth','mockgem','merge','bedrockeu','bedrockclaude','vertexgem','vertexclaude','jevhost','mergedec')\") -ge 20 ]"
check "admin API lists endpoints with the admin key" bash -c "curl -s -m 10 -H 'Authorization: Bearer $ADMIN' http://127.0.0.1:29281/admin/api/endpoints | grep -q mergedec"
check "admin API refuses the inference key" bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -m 10 -H 'Authorization: Bearer $KEY' http://127.0.0.1:29281/admin/api/endpoints) -ge 401 ]"
check "no secret appears in the admin API's endpoint view" bash -c "! curl -s -m 10 -H 'Authorization: Bearer $ADMIN' http://127.0.0.1:29281/admin/api/endpoints | grep -qE 'good-key|key-merge|TESTSECRET|BEGIN PRIVATE'"
check "no secret appears in the gateway log" bash -c "! grep -qE 'good-key-oai|key-merge|TESTSECRETKEY|BEGIN PRIVATE|ya29.mock-minted' '$RUN/gw.log'"

echo "== review fixes, end to end"
code=$(curl -s -o "$RUN/patch.out" -w '%{http_code}' -m 10 -X PATCH -H "Authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  http://127.0.0.1:29281/admin/api/endpoints/mockoai -d '{"base_url":"http://127.0.0.1:29399/v1"}')
check "admin API refuses moving a keyed endpoint's base_url (C1) [got $code]" bash -c "[ '$code' = 409 ] || [ '$code' = 400 ]"
check "the refused move left base_url unchanged" bash -c "psql '$PG/$DB' -Atc \"select base_url from endpoint where name='mockoai'\" | grep -q '29301'"
cat_add merge/hand/own-model hand/own-model 1 1
A endpoint sync merge > "$RUN/sync3.out"
check "sync keeps a model the operator added by hand (C3)" bash -c "[ \$(psql '$PG/$DB' -Atc \"select count(*) from model_catalog where id='merge/hand/own-model'\") = 1 ]"
ARN='arn:aws:bedrock:eu-west-3:123456789012:application-inference-profile/abc123'
cat_add "bedrockeu/$ARN" "$ARN" 2 6
sleep 3
check "a Bedrock ARN model id answers (K3)" bash -c "$(declare -f chat says); KEY='$KEY'; chat 'bedrockeu/$ARN' | says 'from aws'"
check "the ARN went upstream as one encoded path segment" bash -c "grep '\"upstream\": \"aws\"' '$LOG' | grep -q 'application-inference-profile%2Fabc123/converse'"

echo "== live changes without restart"
A endpoint add --name hotoai --dialect openai --platform plain --base-url http://127.0.0.1:29301/v1 --auth bearer >/dev/null
OAG_ACCOUNT_SECRET=hot-key A account add --name hotoai-1 --provider hotoai --route default >/dev/null
cat_add hotoai/oai-hot oai-hot 1 2
sleep 5
check "an endpoint added while running serves within the refresh interval" bash -c "$(declare -f chat says); KEY='$KEY'; chat hotoai/oai-hot | says 'from mockoai'"
A endpoint remove mockoai > "$RUN/rm.out"
check "removing an endpoint that still has keys is refused" grep -qiE "credential|in use|refus|still" "$RUN/rm.out"

echo
echo "RESULT: $PASS passed, $FAIL failed"
for f in ${FAILS[@]+"${FAILS[@]}"}; do echo "  - $f"; done
echo "logs: $RUN"
[ "$FAIL" = 0 ]
