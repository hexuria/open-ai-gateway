# The user and database names are interpolated rather than hardcoded, and that
# is the point: without these references `google_sql_user.this` and
# `google_sql_database.this` are graph leaves that nothing waits on, so the
# migrate job can be executed before the role it connects as exists.
output "database_url" {
  value     = "postgres://${google_sql_user.this.name}:${random_password.db.result}@${google_sql_database_instance.this.private_ip_address}:5432/${google_sql_database.this.name}"
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
