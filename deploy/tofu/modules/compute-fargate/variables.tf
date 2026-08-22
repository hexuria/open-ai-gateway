variable "name" { type = string }
variable "region" { type = string }
variable "image" { type = string }
variable "vpc_id" { type = string }
variable "public_subnet_ids" { type = list(string) }
variable "private_subnet_ids" { type = list(string) }
variable "lb_security_group_id" { type = string }
variable "task_security_group_id" { type = string }
variable "execution_role_arn" { type = string }
variable "task_role_arn" { type = string }

variable "env" {
  type    = map(string)
  default = {}
}
variable "secret_env" {
  type        = map(string)
  default     = {}
  description = "env var name -> Secrets Manager or SSM ARN."
}

variable "cpu" {
  type    = string
  default = "512"
}
variable "memory" {
  type    = string
  default = "1024"
}
variable "desired_count" {
  type    = number
  default = 3
}
variable "min_count" {
  type    = number
  default = 3
}
variable "max_count" {
  type    = number
  default = 20
}
variable "target_cpu" {
  type    = number
  default = 70
}

variable "max_stream_duration_seconds" {
  type    = number
  default = 1800
}
variable "idle_timeout_seconds" {
  type    = number
  default = 4000
}
variable "deregistration_delay_seconds" {
  type    = number
  default = 1800
}
variable "health_check_grace_period_seconds" {
  type    = number
  default = 120
}

variable "internal" {
  type    = bool
  default = false
}
variable "certificate_arn" {
  type    = string
  default = ""
}
variable "log_retention_days" {
  type    = number
  default = 30
}

variable "run_migrations" {
  type        = bool
  default     = true
  description = <<-EOT
    Run `oag migrate` as a container the gateway container depends on. Leave true.

    Set it false to deploy while running a long migration out of band, or to
    skip the step during an incident. Rolling back does NOT require it: the
    migrator runs with ignore_missing(true), so an older binary migrates
    happily against a schema a newer release already applied.
  EOT
}

variable "wait_for_steady_state" {
  type        = bool
  default     = true
  description = "Block the apply until the deployment is stable. Turning this off means a failed migration produces a green apply."
}
