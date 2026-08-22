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
