#!/usr/bin/env bash
# Does the GCP stack apply against a GCP-shaped API, with no cloud account?
#
# floci (https://floci.io) is a local GCP emulator: it answers the real GCP REST
# APIs on http://localhost:4588 with no project, billing, or auth. Pointing the
# terraform google provider at it lets us apply the deploy stack's API-driven
# resources and see them created — a step beyond `terraform validate`, which
# only parses the config, and short of a real deploy.
#
# What this proves, and what it does NOT:
#   - PROVES: the Secret Manager + data layer of stacks/gcp-cloudrun plans and
#     applies cleanly against a GCP-shaped API. The four secrets and their
#     versions are really created in the emulator.
#   - Does NOT run the gateway. floci mocks control-plane APIs; it does not run
#     the container. "Does the app work" is covered by `just verify`,
#     `just verify-k8s`, and the compose stack — not by this.
#   - Does NOT cover google_service_account / Cloud Run. The google provider
#     routes those to an endpoint the emulator override does not intercept (they
#     leak to real googleapis.com and 401). A full apply is verified against a
#     real throwaway project by verify-migration-gate.sh, not here.
#
# Needs Docker and terraform (or tofu). No credentials.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FLOCI_PORT="${FLOCI_PORT:-4588}"
FLOCI_IMAGE="${FLOCI_IMAGE:-floci/floci-gcp:latest}"
CONTAINER="oag-floci-$$"
WORK="$(mktemp -d)"
TF="$(command -v tofu || command -v terraform)"
OK=0

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  [ "$OK" = 1 ] && echo "PASS: the GCP stack's secret + data layer applies against floci" \
                || echo "FAIL: see output above"
}
trap cleanup EXIT

echo "== 1/4  floci emulator"
docker run -d --name "$CONTAINER" -p "$FLOCI_PORT:4588" "$FLOCI_IMAGE" >/dev/null
for _ in $(seq 1 40); do
  curl -sf -o /dev/null "http://localhost:$FLOCI_PORT/v1/projects/demo/secrets" && break
  sleep 1
done
echo "  ok  floci on :$FLOCI_PORT"

echo "== 2/4  stack copy, pointed at the emulator"
cp -R "$REPO_ROOT/deploy/tofu" "$WORK/tofu"
STACK="$WORK/tofu/stacks/gcp-cloudrun"
rm -rf "$STACK/.terraform" "$STACK"/terraform.tfstate*
cat > "$STACK/floci_override.tf" <<EOF
# Merged onto the base provider block: sends the google provider at floci.
provider "google" {
  access_token                    = "floci-fake-token"
  cloud_run_v2_custom_endpoint    = "http://localhost:$FLOCI_PORT/v2/"
  secret_manager_custom_endpoint  = "http://localhost:$FLOCI_PORT/v1/"
  iam_custom_endpoint             = "http://localhost:$FLOCI_PORT/v1/"
}
EOF

echo "== 3/4  terraform init"
"$TF" -chdir="$STACK" init -input=false -no-color >/dev/null
echo "  ok  initialised"

echo "== 4/4  apply the secret + data layer against floci"
# neutral data mode: no Cloud SQL/Memorystore to emulate. The URLs must satisfy
# the stack's own preflight (pooled Neon host, sslmode); they are never dialled.
# -target is deliberate: it is the slice floci fully serves (see the header).
CLOUDFLARE_API_TOKEN=floci-dummy "$TF" -chdir="$STACK" apply -input=false -no-color -auto-approve \
  -target=module.data_neutral \
  -target=google_secret_manager_secret.this \
  -target=google_secret_manager_secret_version.this \
  -var project_id=floci-demo \
  -var image=ghcr.io/hexuria/open-ai-gateway:main \
  -var data_mode=neutral \
  -var neutral_database_url='postgres://u:p@db-pooler.neon.tech/oag?sslmode=require' \
  -var neutral_redis_url='rediss://u:p@redis.upstash.io:6379' \
  -var signing_secret='floci-smoke-test-signing-secret-0123456789' \
  -var credential_kek='ZmxvY2ktc21va2UtdGVzdC1rZWstMzJieXRlcyEhIQ==' \
  -var cloudflare_zone_id='' \
  | grep -iE "creating|complete|Resources:" || true

# Assert the emulator actually holds the four secrets we just applied.
count=$(curl -s "http://localhost:$FLOCI_PORT/v1/projects/floci-demo/secrets" \
        | grep -o 'open-ai-gateway-' | wc -l | tr -d ' ')
echo "  floci holds $count gateway secrets (expect 4)"
[ "$count" -ge 4 ] && OK=1
