#!/usr/bin/env bash
# Does the chart render correctly in every data mode — including its default?
#
# The only other place the chart is exercised is kind-verify.sh, which pins
# `data.mode=inCluster`. That is the one mode that never had the bug this exists
# to catch: the migrate Job's wait-for-postgres init container rendered
# `PGHOST: ""` in `external` (the default), looped 120 times on an empty host,
# and made the chart uninstallable and un-upgradeable out of the box. Nothing
# rendered the default mode, so nothing noticed.
#
# This needs no cluster and takes a second. Three modes, three assertions:
#   1. default — no `--set data.mode` at all, only the values the chart
#      requires. The literal default is the path that shipped broken, so it is
#      exercised as the literal default rather than as an explicit `external`.
#   2. external via existingSecret — same expectation.
#   3. inCluster — the init container is present and PGHOST names the
#      StatefulSet the chart itself starts.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$REPO_ROOT/deploy/helm/open-ai-gateway"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mok\033[0m  %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; exit 1; }

# Test-only values, byte-identical to the ones in ci.yml. The KEK must decode to
# exactly 32 bytes or the chart's validation refuses to render.
render() {
  helm template t "$CHART" -s templates/migrate-job.yaml \
    --set security.signingSecret="ci-only-signing-secret-0123456789abcdefghij" \
    --set security.credentialKek="Y2ktb25seS1rZWstMzItYnl0ZXMtMDEyMzQ1Njc4OTA=" \
    "$@"
}

say "1/3  default mode (external, not set explicitly)"
out="$(render \
  --set data.external.databaseUrl="postgres://oag:oag@db.example.invalid:5432/oag" \
  --set data.external.redisUrl="redis://cache.example.invalid:6379")"
grep -q 'initContainers' <<<"$out" && fail "default mode must not wait for an in-cluster Postgres it does not start"
grep -q 'PGHOST' <<<"$out" && fail "default mode rendered a PGHOST it cannot know"
grep -q 'name: migrate' <<<"$out" || fail "default mode did not render the migrate container at all"
pass "no init container, migrate container present"

say "2/3  external via existingSecret"
out="$(render --set data.external.existingSecret=my-data)"
grep -q 'initContainers' <<<"$out" && fail "existingSecret mode must not wait for an in-cluster Postgres"
pass "no init container"

say "3/3  inCluster"
out="$(render --set data.mode=inCluster)"
grep -q 'initContainers' <<<"$out" || fail "inCluster mode must wait for the StatefulSet it starts"
grep -q 'value: t-open-ai-gateway-postgres' <<<"$out" \
  || fail "inCluster PGHOST must name the chart's own Postgres; got: $(grep -A1 'name: PGHOST' <<<"$out" | tail -1)"
pass "init container waits on t-open-ai-gateway-postgres"

printf '\n\033[32mPASS: the chart renders in all three data modes\033[0m\n'
