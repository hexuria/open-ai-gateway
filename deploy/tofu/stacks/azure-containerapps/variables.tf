variable "name" {
  type    = string
  default = "open-ai-gateway"
}

variable "location" {
  type = string
}

variable "image" {
  type        = string
  description = "e.g. ghcr.io/hexuria/open-ai-gateway:0.1.0"
}

variable "data_mode" {
  type        = string
  default     = "managed"
  description = "managed = Postgres Flexible Server + Azure Cache for Redis; neutral = Neon + Upstash URLs."
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

# Unlike the AWS stack, the network is created here. Postgres Flexible Server
# and Container Apps each require a subnet delegated specifically to them, so a
# bring-your-own-VNet flow would mean asking the operator to pre-create subnets
# with exact delegations — more error-prone than owning them.
variable "address_space" {
  type    = string
  default = "10.42.0.0/16"
}

variable "external" {
  type        = bool
  default     = false
  description = "Expose the container app to the internet. This is an internal gateway; the default is deliberate."
}

variable "gateway_env" {
  type    = map(string)
  default = {}
}

variable "max_stream_duration_seconds" {
  type    = number
  default = 1800
}

variable "premium_ingress" {
  type        = bool
  default     = true
  description = "Container Apps caps the ingress timeout at 240s without a dedicated workload profile, which kills any longer stream. Only set false if max_stream_duration_seconds <= 240."
}

variable "stream_keepalive_interval_seconds" {
  type    = number
  default = 10
}

variable "min_replicas" {
  type    = number
  default = 2
}

variable "max_replicas" {
  type    = number
  default = 10
}

variable "highly_available" {
  type    = bool
  default = false
}

variable "log_retention_days" {
  type    = number
  default = 30
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
    Run `oag migrate` as an init container. Leave this true.

    Set it false for exactly one apply when ROLLING BACK to an older image:
    sqlx runs with ignore_missing = false, so an older binary's migrate fails
    with VersionMissing once a newer migration has been applied.
  EOT
}
