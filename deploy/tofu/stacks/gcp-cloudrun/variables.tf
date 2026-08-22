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
  description = "e.g. ghcr.io/hexuria/open-ai-gateway:0.1.0"
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

    Set it false for exactly one apply when ROLLING BACK to an older image:
    sqlx runs with ignore_missing = false, so an older binary's migrate fails
    with VersionMissing once a newer migration has been applied. Roll the image
    back and set this false in the same apply.
  EOT
}
