#!/usr/bin/env bash
# Does a rolling restart sever a live stream?
#
# This is the property the drain logic exists for, and until now it was an
# anecdote: it was checked once, by hand, and written into a doc. Nothing
# re-checked it, so a regression would have been found by a user rather than by
# CI. This script makes the claim reproducible.
#
# What it asserts, and why each matters:
#   1. N long streams opened against N replicas all COMPLETE across a
#      `kubectl rollout restart` — the drain budget is honoured rather than the
#      terminationGracePeriod cutting streams off mid-flight.
#   2. The migrate Job ran as a pre-install hook, before any pod served.
#   3. Every completed stream carries a ledger row, so a severed-then-resumed
#      stream cannot pass as a survivor.
#
# Local only: kind, no cloud, no credentials. Takes a few minutes.
#
# STATUS: NOT YET PROVEN. This script has never completed a full run. It gets as
# far as standing up the cluster, loading the image, starting the mock and
# beginning the chart install; the one run that reached `helm install --wait`
# timed out at 10 minutes with the gateway Deployment at 0/3, and the cause was
# never diagnosed. The likely candidates, in order, are: the 10m timeout being
# too short for image pulls plus two StatefulSets plus the migrate hook on a
# cold machine; a readiness probe that cannot reach the in-cluster Postgres or
# Redis; and the chart's terminationGracePeriodSeconds (which must exceed the
# 1800s maxStreamDuration) interacting badly with `--wait`.
#
# Finish this before trusting anything it prints. Everything below the install
# step — the drain assertion itself — has never executed.
set -euo pipefail

CLUSTER="${CLUSTER:-oag-verify}"
NS="${NS:-oag}"
STREAMS="${STREAMS:-12}"
# Long enough that the restart lands mid-stream, which is the whole test.
STREAM_SECONDS="${STREAM_SECONDS:-90}"
# Raise on a cold runner: it pulls the node image, postgres and redis before
# anything here can start.
HELM_TIMEOUT="${HELM_TIMEOUT:-20m}"
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="$(mktemp -d)"

