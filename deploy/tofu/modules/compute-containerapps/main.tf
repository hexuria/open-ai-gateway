# Azure Container Apps.
#
# One thing to know before choosing this platform: the default ingress request
# timeout is 240 seconds, which cuts a streamed completion four minutes in.
# Raising it to an hour requires **premium ingress** on the environment, which
# is a billed workload profile — so on Azure, supporting long streams is a cost
# decision rather than a configuration flag.
#
# Like Cloud Run, ingress routes to exactly one port, so the gateway runs in
# single-listener mode here.

terraform {
  required_providers {
    azurerm = { source = "hashicorp/azurerm", version = ">= 3.80" }
  }
}

resource "azurerm_container_app_environment" "this" {
  name                       = "${var.name}-env"
  resource_group_name        = var.resource_group_name
  location                   = var.location
  log_analytics_workspace_id = var.log_analytics_workspace_id
  infrastructure_subnet_id   = var.infrastructure_subnet_id

  # Premium ingress. Without a dedicated workload profile the request timeout
  # cannot be raised past 240s, and every stream longer than four minutes dies.
  dynamic "workload_profile" {
    for_each = var.premium_ingress ? [1] : []
    content {
      name                  = "Dedicated-D4"
      workload_profile_type = "D4"
      minimum_count         = 1
      maximum_count         = 3
    }
  }
}

resource "azurerm_container_app" "this" {
  name                         = var.name
  resource_group_name          = var.resource_group_name
  container_app_environment_id = azurerm_container_app_environment.this.id
  revision_mode                = "Single"

  template {
    min_replicas = var.min_replicas
    max_replicas = var.max_replicas

    container {
      name   = "gateway"
      image  = var.image
      cpu    = var.cpu
      memory = var.memory
      args   = ["serve"]

      env {
        name  = "OAG_SERVER__PUBLIC_ADDR"
        value = "0.0.0.0:8080"
      }
      env {
        # Ingress routes to one port, so the admin API, /metrics and
        # /health/ready share it. They still require an admin key.
        name  = "OAG_SERVER__SINGLE_LISTENER"
        value = "true"
      }
      env {
        name  = "OAG_GATEWAY__MAX_STREAM_DURATION"
        value = tostring(var.max_stream_duration_seconds)
      }

      dynamic "env" {
        for_each = var.env
        content {
          name  = env.key
          value = env.value
        }
      }
      dynamic "env" {
        for_each = var.secret_env
        content {
          name        = env.key
          secret_name = env.value
        }
      }

      liveness_probe {
        transport        = "HTTP"
        port             = 8080
        path             = "/health/live"
        interval_seconds = 30
      }

      readiness_probe {
        transport        = "HTTP"
        port             = 8080
        path             = "/health/ready"
        interval_seconds = 5
      }
    }
  }

  ingress {
    external_enabled = var.external
    target_port      = 8080
    transport        = "http"

    traffic_weight {
      latest_revision = true
      percentage      = 100
    }
  }

  dynamic "secret" {
    # Iterating the keys rather than the map: for_each cannot take a sensitive
    # value, and marking the whole map non-sensitive would put every secret in
    # the plan output.
    for_each = nonsensitive(toset(keys(var.secrets)))
    content {
      name  = secret.value
      value = var.secrets[secret.value]
    }
  }

  lifecycle {
    precondition {
      condition     = var.premium_ingress || var.max_stream_duration_seconds <= 240
      error_message = "Container Apps caps the ingress request timeout at 240 seconds without premium ingress. Either set premium_ingress = true (a billed dedicated workload profile) or lower max_stream_duration_seconds to 240 or less."
    }
  }
}
