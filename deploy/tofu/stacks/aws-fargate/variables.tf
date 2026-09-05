variable "name" {
  type    = string
  default = "open-ai-gateway"
}

variable "region" {
  type = string
}

variable "image" {
  type        = string
  description = "e.g. ghcr.io/hexuria/open-ai-gateway:0.1.0"
}

# Bring your own network. Creating a VPC here would make this stack the owner
# of shared infrastructure it has no business owning, and most organisations
# already have one they want this to live in.
variable "vpc_id" {
  type = string
}

variable "public_subnet_ids" {
  type        = list(string)
  description = "Load balancer subnets. Ignored when `internal` is true, but the ALB still needs two AZs."
}

variable "private_subnet_ids" {
  type        = list(string)
  description = "Task and data subnets. Two AZs minimum."
}

variable "data_mode" {
  type        = string
  default     = "managed"
  description = "managed = RDS + ElastiCache; neutral = Neon + Upstash URLs."
  validation {
    condition     = contains(["managed", "neutral"], var.data_mode)
    error_message = "data_mode must be \"managed\" or \"neutral\"."
  }
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

variable "certificate_arn" {
  type        = string
  default     = ""
  description = "ACM cert for HTTPS on the ALB. Empty serves plain HTTP, which is only defensible behind Cloudflare or on a private network."
}

# The module refuses to plan a plaintext listener unless told, in writing,
# that something in front terminates TLS. This is the way to tell it. Without
# this pass-through the refusal had no override at all from the stack, and a
# certificate-free deployment — the one the `certificate_arn` description
# calls defensible behind Cloudflare or on a private network — could not plan.
variable "allow_plaintext_listener" {
  type        = bool
  default     = false
  description = "Permit an HTTP-only ALB listener. Only for a deployment that terminates TLS in front of the ALB."
}

variable "internal" {
  type        = bool
  default     = true
  description = "Internal ALB. This is an internal gateway; defaulting to internet-facing would be the wrong default to get wrong."
}

variable "allowed_cidrs" {
  type        = list(string)
  default     = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
  description = "Who may reach the load balancer. Defaults to RFC1918 so an accidental public ALB is still not an open one."
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

variable "desired_count" {
  type    = number
  default = 2
}

variable "min_count" {
  type    = number
  default = 2
}

variable "max_count" {
  type    = number
  default = 10
}

variable "highly_available" {
  type    = bool
  default = false
}

variable "deletion_protection" {
  type    = bool
  default = true
}

variable "cloudflare_zone_id" {
  type    = string
  default = ""
}

variable "hostname" {
  type    = string
  default = ""
}

variable "tags" {
  type    = map(string)
  default = {}
}

variable "run_migrations" {
  type        = bool
  default     = true
  description = <<-EOT
    Run `oag migrate` as part of the deployment. Leave this true.

    Set it false to deploy while running a long migration out of band, or to
    skip the step during an incident. Rolling back does NOT require it: the
    migrator runs with ignore_missing(true), so an older binary migrates
    happily against a schema a newer release already applied.
  EOT
}
