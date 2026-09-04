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
    google = { source = "hashicorp/google", version = ">= 5.0" }
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

  secret_env = {
    OAG_DATABASE__URL            = google_secret_manager_secret.this["database-url"].secret_id
    OAG_REDIS__URL               = google_secret_manager_secret.this["redis-url"].secret_id
    OAG_SECURITY__SIGNING_SECRET = google_secret_manager_secret.this["signing-secret"].secret_id
    OAG_SECURITY__CREDENTIAL_KEK = google_secret_manager_secret.this["credential-kek"].secret_id
  }

  env = var.gateway_env

  max_stream_duration_seconds = var.max_stream_duration_seconds
  request_timeout_seconds     = var.max_stream_duration_seconds + 300
  min_instances               = var.min_instances
  max_instances               = var.max_instances
  ingress                     = var.ingress

  service_account_email = google_service_account.gateway.email
  run_migrations        = var.run_migrations

  # Both are load-bearing for migration ordering; neither is redundant. The
  # module only ever depended on the secret *containers* via `.secret_id`, never
  # on their versions — and the job's `secret_key_ref { version = "latest" }`
  # resolves to nothing when no version exists yet.
  depends_on = [
    google_secret_manager_secret_version.this,
    time_sleep.iam_propagation,
  ]
}

# The invoker grant that used to be a manual console step. See
# `invoker_members` for what it means and when to narrow it.
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
}
