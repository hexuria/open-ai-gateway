variable "project_id" { type = string }
variable "region" {
  type    = string
  default = "us-central1"
}
variable "name" {
  type    = string
  default = "open-ai-gateway"
}
variable "image" {
  type        = string
  description = <<-EOT
    The gateway image to run, including a tag.

    `ghcr.io/hexuria/open-ai-gateway:main` is published on every push to the
    default branch, and `:sha-<full sha>` on the same pushes — either is a real
    tag today. A semver tag such as `:0.1.0` exists only once a `v0.1.0` git tag
    has been pushed; the release workflow publishes semver on `v*` and nothing
    else does, so naming one before it is cut lands on ImagePullBackOff.

    Pin a sha for anything you intend to keep: `:main` moves under you.
  EOT
}

variable "data_mode" {
  type    = string
  default = "managed"
  validation {
    condition     = contains(["managed", "neutral"], var.data_mode)
    error_message = "data_mode must be 'managed' (Cloud SQL + Memorystore) or 'neutral' (Neon + Upstash)."
  }
}

variable "network_id" {
  type        = string
  default     = ""
  description = "VPC self-link. Required when data_mode = managed."
}
variable "vpc_subnet" {
  type        = string
  default     = ""
  description = "Subnet self-link for Direct VPC egress. Required when data_mode = managed."
}

variable "neutral_database_url" {
  type      = string
  default   = ""
  sensitive = true
}
variable "neutral_redis_url" {
  type      = string
  default   = ""
  sensitive = true
}

variable "signing_secret" {
  type      = string
  sensitive = true
}
variable "credential_kek" {
  type      = string
  sensitive = true
}

variable "gateway_env" {
  type    = map(string)
  default = {}
}
variable "max_stream_duration_seconds" {
  type    = number
  default = 1800
}
variable "stream_keepalive_interval_seconds" {
  type    = number
  default = 10
}
variable "min_instances" {
  type    = number
  default = 1
}
variable "max_instances" {
  type    = number
  default = 20
}
variable "highly_available" {
  type    = bool
  default = true
}
variable "ingress" {
  type    = string
  default = "INGRESS_TRAFFIC_ALL"
}

# Who may invoke the service at the platform layer. Empty by default, on
# purpose: this stack runs the gateway single-listener, so whoever can invoke
# the service reaches the dashboard, `/metrics` and `/health/ready` as well as
# inference, and only inference authenticates itself. A default of `allUsers`
# published those three to the internet — and, because an IAM member grant is
# additive, added them to every existing deploy whose operator had narrowed
# the invoker by hand. A fresh deploy with nothing in front of it needs
# `["allUsers"]` here or it answers 403 to everyone; set it, knowingly.
# Fronting the service with a Google load balancer or IAP? Name that
# principal and set `ingress` to match; the two are one decision.
variable "invoker_members" {
  type        = list(string)
  default     = []
  description = "Principals granted roles/run.invoker. [\"allUsers\"] for a public service with nothing in front of it."
}

variable "cloudflare_zone_id" {
  type    = string
  default = ""
}
variable "hostname" {
  type    = string
  default = ""
}

variable "run_migrations" {
  type        = bool
  default     = true
  description = <<-EOT
    Run `oag migrate` as part of the apply. Leave this true.

    Set it false to deploy while running a long migration out of band, or to
    skip the step during an incident. Rolling back does NOT require it: the
    migrator runs with ignore_missing(true), so an older binary migrates
    happily against a schema a newer release already applied.
  EOT
}

variable "cloudflare_proxied" {
  type        = bool
  default     = true
  description = <<-EOT
    Whether the Cloudflare record proxies (orange cloud) or resolves through.

    Proxied sends `hostname` as the Host header. Cloud Run routes by Host, so a
    proxied record needs a domain mapping or every request to the custom
    hostname 404s while the run.app URL keeps working — see
    `domain_mapping_verified`.
  EOT
}

variable "domain_mapping_verified" {
  type        = bool
  default     = false
  description = <<-EOT
    Set once a Cloud Run domain mapping exists for `hostname`.

    Creating one requires the domain to be verified in Google Search Console
    first, which is a manual step this stack cannot perform and should not
    appear to. So it asks instead: with `cloudflare_proxied = true` and this
    false, the apply is refused rather than producing a hostname that answers
    404 for every request while reporting success.
  EOT
}

variable "cloudflare_rate_limit_requests_per_minute" {
  type        = number
  default     = 0
  description = <<-EOT
    Per-IP requests per minute blocked at the Cloudflare edge, or 0 for no limit.

    0 by default because the right ceiling depends on the traffic, and a limit
    guessed here would cut off long-lived streams — which is why the module
    calls its own limit "deliberately generous". Ahead of the gateway's own
    per-key limits, this stops obvious abuse before it reaches the database.

    Only takes effect when `cloudflare_zone_id` is set and the record is
    proxied; an unproxied record never sees the traffic.
  EOT
}

variable "deletion_protection" {
  type        = bool
  default     = false
  description = <<-EOT
    Refuse to delete the Cloud Run service.

    `false`, matching the migrate job beside it: the service holds no state and
    is reproducible from this configuration, and the provider's own default of
    `true` makes `terraform destroy` fail at exactly the moment someone is
    tearing down an environment they meant to tear down.

    Set it true for a service you would rather not lose to a misapplied plan.
  EOT
}
