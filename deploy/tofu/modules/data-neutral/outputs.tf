output "database_url" {
  value      = var.database_url
  sensitive  = true
  depends_on = [terraform_data.preflight]
}
output "redis_url" {
  value      = var.redis_url
  sensitive  = true
  depends_on = [terraform_data.preflight]
}
