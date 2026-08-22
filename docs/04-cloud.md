# Cloud deployment

Four clouds, mixed and matched, without four deployments.

## What actually constrains the choice

This gateway holds SSE streams open for up to 30 minutes. That single fact
eliminates most of the serverless world and decides everything else. These are
the measured limits, not recollections:

| Platform | Ceiling | Usable |
|---|---|---|
| **GCP Cloud Run** | 60 min request timeout | ✅ the best fit |
| **AWS ECS Fargate + ALB** | 4000s (~66 min) **idle** timeout | ✅ |
| **Azure Container Apps** | 240s default → 1 hour with **premium ingress** | ✅ at a price |
| **Kubernetes** (GKE/EKS/AKS) | whatever you configure | ✅ most control |
| **Cloudflare** (as edge) | 100–125s **Proxy Read Timeout** | ✅ see below |
| AWS Lambda | 15 min, hard | ❌ |
| AWS API Gateway | 29s | ❌ outright |
| Cloudflare Workers | Rust→WASM; no tokio, no TCP to Postgres | ❌ |

Most of those are **inactivity** timeouts, not caps on total duration. The
gateway emits a keepalive every 10 seconds, and that is the only reason a quiet
30-minute stream survives Cloudflare or an ALB. Raise
`gateway.stream_keepalive_interval` past ~90s and streams start dying at the
edge while the gateway's own logs show nothing wrong — because from its side
nothing is. The Cloudflare module refuses to apply a configuration that would
do this.

Cloud Run's 60 minutes and Azure's 1 hour *are* total caps. The default
`max_stream_duration` of 30 minutes sits under both deliberately.

## The shape

The gateway is one stateless container configured entirely by environment
variables, so the portable seam is **container + a Postgres URL + a Redis URL**.
Everything else is per-cloud glue.

```
                    Cloudflare  (DNS, TLS, WAF — identical everywhere)
                         │
        ┌────────────────┼────────────────┐
        ▼                ▼                ▼
   Cloud Run         Fargate        Container Apps        ── or Kubernetes
        └────────────────┼────────────────┘
                         ▼
              Postgres + Redis, chosen per deployment:
              managed per cloud  |  cloud-neutral  |  in-cluster
```

## Choosing the data tier

Every `data-*` module exposes the same two outputs, so the compute tier does not
know which one is behind it.

| Mode | What it is | Pick it when |
|---|---|---|
| **managed** | Cloud SQL + Memorystore, RDS + ElastiCache, Azure DB + Azure Cache | Lowest latency and best integration. This is also the choice that pins a deployment to its cloud. |
| **neutral** | Neon + Upstash, supplied as URLs | You actually want to mix and match. Compute can move clouds without the data moving with it. |
| **inCluster** | StatefulSets in the same cluster | Dev, or a small internal deployment. Not a substitute for an operator — see below. |

The neutral module checks two things that are painful to discover in
production: that a Neon URL is the **pooled** endpoint (the gateway opens a pool
per replica, and the direct endpoint runs out of connections under exactly the
autoscaling you deployed it for), and that both URLs use TLS.

For self-hosting Postgres in production, run **CloudNativePG** and point
`data.mode=external` at it. The chart's in-cluster StatefulSet is a single
instance with a PVC: no failover, no point-in-time recovery, no pooling. It is
honest about being a starting point.

## Kubernetes

```bash
helm install oag deploy/helm/open-ai-gateway \
  --namespace oag --create-namespace \
  --set image.repository=ghcr.io/hexuria/open-ai-gateway \
  --set security.signingSecret="$(openssl rand -base64 48)" \
  --set security.credentialKek="$(openssl rand -base64 32)" \
  --set data.mode=external \
  --set data.external.existingSecret=oag-data
```

The chart **refuses to render** rather than deploy something that fails
silently later:

- a `terminationGracePeriodSeconds` below `preStopDelaySeconds +
  maxStreamDurationSeconds`, which would sever live streams on every update
- missing or partial secrets, which would leave replicas rejecting each other's
  tokens
- `data.mode=external` with no URLs
- a `PodDisruptionBudget` that would make node drains hang forever

Two details worth knowing:

**`preStopDelaySeconds`** delays SIGTERM so the endpoint is removed from every
kube-proxy *before* the process stops accepting. Without it, an ordinary deploy
produces a short window of refused connections that look like application
errors.

**Migrations run as a pre-install/pre-upgrade hook**, ordered by weight behind
the secrets (−20) and the in-cluster data (−10). `oag migrate` takes a Postgres
advisory lock, so several replicas or several releases racing is safe.

## OpenTofu

```
deploy/tofu/
  modules/
    data-gcp  data-aws  data-azure  data-neutral    → database_url, redis_url
    compute-cloudrun  compute-fargate  compute-containerapps
    edge-cloudflare
  stacks/
    gcp-cloudrun     (data_mode = managed | neutral)
```

Written for OpenTofu / Terraform ≥ 1.5. Each module carries `precondition`
blocks for the mistakes that do not fail loudly:

- **Cloud Run**: `cpu_idle = false`. This is not a performance knob. The gateway
  writes its ledger row in a task that runs at the instant the response body
  completes, and with the default Cloud Run de-allocates CPU exactly then — so
  the write may simply never happen. Spend the provider already billed for,
  missing from the ledger, with nothing logged because the process was frozen
  mid-task. It also keeps the catalog and credential-refresh timers running.
- **Cloud Run**: routes to one port, so it runs in single-listener mode. The
  admin API keeps its key requirement; restrict the service with `ingress` and
  IAM.
- **ALB**: `least_outstanding_requests`, never round robin. Completions vary by
  two orders of magnitude, so round robin distributes arrivals evenly and load
  very unevenly.
- **ALB**: `deregistration_delay` at least the max stream duration.
- **Container Apps**: premium ingress, or streams die at 240s. On Azure,
  supporting long streams is a billing decision.

## Verified

| | |
|---|---|
| Helm chart | linted; five preflight guards each confirmed to fire; four valid configurations render |
| Kubernetes | deployed to `kind`: migration hook, in-cluster Postgres and Redis, two replicas ready, both listeners answering |
| Request path | streamed through the Service; ledger row exact — `in=1200 cached=18000 out=142`, `$0.004085` against `$0.061275` |
| Rolling update | 12 of 12 long streams survived a `rollout restart` mid-flight |
| OpenTofu | all nine modules and stacks pass `terraform validate` |

Not verified: no `terraform apply` was run against a real cloud account. The
configurations are validated and the reasoning behind each constraint is cited
above, but nothing here has provisioned a real Cloud SQL instance.
