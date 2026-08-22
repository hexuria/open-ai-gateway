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
    gcp-cloudrun          (data_mode = managed | neutral)
    aws-fargate           (data_mode = managed | neutral)
    azure-containerapps   (data_mode = managed | neutral)
```

Every stack takes the same `data_mode`, and every data module satisfies the
same two-output contract — `database_url`, `redis_url` — so the compute tier
never knows which one is behind it. `managed` pins the deployment to that
cloud's database; `neutral` (Neon + Upstash) lets compute move without the
data moving with it.

The two clouds differ in who owns the network. AWS brings its own VPC and
subnet IDs, because most organisations already have one this should live in.
Azure creates its own, because Container Apps and Postgres Flexible Server
each require a subnet delegated specifically to them, and hand-built
delegations fail in ways that surface only at apply time.

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

## Migrations

`oag serve` does not migrate on boot — only `oag migrate` does. So every deployment path
needs a step that runs it, ordered after the database and the secrets exist and before the
service takes traffic. Helm has always had one (a pre-install/pre-upgrade hook) and so has
compose; the cloud stacks did not, which meant a successful `apply` could leave a running
gateway pointed at an empty schema.

The mechanism differs per cloud because what the providers expose differs, not by taste:

| | Mechanism | Failed migration fails the apply? |
|---|---|---|
| Cloud Run | The existing job, executed via `run_execution_token`; the service `depends_on` it | Yes — the job is ready only once the execution *completes* |
| Fargate | A `migrate` container in the same task, `dependsOn { condition = "SUCCESS" }` | Yes — via `wait_for_steady_state` and the deployment circuit breaker |
| Container Apps | An `init_container` before the gateway container | **No.** Fail-closed but silent — see below |

Two rejected alternatives are worth recording, because both look right:

- **`aws_ecs_task_execution`** looks like the native way to run a one-off ECS task. It is a
  *data source*, so it is read at **plan** time — a read-only plan on a pull request would
  fire `RunTask` against the production database — and it never calls `DescribeTasks`, so a
  non-zero exit code is never read at all.
- **`azurerm_container_app_job`** creates a job *definition*. azurerm has no way to start an
  execution: no execution resource, no data source, and `manual_trigger_config` carries only
  parallelism and completion count. A job there would be defined and never run — exactly the
  Cloud Run defect being fixed.

**Azure cannot fail the apply, and the docs should not pretend otherwise.** azurerm exposes
no revision health, no `runningState`, and no revision data source, so nothing in the
Terraform graph can read whether the init container succeeded. What Azure does get is
fail-*closed*: the gateway process never starts in a replica whose migration failed, so
serving in front of an unmigrated database is structurally impossible. The stack outputs
`migrate_check` with the command to run after every apply.

That choice costs something real, and it is deliberate. `Db::connect` is lazy and
`/health/live` ignores the database precisely so a replica survives a Postgres failover by
reporting `ready: false` and being routed around. Gating replica start on migrate means a
scale-out replica during a failover crash-loops instead — and `init_container` has no retry
limit. `run_migrations = false` is the lever.

**Rolling back works without a lever**, deliberately. The migrator runs with
`ignore_missing(true)`, so an older binary migrates happily against a schema a newer release
already applied. sqlx defaults that to `false`, which sounds safer and is not: during any
rolling deploy the migration lands while the previous release is still serving — on ECS for
up to the 1800s deregistration delay — so old-binary-against-new-schema is the normal steady
state for tens of minutes anyway. The default would forbid at rollback time precisely what
every release does for half an hour, and on AWS and Azure, where the gateway container
depends on the migrate step, it would leave the rolled-back revision unable to start at all.

`run_migrations = false` remains for deploying while a long migration runs out of band.

**Migrations must be expand/contract.** In all three the migration lands while the previous
release is still serving — on AWS for up to the deregistration delay, which defaults to the
full 1800s stream budget. Every migration has to be readable by the previous binary for at
least that long.

One consequence to expect: the Cloud Run stack's plan is **never clean**. The Cloud Run API
does not return `run_execution_token`, so it re-appears as a diff on every plan. That is what
makes the job execute; do not silence it with `ignore_changes`, and do not use
`terraform plan -detailed-exitcode` as a drift gate on that stack.

## Verified

| | |
|---|---|
| Helm chart | linted; five preflight guards each confirmed to fire; four valid configurations render |
| Kubernetes | deployed to `kind` in CI: migration hook confirmed to run before any pod serves, in-cluster Postgres and Redis, three replicas, both listeners answering |
| Request path | streamed through the Service; ledger row exact — `in=1200 cached=18000 out=142`, `$0.004085` against `$0.061275` |
| Rolling update | 8 of 8 long streams survived a `rollout restart` mid-flight, and all 8 reached the ledger — in CI, on every push (`.github/workflows/k8s.yml`), not by hand |
| OpenTofu | all modules and all three stacks pass `terraform validate` |
| Container image | builds; `oag --version` runs; ELF `e_machine` matches the image architecture |

Also unverified, and specific to the migration step: that the Cloud Run provider surfaces a
FAILED execution as an apply error rather than a ready-but-failed resource (the whole GCP
guarantee rests on it); whether the Azure LRO reports Failed when an init container
crash-loops, and whether `revision_mode = "Single"` holds traffic on the previous revision
until the new one is healthy — if it shifts on provisioning instead, a failed migration on
upgrade is an outage on a green apply; and whether Fargate applies any default
dependency-resolution timeout when `startTimeout` is omitted. Each is one throwaway apply to
settle.

Not verified: no `terraform apply` was run against a real cloud account. The
configurations are validated and the reasoning behind each constraint is cited
above, but nothing here has provisioned a real Cloud SQL instance. `validate`
checks that a configuration is well-formed against the provider schema; it does
not check that a quota exists, that an IAM binding is sufficient, or that two
resources agree at runtime. Expect the first apply to find things.
