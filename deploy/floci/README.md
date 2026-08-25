# Deploy to a local "GCP" with floci

[floci-gcp](https://floci.io) is a local GCP emulator that runs Cloud Run
containers **for real** over the Docker socket — so a `terraform apply` of the
`gcp-cloudrun` stack against it genuinely starts the OAG image and serves
traffic, with no cloud account, project, or billing. It is the fastest way to
rehearse the GCP deploy end to end.

Two modes, differing only in where the database comes from:

| Command | Data tier | Database |
|---|---|---|
| `just floci-up` (`./deploy/floci/deploy.sh`) | neutral | plain Postgres container |
| `just floci-cloudsql` (`./deploy/floci/deploy-cloudsql.sh`) | managed | a real `google_sql_database_instance`, which floci starts as a Postgres 16 container |

Both need Docker and `terraform` or `tofu`, and both deploy a **published
image** — `ghcr.io/hexuria/open-ai-gateway:main` — rather than building your
tree. Point them at a local build with `OAG_IMAGE`. Tear either down with
`just floci-down`, which also removes the containers floci itself spawned;
`docker compose ... down` alone leaves those behind.

```bash
just floci-up              # or ./deploy/floci/deploy.sh
```

That brings up floci + Postgres + Redis, migrates, applies a floci-patched copy
of `stacks/gcp-cloudrun`, and health-checks the OAG Cloud Run service floci
starts.

## Cloud SQL for the database (mirrors GCP more closely)

`just floci-up` uses the **neutral** data tier — plain Postgres in a container.
To rehearse the **managed** tier the way a real GCP deploy runs it, with the
database as **Cloud SQL**:

```bash
just floci-cloudsql        # or ./deploy/floci/deploy-cloudsql.sh
```

floci Docker-backs Cloud SQL for real: `terraform apply` of the `managed` stack
provisions a `google_sql_database_instance`, which floci starts as a Postgres 16
container on the compose network. The gateway connects to it exactly as it would
to Cloud SQL on GCP, and `admin init` writes land there. Two honest caveats, both
because floci is an emulator:

- **Redis stays a container.** floci backs Cloud SQL but not Memorystore, so the
  managed tier's Memorystore is dropped and Redis runs in `docker-compose.yml`.
- **Public IP on the compose network**, not private IP + Direct VPC egress —
  floci has no VPC. The real managed tier keeps Cloud SQL private.

The schema is migrated by a one-off container right after Cloud SQL comes up
(floci runs services, not the Cloud Run migrate job), so the gateway reports
not-ready for a moment and then flips to ready. Tear down with `just floci-down`,
which also removes the Cloud SQL container floci spawned.

## What it proves, and what it doesn't

**Proves:** the deploy applies against GCP's real API shapes, and the gateway
**runs** as a Cloud Run service — floci creates the Secret Manager secrets, the
service account, and the Cloud Run service, then starts the OAG container, which
connects to Postgres + Redis and answers `/health/ready`.

**Does not replace a real deploy.** floci is an emulator, so the harness makes
three floci-specific adjustments to a *throwaway copy* of the stack (the real
stack under `stacks/gcp-cloudrun` is never touched):

1. **Plain env, not Secret Manager `valueSource`** — floci's Cloud Run execution
   does not resolve secret-backed env vars, so the DB URL and secrets are passed
   as plain values.
2. **The migrate Cloud Run *job* is skipped** — floci runs Cloud Run services,
   not jobs; the schema is applied by a one-off `oag migrate` container instead.
3. **The neutral-tier TLS/pooler preflight and the secret-IAM bindings are
   dropped** — meaningless against an emulator that ignores auth and a local
   Postgres with no TLS.

Because of these, a green floci run is a strong *config + runtime* rehearsal,
not proof of the exact production path. The production path (real Secret Manager
injection, the migrate job, IAM) is exercised only by a real deploy —
`deploy/tofu/deploy-gcp.sh` against a GCP project, or `verify-migration-gate.sh`
for the migration gate specifically.

## Sending traffic to it

The Cloud Run service is single-listener (Cloud Run routes one port), reachable
at `<container-ip>:8080` from any container on the `oag-floci_default` network —
the script prints the address and a ready-to-run `oag admin init` for the first
key. From there it is an ordinary gateway: add accounts, set the route ladder,
mint an inference key, and POST to `/v1/messages` — the same sequence as
`docs/07-running-locally.md`, against this database instead of the dev one.

## The plain-API emulator (no Cloud Run)

To rehearse only that the terraform *applies* against a GCP-shaped API — faster,
no container execution, nothing to send a request to — use
`deploy/tofu/verify-floci-gcp.sh` instead, which applies the secret + data layer
against a throwaway floci and asserts the secrets landed. It is the narrower of
the two paths, and predates floci's Cloud Run execution; if something you read
says floci cannot run the gateway, it is describing this script.
