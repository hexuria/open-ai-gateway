# Azure: Container Apps, with the data tier selectable.
#
#   managed  — Postgres Flexible Server + Azure Cache for Redis, both reached
#              privately over the VNet.
#   neutral  — Neon + Upstash, supplied as URLs, so compute can move clouds
#              without the data moving with it.
#
# Terraform cannot select a module source dynamically, so both are declared and
# `count` picks one. It reads oddly and it is the standard way to do this.
#
# The thing to know about this platform: Container Apps caps the ingress
# request timeout at 240 seconds unless the environment has a dedicated
# workload profile. Four minutes kills most real streaming sessions, so
# `premium_ingress` defaults on and the compute module refuses the combination
# that would silently truncate streams.

terraform {
  required_version = ">= 1.5"
  required_providers {
    # Pinned to a major on purpose: v5 replaced
    # `resource_group_name`/`private_dns_zone_name` on the private DNS zone
    # link with `private_dns_zone_id`, so v4 and v5 configs are not
    # interchangeable and an unpinned constraint silently breaks on upgrade.
    azurerm = { source = "hashicorp/azurerm", version = "~> 5.0" }
    # Pinned to v4: v5 turned `rules` from a block into an attribute, so the
    # ruleset resources in the edge module do not parse against it.
    cloudflare = { source = "cloudflare/cloudflare", version = "~> 4.0" }
  }
}

provider "azurerm" {
  features {}
}

locals {
  managed = var.data_mode == "managed"

  # Container Apps wants /23 for its infrastructure subnet; Postgres Flexible
  # Server wants a subnet of its own that nothing else may join.
  infra_subnet_cidr    = cidrsubnet(var.address_space, 7, 0)
  postgres_subnet_cidr = cidrsubnet(var.address_space, 8, 2)
}

resource "azurerm_resource_group" "this" {
  name     = var.name
  location = var.location
  tags     = var.tags
}

resource "azurerm_virtual_network" "this" {
  name                = "${var.name}-vnet"
  location            = azurerm_resource_group.this.location
  resource_group_name = azurerm_resource_group.this.name
  address_space       = [var.address_space]
  tags                = var.tags
}

resource "azurerm_subnet" "infra" {
  name                 = "${var.name}-infra"
  resource_group_name  = azurerm_resource_group.this.name
  virtual_network_name = azurerm_virtual_network.this.name
  address_prefixes     = [local.infra_subnet_cidr]

  delegation {
    name = "containerapps"
    service_delegation {
      name    = "Microsoft.App/environments"
      actions = ["Microsoft.Network/virtualNetworks/subnets/join/action"]
    }
  }
}

resource "azurerm_subnet" "postgres" {
  count                = local.managed ? 1 : 0
  name                 = "${var.name}-postgres"
  resource_group_name  = azurerm_resource_group.this.name
  virtual_network_name = azurerm_virtual_network.this.name
  address_prefixes     = [local.postgres_subnet_cidr]

  delegation {
    name = "postgres"
    service_delegation {
      name    = "Microsoft.DBforPostgreSQL/flexibleServers"
      actions = ["Microsoft.Network/virtualNetworks/subnets/join/action"]
    }
  }
}

# A VNet-integrated Flexible Server is only resolvable through a private DNS
# zone whose name ends in postgres.database.azure.com. Without the zone and the
# link below the server exists and simply cannot be reached by name.
resource "azurerm_private_dns_zone" "postgres" {
  count               = local.managed ? 1 : 0
  name                = "${var.name}.private.postgres.database.azure.com"
  resource_group_name = azurerm_resource_group.this.name
  tags                = var.tags
}

resource "azurerm_private_dns_zone_virtual_network_link" "postgres" {
  count               = local.managed ? 1 : 0
  name                = "${var.name}-postgres"
  private_dns_zone_id = azurerm_private_dns_zone.postgres[0].id
  virtual_network_id  = azurerm_virtual_network.this.id
  tags                = var.tags
}

resource "azurerm_log_analytics_workspace" "this" {
  name                = "${var.name}-logs"
  location            = azurerm_resource_group.this.location
  resource_group_name = azurerm_resource_group.this.name
  sku                 = "PerGB2018"
  retention_in_days   = var.log_retention_days
  tags                = var.tags
}

module "data_managed" {
  count  = local.managed ? 1 : 0
  source = "../../modules/data-azure"

  name                = var.name
  resource_group_name = azurerm_resource_group.this.name
  location            = azurerm_resource_group.this.location
  delegated_subnet_id = azurerm_subnet.postgres[0].id
  private_dns_zone_id = azurerm_private_dns_zone.postgres[0].id
  highly_available    = var.highly_available

  depends_on = [azurerm_private_dns_zone_virtual_network_link.postgres]
}

module "data_neutral" {
  count  = local.managed ? 0 : 1
  source = "../../modules/data-neutral"

  database_url = var.neutral_database_url
  redis_url    = var.neutral_redis_url
}

locals {
  database_url = local.managed ? module.data_managed[0].database_url : module.data_neutral[0].database_url
  redis_url    = local.managed ? module.data_managed[0].redis_url : module.data_neutral[0].redis_url
}

# Container Apps holds its own secrets, so there is no Key Vault in this path.
# The values still never appear in the container definition — `secret_env` maps
# an env var to a secret name, and the value lives only in the secret.
module "gateway" {
  source = "../../modules/compute-containerapps"

  name                       = var.name
  resource_group_name        = azurerm_resource_group.this.name
  location                   = azurerm_resource_group.this.location
  image                      = var.image
  log_analytics_workspace_id = azurerm_log_analytics_workspace.this.id
  infrastructure_subnet_id   = azurerm_subnet.infra.id

  secrets = {
    "database-url"   = local.database_url
    "redis-url"      = local.redis_url
    "signing-secret" = var.signing_secret
    "credential-kek" = var.credential_kek
  }

  secret_env = {
    OAG_DATABASE__URL            = "database-url"
    OAG_REDIS__URL               = "redis-url"
    OAG_SECURITY__SIGNING_SECRET = "signing-secret"
    OAG_SECURITY__CREDENTIAL_KEK = "credential-kek"
  }

  env = var.gateway_env

  max_stream_duration_seconds = var.max_stream_duration_seconds
  premium_ingress             = var.premium_ingress
  run_migrations              = var.run_migrations
  min_replicas                = var.min_replicas
  max_replicas                = var.max_replicas
  external                    = var.external
}

module "edge" {
  count  = var.cloudflare_zone_id == "" ? 0 : 1
  source = "../../modules/edge-cloudflare"

  zone_id                    = var.cloudflare_zone_id
  hostname                   = var.hostname
  origin                     = module.gateway.fqdn
  keepalive_interval_seconds = var.stream_keepalive_interval_seconds
}
