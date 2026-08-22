# Cloud SQL for PostgreSQL and Memorystore for Redis, both on the private
# network so nothing is reachable from the internet.

terraform {
  required_providers {
    google = { source = "hashicorp/google", version = ">= 5.0" }
    random = { source = "hashicorp/random", version = ">= 3.5" }
  }
}

resource "random_password" "db" {
  length  = 32
  special = false
}

resource "google_sql_database_instance" "this" {
  name             = "${var.name}-pg"
  database_version = "POSTGRES_16"
  region           = var.region

  # A deleted database is not recoverable, and this one holds the credential
  # store. Off in production; the variable exists so a scratch stack can be
  # torn down.
  deletion_protection = var.deletion_protection

  settings {
    tier              = var.db_tier
    availability_type = var.highly_available ? "REGIONAL" : "ZONAL"
    disk_autoresize   = true

    backup_configuration {
      enabled                        = true
      point_in_time_recovery_enabled = true
    }

    ip_configuration {
      # Private IP only. Cloud Run reaches it over Direct VPC egress.
      ipv4_enabled    = false
      private_network = var.network_id
    }

    database_flags {
      # The gateway opens a pool per replica; the default on small tiers is low
      # enough that a modest autoscale exhausts it.
      name  = "max_connections"
      value = tostring(var.max_connections)
    }
  }
}

resource "google_sql_database" "this" {
  name     = "oag"
  instance = google_sql_database_instance.this.name
}

resource "google_sql_user" "this" {
  name     = "oag"
  instance = google_sql_database_instance.this.name
  password = random_password.db.result
}

resource "google_redis_instance" "this" {
  name           = "${var.name}-redis"
  region         = var.region
  tier           = var.highly_available ? "STANDARD_HA" : "BASIC"
  memory_size_gb = var.redis_memory_gb
  redis_version  = "REDIS_7_2"

  authorized_network = var.network_id
  connect_mode       = "PRIVATE_SERVICE_ACCESS"

  # Everything the gateway keeps here is expendable — slots, session pins, the
  # auth cache — so persistence buys little. Losing it costs a burst of
  # database reads, not money or credentials.
  redis_configs = {
    maxmemory-policy = "allkeys-lru"
  }
}
