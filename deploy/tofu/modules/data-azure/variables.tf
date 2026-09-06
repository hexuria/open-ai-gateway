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
variable "redis_private" {
  type        = bool
  default     = false
  description = <<-EOT
    Reach the cache over a private endpoint instead of the internet.

    `false` leaves `public_network_access_enabled = true`, which is the shape
    every existing deployment already has — so an upgrade changes nothing. It
    also means anyone holding the access key can read the auth cache from
    anywhere, which is worth knowing rather than discovering: the session pins
    and cached identities in there describe who is talking to what.

    `true` turns public access off and creates a private endpoint in
    `private_endpoint_subnet_id`. It also forces the Premium family, because
    Basic and Standard have no VNet integration — so this is a cost decision,
    which is why it is a variable rather than simply the right answer.

    Postgres beside it is private unconditionally and always has been.
  EOT
}

variable "private_endpoint_subnet_id" {
  type        = string
  default     = ""
  description = "Subnet for the Redis private endpoint. Required when redis_private is true."
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
