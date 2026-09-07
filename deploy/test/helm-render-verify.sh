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
#
# Then the two guards and the escape hatch: the PDB floor (D9), a secret change
# rolling the pods (H12), and `extraEnv` overriding a key the chart names (D10).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$REPO_ROOT/deploy/helm/open-ai-gateway"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mok\033[0m  %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; exit 1; }

# Test-only values, byte-identical to the ones in ci.yml. The KEK decodes to
# exactly 32 bytes because `Kek::from_base64` refuses anything else at process
# start — not because the chart checks: it does not. A 48-byte KEK renders
# perfectly here and then crash-loops every replica, which is worth knowing
# before you conclude from a green render that the value is good.
render() {
  helm template t "$CHART" -s templates/migrate-job.yaml \
    --set security.signingSecret="ci-only-signing-secret-0123456789abcdefghij" \
    --set security.credentialKek="Y2ktb25seS1rZWstMzItYnl0ZXMtMDEyMzQ1Njc4OTA=" \
    "$@"
}

say "1/7  default mode (external, not set explicitly)"
out="$(render \
  --set data.external.databaseUrl="postgres://oag:oag@db.example.invalid:5432/oag" \
  --set data.external.redisUrl="redis://cache.example.invalid:6379")"
grep -q 'initContainers' <<<"$out" && fail "default mode must not wait for an in-cluster Postgres it does not start"
grep -q 'PGHOST' <<<"$out" && fail "default mode rendered a PGHOST it cannot know"
grep -q 'name: migrate' <<<"$out" || fail "default mode did not render the migrate container at all"
pass "no init container, migrate container present"

say "2/7  external via existingSecret"
out="$(render --set data.external.existingSecret=my-data)"
grep -q 'initContainers' <<<"$out" && fail "existingSecret mode must not wait for an in-cluster Postgres"
pass "no init container"

say "3/7  inCluster"
out="$(render --set data.mode=inCluster)"
grep -q 'initContainers' <<<"$out" || fail "inCluster mode must wait for the StatefulSet it starts"
grep -q 'value: t-open-ai-gateway-postgres' <<<"$out" \
  || fail "inCluster PGHOST must name the chart's own Postgres; got: $(grep -A1 'name: PGHOST' <<<"$out" | tail -1)"
pass "init container waits on t-open-ai-gateway-postgres"

say "4/7  every tunable the runbooks name is settable from the chart"
# `OagReplicaShedding` says to raise server.max_in_flight. A knob an alert
# points at and the chart cannot set is a runbook step that cannot be done.
out="$(helm template t "$CHART" -s templates/configmap.yaml \
  --set security.signingSecret="ci-only-signing-secret-0123456789abcdefghij" \
  --set security.credentialKek="Y2ktb25seS1rZWstMzItYnl0ZXMtMDEyMzQ1Njc4OTA=" \
  --set data.external.existingSecret=my-data \
  --set server.maxInFlight=128 \
  --set database.statementTimeoutSeconds=45 \
  --set gateway.upstreamResponseTimeoutSeconds=120 \
  --set gateway.clientWriteTimeoutSeconds=90 \
  --set gateway.spendReconcileIntervalSeconds=300)"
for pair in 'OAG_SERVER__MAX_IN_FLIGHT: "128"' \
            'OAG_DATABASE__STATEMENT_TIMEOUT: "45"' \
            'OAG_GATEWAY__UPSTREAM_RESPONSE_TIMEOUT: "120"' \
            'OAG_GATEWAY__CLIENT_WRITE_TIMEOUT: "90"' \
            'OAG_GATEWAY__SPEND_RECONCILE_INTERVAL: "300"'; do
  grep -qF "$pair" <<<"$out" || fail "configmap did not render $pair"
done
pass "max_in_flight, statement_timeout, upstream_response_timeout, client_write_timeout, spend_reconcile_interval"

say "5/7  a secret change rolls the pods"
# H12. The pod template carried `checksum/config` and no `checksum/secret`, so
# `helm upgrade` with a new `credentialKek` updated the Secret and left the pod
# template byte-identical: a successful upgrade with nothing to roll. The
# rotation then landed pod by pod at arbitrary times, through HPA scale-ups and
# node drains, giving a fleet where some pods could decrypt sealed credentials
# and some could not — with no deploy to correlate against.
#
# Two renders differing only in the KEK. If the annotation is missing the pod
# templates are identical, which is the bug stated as a diff.
deployment() {
  helm template t "$CHART" -s templates/deployment.yaml \
    --set security.signingSecret="ci-only-signing-secret-0123456789abcdefghij" \
    --set security.credentialKek="$1" \
    --set data.external.databaseUrl="postgres://oag:oag@db.example.invalid:5432/oag" \
    --set data.external.redisUrl="redis://cache.example.invalid:6379"
}
before="$(deployment "Y2ktb25seS1rZWstMzItYnl0ZXMtMDEyMzQ1Njc4OTA=")" \
  || fail "chart did not render"
