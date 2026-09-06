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

  # Premium is required for a private endpoint, so `redis_private` decides the
  # family and SKU rather than leaving an operator to discover the coupling from
  # an apply error. Basic/Standard have no VNet integration at all.
  capacity = var.redis_capacity
  family   = var.redis_private ? "P" : var.redis_family
  sku_name = var.redis_private ? "Premium" : var.redis_sku

  # TLS only. The gateway keeps session pins and the auth cache here; neither is
  # money, but both describe who is talking to what.
  non_ssl_port_enabled = false
  minimum_tls_version  = "1.2"

  # Reachable from the internet unless `redis_private` says otherwise.
  #
  # This defaulted to `true` with no private endpoint while the stack header
  # said both stores are reached privately over the VNet — so anyone holding the
  # access key read the auth cache from anywhere. Postgres beside it has been
  # private since it was written (`public_network_access_enabled = false`
  # above), which is what made the header read as true of both.
  #
  # A variable rather than a flat `false`, because turning it off forces Premium
  # and that is a real cost decision an operator has to make deliberately. It
  # defaults to `false` so no existing deployment changes shape or price on an
  # upgrade; `docs/01-deployment.md` says which way to set it and why.
  public_network_access_enabled = !var.redis_private

  redis_configuration {
    maxmemory_policy = "allkeys-lru"
  }
}

# The private endpoint, when the cache is private.
#
# Without this, `public_network_access_enabled = false` would leave the cache
# reachable by nothing at all — a correct-looking apply that breaks every
# replica. The two belong together and are created together.
resource "azurerm_private_endpoint" "redis" {
  count = var.redis_private ? 1 : 0

  lifecycle {
    precondition {
      condition     = var.private_endpoint_subnet_id != ""
      error_message = "redis_private = true needs private_endpoint_subnet_id: a private cache with nowhere to attach is reachable by nothing."
    }
  }

  name                = "${var.name}-redis-pe"
  resource_group_name = var.resource_group_name
  location            = var.location
  subnet_id           = var.private_endpoint_subnet_id

  private_service_connection {
    name                           = "${var.name}-redis-psc"
    private_connection_resource_id = azurerm_redis_cache.this.id
    subresource_names              = ["redisCache"]
    is_manual_connection           = false
  }
}
