variable "name" { type = string }
variable "region" { type = string }
variable "image" { type = string }

variable "env" {
  type        = map(string)
  default     = {}
  description = "Plain environment. Never secrets — use secret_env."
}
# The VERSION is pinned, not `latest`. A revision's environment is fixed when
# the revision is created, and Cloud Run only creates one when the template
# changes; `latest` in the template does not change when a new version is
# added, so a rotated secret — Memorystore AUTH being turned on, say — went to
# no running instance until something unrelated forced a deploy, and every
# live instance kept dialling the old URL. Naming the version puts the
# rotation in the template, where it rolls the service.
variable "secret_env" {
  type = map(object({
    secret  = string
    version = string
  }))
  default     = {}
  description = "env var name -> Secret Manager secret id and the version to read."
}

variable "vpc_subnet" {
  type        = string
  default     = ""
  description = "Subnet self-link for Direct VPC egress. Required to reach Cloud SQL or Memorystore on private IPs."
}

variable "request_timeout_seconds" {
  type    = number
  default = 3600
}
variable "max_stream_duration_seconds" {
  type    = number
  default = 1800
}

variable "min_instances" {
  type        = number
  default     = 1
  description = "Not zero: a cold start reloads the catalog and empties the auth cache."
}
variable "max_instances" {
  type    = number
  default = 20
}
variable "concurrency" {
  type        = number
  default     = 80
  description = "Streams are mostly idle waiting on the upstream, so concurrency well above CPU count is correct here."
}

variable "cpu" {
  type    = string
  default = "1"
}
variable "memory" {
  type    = string
  default = "512Mi"
}

variable "ingress" {
  type        = string
  default     = "INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER"
  description = "INGRESS_TRAFFIC_ALL only if clients reach it directly. Behind Cloudflare, keep it restricted and front it with a load balancer."
}

variable "service_account_email" {
  type        = string
  description = "Runtime identity for both the service and the migrate job. Created by the stack, not here, so the Secret Manager grants can be ordered BEFORE this module — otherwise the job executes without permission to read them."
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
