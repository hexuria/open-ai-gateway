output "database_url" {
  value     = "postgres://oag:${random_password.db.result}@${azurerm_postgresql_flexible_server.this.fqdn}:5432/oag?sslmode=require"
  sensitive = true
}

output "redis_url" {
  value     = "rediss://:${azurerm_redis_cache.this.primary_access_key}@${azurerm_redis_cache.this.hostname}:${azurerm_redis_cache.this.ssl_port}"
  sensitive = true
}
