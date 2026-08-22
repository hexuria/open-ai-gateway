terraform {
  required_providers {
    azurerm = { source = "hashicorp/azurerm", version = ">= 3.80" }
    random  = { source = "hashicorp/random", version = ">= 3.5" }
  }
}

resource "random_password" "db" {
  length  = 32
  special = false
}

resource "azurerm_postgresql_flexible_server" "this" {
  name                = "${var.name}-pg"
  resource_group_name = var.resource_group_name
  location            = var.location
  version             = "16"

  administrator_login    = "oag"
  administrator_password = random_password.db.result

  sku_name   = var.db_sku
  storage_mb = var.db_storage_mb

  # Private only: joined to the delegated subnet, no public endpoint.
  delegated_subnet_id           = var.delegated_subnet_id
  private_dns_zone_id           = var.private_dns_zone_id
  public_network_access_enabled = false

  backup_retention_days        = var.backup_retention_days
  geo_redundant_backup_enabled = var.highly_available

  dynamic "high_availability" {
    for_each = var.highly_available ? [1] : []
    content {
      mode = "ZoneRedundant"
    }
  }
}

resource "azurerm_postgresql_flexible_server_database" "this" {
  name      = "oag"
  server_id = azurerm_postgresql_flexible_server.this.id
  collation = "en_US.utf8"
  charset   = "utf8"
}

resource "azurerm_redis_cache" "this" {
  name                = "${var.name}-redis"
  resource_group_name = var.resource_group_name
  location            = var.location

  capacity = var.redis_capacity
  family   = var.redis_family
  sku_name = var.redis_sku

  # TLS only. The gateway keeps session pins and the auth cache here; neither is
  # money, but both describe who is talking to what.
  non_ssl_port_enabled = false
  minimum_tls_version  = "1.2"

  redis_configuration {
    maxmemory_policy = "allkeys-lru"
  }
}
