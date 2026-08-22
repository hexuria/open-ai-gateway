variable "name" { type = string }
variable "resource_group_name" { type = string }
variable "location" { type = string }
variable "image" { type = string }
variable "log_analytics_workspace_id" { type = string }
variable "infrastructure_subnet_id" { type = string }

variable "env" {
  type    = map(string)
  default = {}
}
variable "secret_env" {
  type        = map(string)
  default     = {}
  description = "env var name -> secret name declared in `secrets`."
}
variable "secrets" {
  type      = map(string)
  default   = {}
  sensitive = true
}

variable "cpu" {
  type    = number
  default = 0.5
}
variable "memory" {
  type    = string
  default = "1Gi"
}
variable "min_replicas" {
  type    = number
  default = 3
}
variable "max_replicas" {
  type    = number
  default = 20
}
variable "max_stream_duration_seconds" {
  type    = number
  default = 1800
}
variable "premium_ingress" {
  type        = bool
  default     = true
  description = "Required for any stream longer than 240 seconds."
}
variable "external" {
  type    = bool
  default = false
}

variable "run_migrations" {
  type        = bool
  default     = true
  description = <<-EOT
    Run `oag migrate` as an init container. Leave this true.

    Set it false to deploy while running a long migration out of band, or to
    skip the step during an incident. Rolling back does NOT require it: the
    migrator runs with ignore_missing(true), so an older binary migrates
    happily against a schema a newer release already applied.
  EOT
}

# Consumption requires total memory in Gi to be exactly twice total CPU. With
# the module defaults of 0.5 / "1Gi" these total 0.75 CPU and 1.5Gi, which
# satisfies it. No precondition guards this: the ceiling depends on the
# workload profile, so a guard would be wrong in both directions, and ARM's own
# rejection is clearer than a wrong guess.
variable "migrate_cpu" {
  type    = number
  default = 0.25
}

variable "migrate_memory" {
  type    = string
  default = "0.5Gi"
}
