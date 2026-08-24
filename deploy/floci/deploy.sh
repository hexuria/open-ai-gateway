#!/usr/bin/env bash
# Deploy the gcp-cloudrun stack to a LOCAL floci "GCP", and prove it serves.
#
# floci-gcp (https://floci.io) runs Cloud Run containers for real over the
# Docker socket, so this is a genuine deploy of stacks/gcp-cloudrun — floci
# creates the Secret Manager secrets, the service account and the Cloud Run
# service, then starts the OAG image, which connects to the Postgres + Redis in
# docker-compose.yml and serves. No cloud account, no billing.
#
# It is a REHEARSAL, not production: three floci-specific adjustments are made to
# a throwaway copy of the stack (the real stack is never touched), because floci
# is an emulator, not GCP:
#   1. env comes as plain values, not Secret Manager valueSource — floci's Cloud
#      Run execution does not resolve secret-backed env.
#   2. the migrate Cloud Run *job* is skipped (floci runs services, not jobs);
#      the schema is applied by a one-off `oag migrate` container instead.
#   3. the neutral-tier TLS/pooler preflight and the secret-IAM bindings are
#      dropped — pointless against an emulator that ignores auth and a local
#      Postgres with no TLS.
#
# Needs Docker and terraform (or tofu). Run from anywhere in the repo.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FLOCI_DIR="$REPO_ROOT/deploy/floci"
COMPOSE="docker compose -f $FLOCI_DIR/docker-compose.yml"
NETWORK="oag-floci_default"
IMAGE="${OAG_IMAGE:-ghcr.io/hexuria/open-ai-gateway:main}"
TF="$(command -v tofu || command -v terraform)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

say() { printf '\n\033[1;34m== %s\033[0m\n' "$1"; }

say "1/6  floci + Postgres + Redis"
$COMPOSE up -d --wait
for _ in $(seq 1 40); do
  curl -sf -o /dev/null "http://localhost:4588/v1/projects/x/secrets" && break; sleep 1
done
echo "  floci up on :4588"

say "2/6  secrets (shared by the migrate step and the deployed service)"
SIGNING="$(openssl rand -base64 48 | tr -d '\n')"
KEK="$(openssl rand -base64 32 | tr -d '\n')"
DB_URL="postgres://oag:oag@postgres:5432/oag"
REDIS_URL="redis://redis:6379"

say "3/6  migrate the database (floci runs Cloud Run services, not jobs)"
docker run --rm --network "$NETWORK" \
  -e OAG_DATABASE__URL="$DB_URL" -e OAG_REDIS__URL="$REDIS_URL" \
  -e OAG_SECURITY__SIGNING_SECRET="$SIGNING" -e OAG_SECURITY__CREDENTIAL_KEK="$KEK" \
  "$IMAGE" migrate

