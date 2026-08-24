# Deploy to a local "GCP" with floci

[floci-gcp](https://floci.io) is a local GCP emulator that runs Cloud Run
containers **for real** over the Docker socket — so a `terraform apply` of the
`gcp-cloudrun` stack against it genuinely starts the OAG image and serves
traffic, with no cloud account, project, or billing. It is the fastest way to
rehearse the GCP deploy end to end.

```bash
./deploy/floci/deploy.sh
```

That brings up floci + Postgres + Redis, migrates, applies a floci-patched copy
of `stacks/gcp-cloudrun`, and health-checks the OAG Cloud Run service floci
starts. Tear down with `docker compose -f deploy/floci/docker-compose.yml down`.

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
mint an inference key, and POST to `/v1/messages`.

## The plain-API emulator (no Cloud Run)

To rehearse only that the terraform *applies* against a GCP-shaped API — faster,
no container execution — use `deploy/tofu/verify-floci-gcp.sh` instead, which
applies the secret + data layer against a throwaway floci and asserts the
secrets landed.
