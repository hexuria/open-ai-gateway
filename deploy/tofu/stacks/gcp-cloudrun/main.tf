# GCP: Cloud Run, with the data tier selectable.
#
# The data tier is chosen by `data_mode`, and every option satisfies the same
# two-output contract, so the compute below does not know or care which one is
# in use:
#
#   managed  — Cloud SQL + Memorystore, private IP only. Lowest latency, and
#              the choice that pins this deployment to GCP.
#   neutral  — Neon + Upstash, supplied as URLs. Compute can move clouds
#              without the data moving with it.
#
# Terraform cannot select a module source dynamically, so both are declared and
# `count` picks one. It reads oddly and it is the standard way to do this.

terraform {
  required_version = ">= 1.5"
  required_providers {
    google = { source = "hashicorp/google", version = "~> 7.0" }
    # Pinned to v4: v5 turned `rules` from a block into an attribute, so the
    # ruleset resources below do not parse against it.
    cloudflare = { source = "cloudflare/cloudflare", version = "~> 4.0" }
    time       = { source = "hashicorp/time", version = "~> 0.11" }
  }
}

provider "google" {
  project = var.project_id
  region  = var.region
}

module "data_managed" {
  count  = var.data_mode == "managed" ? 1 : 0
  source = "../../modules/data-gcp"

  name             = var.name
  region           = var.region
  network_id       = var.network_id
  highly_available = var.highly_available
}

module "data_neutral" {
  count  = var.data_mode == "neutral" ? 1 : 0
  source = "../../modules/data-neutral"

  database_url = var.neutral_database_url
  redis_url    = var.neutral_redis_url
}

locals {
  database_url = var.data_mode == "managed" ? module.data_managed[0].database_url : module.data_neutral[0].database_url
  redis_url    = var.data_mode == "managed" ? module.data_managed[0].redis_url : module.data_neutral[0].redis_url

  # Cloud Run reaches a private-IP Cloud SQL or Memorystore over Direct VPC
  # egress. With a neutral data tier there is nothing private to reach, so the
  # subnet is unnecessary.
  vpc_subnet = var.data_mode == "managed" ? var.vpc_subnet : ""
}

# Secrets live in Secret Manager, never in the service description. They ARE in
# this stack's state: `google_secret_manager_secret_version.secret_data` holds
# the value (marked sensitive, which hides it from plans, not from the state
# file). Protect the state backend accordingly, or create the versions out of
# band and pass their names in.
resource "google_secret_manager_secret" "this" {
  for_each  = toset(["database-url", "redis-url", "signing-secret", "credential-kek"])
  secret_id = "${var.name}-${each.key}"
  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_version" "this" {
  for_each = {
    "database-url"   = local.database_url
    "redis-url"      = local.redis_url
    "signing-secret" = var.signing_secret
    "credential-kek" = var.credential_kek
  }
  secret      = google_secret_manager_secret.this[each.key].id
  secret_data = each.value

  # `DISABLE`, not the default `DELETE`.
  #
  # The Cloud Run module pins each secret by version number so that a rotation
  # produces a new revision — which is right, and it means the *previous*
  # revision still references the previous version. Destroying that version on
  # rotation makes rolling back fail with "secret version was destroyed": the
  # rollback is attempted precisely when something has gone wrong, and it is the
  # one moment the old version is needed.
  #
  # Disabled versions are recoverable and cost nothing. A destroyed one is gone.
  deletion_policy = "DISABLE"
}

resource "google_service_account" "gateway" {
  account_id   = "${var.name}-sa"
  display_name = "open-ai-gateway"
}

# Hoisted out of the module so the IAM grants below can be ordered BEFORE it.
# Without this, applying the change destroys and recreates the account, which
# briefly revokes the running service's access to its own secrets.
moved {
  from = module.gateway.google_service_account.this
  to   = google_service_account.gateway
}

# `depends_on` orders the SetIamPolicy *call*, not the propagation behind it.
# Secret Manager bindings are eventually consistent and the migrate execution
# fires seconds later, so a fresh stack can fail PERMISSION_DENIED on its very
# first apply. This plus the job's own retries covers it.
resource "time_sleep" "iam_propagation" {
  depends_on      = [google_secret_manager_secret_iam_member.read]
  create_duration = "30s"
}

module "gateway" {
  source = "../../modules/compute-cloudrun"

  name       = var.name
  region     = var.region
  image      = var.image
  vpc_subnet = local.vpc_subnet

