variable "name" { type = string }
variable "region" { type = string }
variable "network_id" {
  type        = string
  description = "Self-link of the VPC both services attach to."
}
variable "db_tier" {
  type    = string
  default = "db-custom-2-7680"
}
variable "redis_memory_gb" {
  type    = number
  default = 1
}
variable "highly_available" {
  type    = bool
  default = true
}
variable "deletion_protection" {
  type    = bool
  default = true
}
variable "max_connections" {
  type        = number
  default     = 200
  description = "Must comfortably exceed replicas x database.max_connections."
}
