variable "name" { type = string }
variable "resource_group_name" { type = string }
variable "location" { type = string }
variable "delegated_subnet_id" { type = string }
variable "private_dns_zone_id" { type = string }

variable "db_sku" {
  type    = string
  default = "GP_Standard_D2s_v3"
}
variable "db_storage_mb" {
  type    = number
  default = 65536
}
variable "redis_sku" {
  type    = string
  default = "Standard"
}
variable "redis_family" {
  type    = string
  default = "C"
}
variable "redis_capacity" {
  type    = number
  default = 1
}
variable "highly_available" {
  type    = bool
  default = true
}
variable "backup_retention_days" {
  type    = number
  default = 7
}
