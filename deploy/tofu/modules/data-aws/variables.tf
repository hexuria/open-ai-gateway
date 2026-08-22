variable "name" { type = string }
variable "vpc_id" { type = string }
variable "private_subnet_ids" { type = list(string) }
variable "client_security_group_id" {
  type        = string
  description = "The security group the gateway tasks run in; only it may reach the data tier."
}
variable "db_instance_class" {
  type    = string
  default = "db.t4g.medium"
}
variable "db_storage_gb" {
  type    = number
  default = 50
}
variable "redis_node_type" {
  type    = string
  default = "cache.t4g.micro"
}
variable "highly_available" {
  type    = bool
  default = true
}
variable "backup_retention_days" {
  type    = number
  default = 7
}
variable "deletion_protection" {
  type    = bool
  default = true
}
variable "tls" {
  type        = bool
  default     = true
  description = "Transit encryption. Makes the URL rediss:// rather than redis://."
}
