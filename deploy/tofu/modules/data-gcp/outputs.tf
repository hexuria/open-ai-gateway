output "database_url" {
  value     = "postgres://oag:${random_password.db.result}@${google_sql_database_instance.this.private_ip_address}:5432/oag"
  sensitive = true
}

output "redis_url" {
  value     = "redis://${google_redis_instance.this.host}:${google_redis_instance.this.port}"
  sensitive = true
}

output "vpc_dependency" {
  description = "Force compute to wait for the data tier."
  value       = google_sql_database_instance.this.id
}
