#!/usr/bin/env bash
# Does a failed migration actually fail `terraform apply`?
#
# That question is the whole point of the migrate step, and it is the one thing
# about it that cannot be checked without a real cloud account. Everything else
# in deploy/tofu is covered by `terraform validate`. This script is what you run
# once per cloud, against a throwaway project, when you have credentials.
#
# The method: apply cleanly, corrupt the migration ledger the way a genuinely
# broken migration would, force a redeploy, and assert the second apply FAILS.
# A green second apply means the gate is not real on that platform, whatever the
# configuration says.
#
# Use data_mode = "neutral" (Neon + Upstash). It is far cheaper than the managed
# tier, provisions in seconds rather than ~15 minutes, and — critically — leaves
# the database reachable from here, which is what makes step 3 possible at all.
#
#   ./verify-migration-gate.sh stacks/gcp-cloudrun  -var project_id=... -var region=...
#
# Everything after the stack directory is passed to terraform verbatim.
set -euo pipefail

STACK="${1:?usage: verify-migration-gate.sh <stack-dir> [terraform -var flags...]}"
shift
cd "$(dirname "$0")/$STACK"

: "${OAG_VERIFY_DATABASE_URL:?set it to the same postgres URL you pass as neutral_database_url}"

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\n\033[32mPASS: %s\033[0m\n' "$*"; }

say "1/4  apply cleanly — this must succeed"
terraform init -input=false >/dev/null
terraform apply -auto-approve -input=false "$@" \
  || fail "the baseline apply failed; fix that before testing the gate"

say "2/4  confirm the schema is actually there"
psql "$OAG_VERIFY_DATABASE_URL" -Atc \
  "SELECT count(*) FROM information_schema.tables WHERE table_name = 'usage_event'" \
  | grep -qx 1 || fail "the apply succeeded but did not migrate — the gate is moot, the step never ran"
pass "migrations ran on a clean apply"

say "3/4  corrupt the ledger so the next migrate must fail"
# A wrong checksum on an applied migration is exactly what sqlx refuses to run
# past, and it is the closest reachable stand-in for a migration that is broken
# rather than merely absent. `ignore_missing` does not rescue this case: it
# forgives migrations the binary has never heard of, not ones whose content
# disagrees with what was applied.
psql "$OAG_VERIFY_DATABASE_URL" -Atc \
  "UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1" >/dev/null
psql "$OAG_VERIFY_DATABASE_URL" -Atc \
  "SELECT checksum = '\\x00' FROM _sqlx_migrations WHERE version = 1" \
  | grep -qx t || fail "could not corrupt the ledger; nothing below would prove anything"
pass "ledger corrupted"

say "4/4  redeploy — the apply MUST fail"
# Something has to change, or ECS and Container Apps will reasonably decide
# there is nothing to deploy and never re-run the migration. (Cloud Run would
# re-execute anyway, because run_execution_token is unique per apply — but the
# same second tag keeps this script identical across the three.)
#
# Re-tag the same digest rather than building anything:
#     docker buildx imagetools create -t <repo>:verify-2 <repo>:main
: "${OAG_VERIFY_IMAGE_2:?set it to a second image tag — re-tag the same digest; \
ECS and Container Apps will not redeploy an unchanged task definition}"

if terraform apply -auto-approve -input=false "$@" \
     -var "image=$OAG_VERIFY_IMAGE_2"; then
  fail "apply returned GREEN over a database whose migration cannot run.
  The gate is not real on this platform. Do not rely on it; treat this stack the
  way docs/04-cloud.md already treats Azure, and check the migration out of band
  after every deploy."
fi
pass "a failed migration failed the apply — the gate holds"

cat <<'EOF'

Now tear it down:

    terraform destroy -auto-approve

Then record the result in docs/04-cloud.md, replacing the "unverified" note for
this cloud with what you actually observed — including how long the failure took
to surface, which is what the timeouts in the module are sized against.
EOF