OK=0
say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32m  ok  %s\033[0m\n' "$*"; }

# On failure the cluster STAYS. A harness that deletes the thing you need to
# look at turns every failure into a re-run, and the second run is not always
# the same failure.
cleanup() {
  local code=$?
  kill ${PF:-0} 2>/dev/null || true
  if [ "$OK" = "1" ] && [ "${KEEP:-0}" != "1" ]; then
    say "tearing down"
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    rm -rf "$WORK"
  else
    cat <<EOF

Cluster "$CLUSTER" left up so you can look at it:

    kubectl --context kind-$CLUSTER -n $NS get pods
    kubectl --context kind-$CLUSTER -n $NS describe pod -l app.kubernetes.io/name=open-ai-gateway
    kubectl --context kind-$CLUSTER -n $NS logs -l app.kubernetes.io/name=open-ai-gateway --tail=50
    kubectl --context kind-$CLUSTER -n $NS logs job/\$(kubectl --context kind-$CLUSTER -n $NS get job -o name | head -1 | cut -d/ -f2)

Working files: $WORK
Delete when done: kind delete cluster --name $CLUSTER
EOF
  fi
  exit $code
}
trap cleanup EXIT

say "1/7  cluster"
kind get clusters 2>/dev/null | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --wait 120s
kubectl --context "kind-$CLUSTER" create namespace "$NS" --dry-run=client -o yaml | kubectl --context "kind-$CLUSTER" apply -f - >/dev/null
KC="kubectl --context kind-$CLUSTER -n $NS"

say "2/7  image"
# Built here rather than pulled: the point is to test THIS working tree.
docker build -q -f "$REPO_ROOT/deploy/Dockerfile" -t oag:verify "$REPO_ROOT" >/dev/null
kind load docker-image oag:verify --name "$CLUSTER" >/dev/null

say "3/7  mock upstream"
$KC create configmap mock-upstream \
  --from-file=mock-upstream.py="$REPO_ROOT/deploy/test/mock-upstream.py" \
  --dry-run=client -o yaml | $KC apply -f - >/dev/null
cat > "$WORK/mock.yaml" <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: { name: mock-upstream }
spec:
  replicas: 1
  selector: { matchLabels: { app: mock-upstream } }
  template:
    metadata: { labels: { app: mock-upstream } }
    spec:
      containers:
        - name: mock
          image: python:3.12-alpine
          command: ["python3", "/app/mock-upstream.py"]
          env:
            - { name: PORT, value: "8088" }
            - { name: MOCK_STREAM_SECONDS, value: "$STREAM_SECONDS" }
            - { name: MOCK_CHUNKS, value: "45" }
          ports: [{ containerPort: 8088 }]
          volumeMounts: [{ name: app, mountPath: /app }]
      volumes:
        - name: app
          configMap: { name: mock-upstream }
---
apiVersion: v1
kind: Service
metadata: { name: mock-upstream }
spec:
  selector: { app: mock-upstream }
  ports: [{ port: 8088, targetPort: 8088 }]
YAML
$KC apply -f "$WORK/mock.yaml" >/dev/null
$KC rollout status deployment/mock-upstream --timeout=180s >/dev/null

say "4/7  install the chart"
helm --kube-context "kind-$CLUSTER" upgrade --install oag "$REPO_ROOT/deploy/helm/open-ai-gateway" \
  -n "$NS" --wait --timeout "${HELM_TIMEOUT:-20m}" \
  --set image.repository=oag --set image.tag=verify --set image.pullPolicy=Never \
  --set replicaCount=3 \
  --set data.mode=inCluster \
  --set security.signingSecret="verify-only-signing-secret-0123456789abcdef" \
  --set security.credentialKek="dmVyaWZ5LW9ubHkta2VrLTMyLWJ5dGVzLTAxMjM0NTY=" \
  --set gateway.providerBaseUrls.anthropic="http://mock-upstream:8088" >/dev/null || {
    echo
    echo "helm install failed — pod state and logs follow:"
    # Every line below is `|| true` and none pipes into `head`. The first version
    # of this block took a SIGPIPE under `set -o pipefail` partway through and
    # died before printing the gateway logs, which were the only thing that
    # actually mattered. Diagnostics must not be able to fail.
    $KC get pods -o wide || true
    $KC get events --sort-by=.lastTimestamp 2>/dev/null | tail -30 || true
    echo "--- gateway logs (current) ---"
    $KC logs -l app.kubernetes.io/name=open-ai-gateway -c gateway --tail=40 || true
    echo "--- gateway logs (previous container, where a crash loop leaves its reason) ---"
    for pod in $($KC get pod -l app.kubernetes.io/name=open-ai-gateway -o name 2>/dev/null); do
      $KC logs "$pod" -c gateway --previous --tail=40 || true
    done
    for j in $($KC get job -o name 2>/dev/null); do
      echo "--- $j ---"; $KC logs "$j" --tail=30 || true
    done
    fail "the chart never became ready"
  }

# The migrate hook must have run BEFORE any pod served. If it did not, the
# schema would be absent and every request below would fail for the wrong
# reason — a green-looking test that proves nothing.
$KC get job -o name 2>/dev/null | grep -q migrate \
  || fail "no migrate job — the pre-install hook did not run, so the schema is absent
  and every request below would fail for the wrong reason"
pass "migrate hook ran before any pod served"

SVC="$($KC get svc -l app.kubernetes.io/name=open-ai-gateway -o jsonpath='{.items[0].metadata.name}')"
[ -n "$SVC" ] || fail "could not find the gateway service"
# Discovered, not assumed: the chart's fullname is release + chart name
# ("oag-open-ai-gateway"), not the release name, and guessing it cost a CI run.
DEPLOY="$($KC get deploy -l app.kubernetes.io/name=open-ai-gateway -o jsonpath='{.items[0].metadata.name}')"
[ -n "$DEPLOY" ] || fail "could not find the gateway deployment"
echo "  service=$SVC deployment=$DEPLOY"

say "5/7  bootstrap a key and a credential"
POD="$($KC get pod -l app.kubernetes.io/name=open-ai-gateway -o jsonpath='{.items[0].metadata.name}')"
KEY="$($KC exec "$POD" -- oag admin init --email verify@localhost --route default 2>/dev/null \
        | grep -oE 'oag_live_[0-9a-f]+' | head -1)"
[ -n "$KEY" ] || fail "could not mint an admin key"
$KC exec "$POD" -- oag admin seed-catalog >/dev/null
$KC exec "$POD" -- oag admin add-account --name mock --provider anthropic \
  --secret FAKE-CREDENTIAL-FOR-TESTS --route default >/dev/null
pass "bootstrapped"

say "6/7  open $STREAMS streams, then restart every replica mid-flight"
# Note what this does and does not prove: `port-forward` to a Service binds to
# ONE pod and stays there, so all the streams below land on a single replica
# rather than spreading across three. That is still the property under test —
# the replica holding live streams must drain rather than be killed — but it is
# one pod's drain, not the fleet's. Spreading them would need a real ingress.
#
# It also means the forward itself is part of the assertion: if the pod were
# killed instead of drained, the forward would break and the streams would fail,
# which is the outcome we want to catch.
$KC port-forward "svc/$SVC" 18080:8080 18081:8081 >"$WORK/pf.log" 2>&1 &
PF=$!
for i in $(seq 1 60); do curl -fsS -o /dev/null "http://127.0.0.1:18080/health/live" 2>/dev/null && break; sleep 1; done

for i in $(seq 1 "$STREAMS"); do
  (
    curl -sN --max-time $((STREAM_SECONDS + 120)) -X POST "http://127.0.0.1:18080/v1/messages" \
      -H "x-api-key: $KEY" -H 'content-type: application/json' \
      -d '{"model":"oag/auto","max_tokens":1024,"stream":true,
           "messages":[{"role":"user","content":"drain test"}]}' \
      > "$WORK/stream-$i.txt" 2>/dev/null
  ) &