after="$(deployment "YW5vdGhlci1rZWstMzItYnl0ZXMtMDEyMzQ1Njc4OTA=")" \
  || fail "chart did not render with the second KEK"

grep -q "checksum/secret:" <<<"$before" || fail "no checksum/secret annotation"
[ "$before" != "$after" ] \
  || fail "rotating the KEK left the pod template byte-identical; nothing would roll"
pass "a new KEK changes the pod template"

# And skipped when the chart renders no Secret of its own: an annotation that
# can never change is noise on every pod, and would imply a guarantee the chart
# cannot make about someone else's Secret.
supplied="$(helm template t "$CHART" -s templates/deployment.yaml \
  --set security.existingSecret=my-security \
  --set data.mode=external \
  --set data.external.existingSecret=my-data)" || fail "chart did not render"
grep -q "checksum/secret:" <<<"$supplied" \
  && fail "checksum/secret rendered for a chart that owns no Secret"
pass "no checksum/secret when both secrets are supplied"

say "6/7  the PDB floor is compared against the floor that applies"
# D9. The guard was nested inside `if not autoscaling.enabled`, and autoscaling
# defaults to TRUE — so in the shipped configuration it could not fire at all.
# It also compared against `replicaCount`, which is not the fleet's floor while
# an HPA is running. A `minAvailable` at or above the floor means no pod can
# ever be evicted and a node drain hangs forever.
#
# Nothing rendered these combinations, so the fix was verified by hand and the
# next edit to `_helpers.tpl` would have had nothing to fail against.
guarded() {
  helm template t "$CHART" -s templates/pdb.yaml \
    --set security.signingSecret="ci-only-signing-secret-0123456789abcdefghij" \
    --set security.credentialKek="Y2ktb25seS1rZWstMzItYnl0ZXMtMDEyMzQ1Njc4OTA=" \
    --set data.external.databaseUrl="postgres://oag:oag@db.example.invalid:5432/oag" \
    --set data.external.redisUrl="redis://cache.example.invalid:6379" \
    "$@" 2>&1
}

# The shipped defaults: minAvailable 2 under a floor of 3. Renders.
guarded > /dev/null || fail "the shipped defaults must render"

# Autoscaling on, floor lowered to the PDB's own number. Refused, naming the
# value that applies — this is the configuration the old guard could not see.
out="$(guarded --set autoscaling.minReplicas=2 || true)"
grep -q 'autoscaling.minReplicas' <<<"$out" \
  || fail "minAvailable=2 against minReplicas=2 was not refused, or did not name the floor: $out"

# Autoscaling off, so the floor is replicaCount and the message has to say so.
out="$(guarded --set autoscaling.enabled=false --set replicaCount=2 || true)"
grep -q 'replicaCount' <<<"$out" \
  || fail "minAvailable=2 against replicaCount=2 was not refused, or did not name the floor: $out"
grep -q 'autoscaling.minReplicas' <<<"$out" \
  && fail "the message named the HPA floor for a chart with no HPA: $out"
pass "refused against both floors, and each message names the one it used"

say "7/7  extraEnv overrides a key the chart names"
# D10. `extraEnv` is the escape hatch for a setting the chart has never heard
# of, and it renders last so it also wins over the keys the chart does spell.
# That is deliberate — an operator who needs a value the chart spells
# differently should not wait for a chart release — and it is exactly the kind
# of ordering that survives a template edit unnoticed.
out="$(helm template t "$CHART" -s templates/configmap.yaml \
  --set security.signingSecret="ci-only-signing-secret-0123456789abcdefghij" \
  --set security.credentialKek="Y2ktb25seS1rZWstMzItYnl0ZXMtMDEyMzQ1Njc4OTA=" \
  --set data.external.databaseUrl="postgres://oag:oag@db.example.invalid:5432/oag" \
  --set data.external.redisUrl="redis://cache.example.invalid:6379" \
  --set server.maxInFlight=128 \
  --set-string extraEnv.OAG_SERVER__MAX_IN_FLIGHT=999 \
  --set-string extraEnv.OAG_NOT_A_CHART_KEY=hello)" || fail "chart did not render"

grep -qF 'OAG_NOT_A_CHART_KEY: "hello"' <<<"$out" \
  || fail "extraEnv did not carry a key the chart has never heard of"
# The last duplicate key is the one Kubernetes keeps, so the override has to be
# the last occurrence — not merely present.
[ "$(grep -c 'OAG_SERVER__MAX_IN_FLIGHT:' <<<"$out")" -ge 1 ] \
  || fail "the chart key vanished entirely"
[ "$(grep 'OAG_SERVER__MAX_IN_FLIGHT:' <<<"$out" | tail -1)" = '  OAG_SERVER__MAX_IN_FLIGHT: "999"' ] \
  || fail "extraEnv did not win: $(grep 'OAG_SERVER__MAX_IN_FLIGHT:' <<<"$out")"
pass "an unknown key is carried, and a known one is overridden last"

printf '\n\033[32mPASS: the chart renders in all three data modes\033[0m\n'
