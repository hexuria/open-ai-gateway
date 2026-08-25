#!/usr/bin/env bash
# Deploy OAG to a local floci "GCP" using CLOUD SQL for the database — the
# `managed` data tier, so the demo mirrors a real GCP deploy (Cloud SQL for
# PostgreSQL) instead of a plain container.
#
# floci-gcp Docker-backs Cloud SQL for real: a `google_sql_database_instance`
# genuinely spawns a Postgres 16 container on the compose network, reachable by
# the OAG Cloud Run container floci also starts. So `terraform apply` of the
# real `gcp-cloudrun` stack in `managed` mode actually stands up Cloud SQL and
# runs the gateway against it — no cloud account, no billing.
#
# It is a REHEARSAL, not production. floci is an emulator, so a throwaway copy of
# the stack (the real stack is never touched) is patched for it:
#   1. Cloud SQL gets a public IP on the compose network — floci has no VPC, and
#      private-IP + Direct VPC egress is how the real managed tier connects.
#   2. Memorystore is dropped and Redis stays a container — floci Docker-backs
#      Cloud SQL but NOT Memorystore. The demo is honest about this.
#   3. Secrets come as plain env, not Secret Manager valueSource; the secret-IAM
#      bindings and their propagation sleep are dropped — meaningless on floci.
#   4. The migrate Cloud Run *job* is skipped (floci runs services, not jobs); a
#      one-off `oag migrate` container applies the schema, after Cloud SQL is up
#      and before the gateway serves.
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

say "1/6  floci + Redis (Cloud SQL replaces the Postgres container)"
$COMPOSE up -d --wait floci-gcp redis
for _ in $(seq 1 40); do
  curl -sf -o /dev/null "http://localhost:4588/v1/projects/x/secrets" && break; sleep 1
done
echo "  floci up on :4588"

say "2/6  secrets (shared by the migrate step and the deployed service)"
SIGNING="$(openssl rand -base64 48 | tr -d '\n')"
KEK="$(openssl rand -base64 32 | tr -d '\n')"
REDIS_URL="redis://redis:6379"

say "3/6  a floci-patched copy of the stack (managed tier, Cloud SQL)"
cp -R "$REPO_ROOT/deploy/tofu" "$WORK/tofu"
STACK="$WORK/tofu/stacks/gcp-cloudrun"
rm -rf "$STACK/.terraform" "$STACK"/terraform.tfstate* 2>/dev/null || true

# (a) point the google provider at floci, adding the Cloud SQL Admin endpoint.
#     iam is the bare base ("/"), which is what makes service accounts resolve.
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
  sql_custom_endpoint              = "http://localhost:4588/sql/v1beta4/"
}
EOF

# A normal file (not an _override.tf, which can only override existing
# definitions): exposes the managed tier's database_url so this script can
# migrate the schema against Cloud SQL before the gateway service starts.
cat > "$STACK/floci_dburl.tf" <<'EOF'
output "floci_database_url" {
  value     = local.database_url
  sensitive = true
}
EOF

# (b) patch the Cloud SQL module for floci: public IP on the compose network
#     (no VPC), and drop Memorystore — floci does not back it, so Redis stays a
#     container and redis_url points there.
python3 - "$WORK/tofu/modules/data-gcp/main.tf" "$WORK/tofu/modules/data-gcp/outputs.tf" <<'PY'
import sys, re
main_p, outs_p = sys.argv[1], sys.argv[2]

s = open(main_p).read()
# Public IP, reachable on the compose network; drop the private-network line.
s = s.replace("ipv4_enabled    = false", "ipv4_enabled    = true")
s = re.sub(r'\n\s*private_network = var\.network_id', '', s)
# Memorystore is not emulated; remove the whole resource.
s = re.sub(r'resource "google_redis_instance" "this" \{.*?\n\}\n', '', s, flags=re.DOTALL)
open(main_p, 'w').write(s)

o = open(outs_p).read()
# The instance reports a public IP on floci, not a private one.
o = o.replace("google_sql_database_instance.this.private_ip_address",
              "google_sql_database_instance.this.public_ip_address")
# redis_url no longer has a Memorystore resource to read; point at the container.
o = re.sub(r'value\s*=\s*"redis://\$\{google_redis_instance\.this\.host\}:\$\{google_redis_instance\.this\.port\}"',
           'value     = "redis://redis:6379"', o)
open(outs_p, 'w').write(o)
PY

# (c) plain env instead of Secret Manager valueSource, and drop the secret-IAM
#     bindings + their propagation sleep (all meaningless against floci). Same
#     patch the neutral floci deploy makes.
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

COMMON=(-var project_id=floci-local -var region=us-central1 -var image="$IMAGE"
  -var data_mode=managed -var 'network_id=' -var 'vpc_subnet='
  -var neutral_database_url='' -var neutral_redis_url=''
  -var signing_secret="$SIGNING" -var credential_kek="$KEK"
  -var cloudflare_zone_id='' -var run_migrations=false)

say "4/6  terraform apply — floci creates Cloud SQL and the gateway service"
"$TF" -chdir="$STACK" init -input=false -no-color >/dev/null
CLOUDFLARE_API_TOKEN=floci GOOGLE_OAUTH_ACCESS_TOKEN=floci-fake \
  "$TF" -chdir="$STACK" apply -input=false -no-color -auto-approve "${COMMON[@]}" >/dev/null
DB_URL="$("$TF" -chdir="$STACK" output -raw floci_database_url)"
echo "  Cloud SQL up: ${DB_URL%@*}@<cloud-sql>"

# The gateway service just started against an empty database and is reporting
# not-ready. Migrate the schema now (floci runs services, not the Cloud Run
# migrate job); the service's readiness flips once the schema exists.
say "5/6  migrate the schema against Cloud SQL"
docker run --rm --network "$NETWORK" \
  -e OAG_DATABASE__URL="$DB_URL" -e OAG_REDIS__URL="$REDIS_URL" \
  -e OAG_SECURITY__SIGNING_SECRET="$SIGNING" -e OAG_SECURITY__CREDENTIAL_KEK="$KEK" \
  "$IMAGE" migrate
echo "  schema applied"

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

OAG is deployed on floci as a Cloud Run service backed by CLOUD SQL, at
$IP:8080 (single-listener). The database is a floci-spawned Cloud SQL Postgres
container ($(docker ps --format '{{.Names}}' | grep floci-gcp-cloudsql | head -1)); Redis is a container (floci does not back Memorystore).

Reach it from a container on the $NETWORK network. To send traffic, bootstrap:

  docker run --rm --network $NETWORK \\
    -e OAG_DATABASE__URL='$DB_URL' -e OAG_REDIS__URL=$REDIS_URL \\
    -e OAG_SECURITY__SIGNING_SECRET='$SIGNING' -e OAG_SECURITY__CREDENTIAL_KEK='$KEK' \\
    $IMAGE admin init --email dev@localhost      # prints an admin key

Tear down with:  just floci-down
EOF
