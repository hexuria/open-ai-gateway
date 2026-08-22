variable "name" { type = string }
variable "region" { type = string }
variable "image" { type = string }

variable "env" {
  type        = map(string)
  default     = {}
  description = "Plain environment. Never secrets — use secret_env."
}
variable "secret_env" {
  type        = map(string)
  default     = {}
  description = "env var name -> Secret Manager secret id."
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

    Set it false for exactly one apply when ROLLING BACK to an older image:
    sqlx runs with ignore_missing = false, so an older binary's migrate fails
    with VersionMissing once a newer migration has been applied. Roll the image
    back and set this false in the same apply.
  EOT
}
