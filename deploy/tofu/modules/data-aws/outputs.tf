output "database_url" {
  value     = "postgres://oag:${random_password.db.result}@${aws_db_instance.this.address}:${aws_db_instance.this.port}/oag?sslmode=require"
  sensitive = true
}

output "redis_url" {
  # The AUTH token travels in the URL, because that is the only place the
  # gateway reads a Redis credential from — a token set on the cache and absent
  # here would lock the gateway out of it, which is a worse outcome than the one
  # the token prevents.
  #
  # `sensitive` already, and now genuinely so.
  value     = var.tls ? "rediss://:${random_password.redis_auth[0].result}@${aws_elasticache_replication_group.this.primary_endpoint_address}:6379" : "redis://${aws_elasticache_replication_group.this.primary_endpoint_address}:6379"
  sensitive = true
}