  # Secret AND version. The version is what rolls the service when a value
  # changes: the data module composes the Memorystore AUTH string into the
  # Redis URL, so turning AUTH on writes a new version, and a template that
  # read `latest` would leave every running instance on the old, now
  # password-less URL until something else forced a revision.
  secret_env = {
    for key, env in {
      "database-url"   = "OAG_DATABASE__URL"
      "redis-url"      = "OAG_REDIS__URL"
      "signing-secret" = "OAG_SECURITY__SIGNING_SECRET"
      "credential-kek" = "OAG_SECURITY__CREDENTIAL_KEK"
    } :
    env => {
      secret  = google_secret_manager_secret.this[key].secret_id
      version = google_secret_manager_secret_version.this[key].version
    }
  }

  # The guarded number and the deployed number, merged into one.
  #
  # `stream_keepalive_interval_seconds` was passed to the Cloudflare module,
  # which preconditions on it staying under Cloudflare's ~100s Proxy Read
  # Timeout — and to nothing else. The gateway read its own
  # `OAG_GATEWAY__STREAM_KEEPALIVE_INTERVAL` from `gateway_env` or from its
  # default, so the two were independent: an operator who raised the real one
  # got the 524s the guard promised to prevent, with the guard still green.
  #
  # Merged rather than appended, so an explicit `gateway_env` entry still wins —
  # someone setting it by hand has said something more specific than the
  # variable's default, and the precondition still sees the variable, which is
  # the honest limit of what this can promise.
  env = merge(
    { OAG_GATEWAY__STREAM_KEEPALIVE_INTERVAL = tostring(var.stream_keepalive_interval_seconds) },
    var.gateway_env,
  )

  max_stream_duration_seconds = var.max_stream_duration_seconds
  request_timeout_seconds     = var.max_stream_duration_seconds + 300
  min_instances               = var.min_instances
  max_instances               = var.max_instances
  ingress                     = var.ingress

  service_account_email = google_service_account.gateway.email
  run_migrations        = var.run_migrations

  # The secret versions are already an ordering: `secret_env` names each one
  # by number, so the module cannot be built before they exist. IAM
  # propagation is not visible to the graph at all, hence the sleep.
  depends_on = [time_sleep.iam_propagation]
}

# The invoker grant that used to be a manual console step. See
# `invoker_members` for what it publishes and why it is empty by default.
resource "google_cloud_run_v2_service_iam_member" "invoker" {
  for_each = toset(var.invoker_members)
  project  = var.project_id
  location = var.region
  name     = module.gateway.service_name
  role     = "roles/run.invoker"
  member   = each.value
}

resource "google_secret_manager_secret_iam_member" "read" {
  for_each  = google_secret_manager_secret.this
  secret_id = each.value.id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.gateway.email}"
}

module "edge" {
  count  = var.cloudflare_zone_id == "" ? 0 : 1
  source = "../../modules/edge-cloudflare"

  zone_id                    = var.cloudflare_zone_id
  hostname                   = var.hostname
  origin                     = replace(module.gateway.url, "https://", "")
  keepalive_interval_seconds = var.stream_keepalive_interval_seconds

  # Passed, not defaulted. The precondition below guards `cloudflare_proxied`,
  # and a guard on a value the record does not use is the same defect as D11 —
  # the guarded number and the deployed number have to be one number.
  proxied = var.cloudflare_proxied

  # Passed through, because the module's ruleset is gated on it being non-zero
  # and no stack was passing it — so the rate limit could not be turned on from
  # anywhere. A variable no caller can set is not a default, it is dead code.
  rate_limit_requests_per_minute = var.cloudflare_rate_limit_requests_per_minute

  # Cloud Run routes by `Host`, and a proxied Cloudflare record sends the
  # custom hostname rather than the `run.app` one — so without a domain mapping
  # every request to the custom hostname 404s while the `run.app` URL works.
  # The apply succeeds and the output prints the hostname, which is what makes
  # this expensive to diagnose: everything says it worked.
  #
  # A precondition rather than a `google_cloud_run_domain_mapping`, because a
  # mapping requires the domain to be verified in Search Console first — a
  # manual step this stack cannot perform and should not appear to. Refusing
  # with an explanation beats applying something that cannot serve.
  depends_on = [terraform_data.proxied_hostname_needs_a_mapping]
}

resource "terraform_data" "proxied_hostname_needs_a_mapping" {
  count = var.cloudflare_zone_id == "" ? 0 : 1

  lifecycle {
    precondition {
      condition     = !var.cloudflare_proxied || var.domain_mapping_verified
      error_message = "A proxied Cloudflare record sends `hostname` as the Host header, and Cloud Run routes by Host — so it answers 404 for a hostname it has no domain mapping for, while the run.app URL keeps working. Create the mapping (the domain must be verified in Search Console first) and set domain_mapping_verified = true, or set cloudflare_proxied = false and let the record resolve straight through."
    }
  }
}
