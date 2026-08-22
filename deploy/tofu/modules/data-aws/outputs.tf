output "database_url" {
  value     = "postgres://oag:${random_password.db.result}@${aws_db_instance.this.address}:${aws_db_instance.this.port}/oag?sslmode=require"
  sensitive = true
}

output "redis_url" {
  value     = "${var.tls ? "rediss" : "redis"}://${aws_elasticache_replication_group.this.primary_endpoint_address}:6379"
  sensitive = true
}