say "4/6  a floci-patched copy of the stack"
cp -R "$REPO_ROOT/deploy/tofu" "$WORK/tofu"
STACK="$WORK/tofu/stacks/gcp-cloudrun"
rm -rf "$STACK/.terraform" "$STACK"/terraform.tfstate* 2>/dev/null || true
# (a) point the google provider at floci — endpoints from floci's own provider.tf,
#     note iam is the bare base ("/"), which is what makes service accounts resolve.
cat > "$STACK/floci_override.tf" <<'EOF'
provider "google" {
  user_project_override            = false
  iam_custom_endpoint              = "http://localhost:4588/"
  iam_beta_custom_endpoint         = "http://localhost:4588/v1/"
  secret_manager_custom_endpoint   = "http://localhost:4588/v1/"
  cloud_run_custom_endpoint        = "http://localhost:4588/v2/"
  cloud_run_v2_custom_endpoint     = "http://localhost:4588/v2/"
  service_usage_custom_endpoint    = "http://localhost:4588/v1/"
  resource_manager_custom_endpoint = "http://localhost:4588/v1/"
}
EOF
# (b) drop the neutral preflight (local Postgres has no TLS; floci is not Neon).
python3 - "$WORK/tofu/modules/data-neutral/main.tf" "$WORK/tofu/modules/data-neutral/outputs.tf" <<'PY'
import sys, re
main, outs = sys.argv[1], sys.argv[2]
s = open(main).read()
s = re.sub(r'resource "terraform_data" "preflight" \{.*?\n\}\n', '', s, flags=re.DOTALL)
open(main, 'w').write(s)
o = open(outs).read()
o = re.sub(r'\n\s*depends_on = \[terraform_data\.preflight\]', '', o)
open(outs, 'w').write(o)
PY
# (c) plain env instead of Secret Manager valueSource, and drop the secret-IAM
#     bindings + their propagation sleep (all meaningless against floci).
python3 - "$STACK/main.tf" <<'PY'
import sys, re
p = sys.argv[1]; s = open(p).read()
s = re.sub(r'resource "google_secret_manager_secret_iam_member" "read" \{.*?\n\}\n', '', s, flags=re.DOTALL)
s = re.sub(r'resource "time_sleep" "iam_propagation" \{.*?\n\}\n', '', s, flags=re.DOTALL)
s = re.sub(r'\n\s*time_sleep\.iam_propagation,?', '', s)
s = re.sub(
    r'secret_env = \{.*?\}\n\n  env = var\.gateway_env',
    'secret_env = {}\n\n  env = {\n'
    '    OAG_DATABASE__URL            = local.database_url\n'
    '    OAG_REDIS__URL               = local.redis_url\n'
    '    OAG_SECURITY__SIGNING_SECRET = var.signing_secret\n'
    '    OAG_SECURITY__CREDENTIAL_KEK = var.credential_kek\n'
    '  }',
    s, flags=re.DOTALL)
open(p, 'w').write(s)
PY

say "5/6  terraform apply — floci starts the OAG Cloud Run container"
"$TF" -chdir="$STACK" init -input=false -no-color >/dev/null
CLOUDFLARE_API_TOKEN=floci GOOGLE_OAUTH_ACCESS_TOKEN=floci-fake \
  "$TF" -chdir="$STACK" apply -input=false -no-color -auto-approve \
    -var project_id=floci-local -var region=us-central1 -var image="$IMAGE" \
    -var data_mode=neutral -var neutral_database_url="$DB_URL" -var neutral_redis_url="$REDIS_URL" \
    -var signing_secret="$SIGNING" -var credential_kek="$KEK" \
    -var cloudflare_zone_id='' -var run_migrations=false >/dev/null
echo "  service created"

say "6/6  wait for OAG to come up, then health-check it"
IP=""
for _ in $(seq 1 30); do
  C=$(docker ps --format '{{.Names}}' | grep 'floci-gcp-cloudrun-open-ai-gateway' | head -1 || true)
  [ -n "$C" ] && IP=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NETWORK\").IPAddress}}" "$C" 2>/dev/null || true)
  [ -n "$IP" ] && docker run --rm --network "$NETWORK" curlimages/curl:latest -sf -o /dev/null "http://$IP:8080/health/ready" && break
  sleep 2
done
echo
docker run --rm --network "$NETWORK" curlimages/curl:latest -s "http://$IP:8080/health/ready"; echo
cat <<EOF

OAG is deployed on floci as a Cloud Run service, at $IP:8080 (single-listener).
Reach it from a container on the $NETWORK network. To send traffic, bootstrap:

  docker run --rm --network $NETWORK \\
    -e OAG_DATABASE__URL=$DB_URL -e OAG_REDIS__URL=$REDIS_URL \\
    -e OAG_SECURITY__SIGNING_SECRET='$SIGNING' -e OAG_SECURITY__CREDENTIAL_KEK='$KEK' \\
    $IMAGE admin init --email dev@localhost      # prints an admin key

Tear down with:  $COMPOSE down
EOF