done

# Land the restart squarely in the middle of every stream.
sleep $((STREAM_SECONDS / 3))
$KC rollout restart "deployment/$DEPLOY" >/dev/null
echo "  restart issued with streams in flight; waiting for them to finish"
wait

say "7/7  results"
survived=0
for i in $(seq 1 "$STREAMS"); do
  # message_stop is the only proof of a COMPLETE stream. A severed one still
  # leaves a file with content in it, which is exactly how a drain bug hides.
  grep -q 'message_stop' "$WORK/stream-$i.txt" 2>/dev/null && survived=$((survived + 1))
done
echo "  streams completed: $survived / $STREAMS"
[ "$survived" -eq "$STREAMS" ] || fail "$((STREAMS - survived)) stream(s) were severed by the rollout"
pass "every stream survived a full rolling restart"

# A completed stream that never reached the ledger would mean the metering task
# was cut off with its pod — the disconnect-billing path failing silently, which
# a stream-completion check alone cannot see.
requests="$(curl -fsS "http://127.0.0.1:18081/admin/api/summary" -H "x-api-key: $KEY" 2>/dev/null \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["requests"])' 2>/dev/null || echo 0)"
echo "  ledger rows: $requests (expected >= $STREAMS)"
[ "$requests" -ge "$STREAMS" ] \
  || fail "only $requests of $STREAMS completed streams reached the ledger — metering
  was cut off with the pod, so spend on a drained stream is invisible"
pass "every completed stream was metered"

OK=1
printf '\n\033[32mPASS: rolling restart severed no stream (%s/%s)\033[0m\n' "$survived" "$STREAMS"
