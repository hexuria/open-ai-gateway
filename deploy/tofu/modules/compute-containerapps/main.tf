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
    # Floor raised to 5.0: `init_container` — which is how migrations run here —
    # is not present in 3.x, and a loose floor would let an older provider
    # silently drop the migration rather than fail.
    azurerm = { source = "hashicorp/azurerm", version = ">= 5.0" }
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

    # Migrations run as an init container, not as an azurerm_container_app_job:
    # azurerm has no way to *start* a job execution — no execution resource, no
    # execution data source, and `manual_trigger_config` carries only
    # parallelism and replica_completion_count. A job here would be defined and
    # never run, which is exactly the Cloud Run defect this change fixes.
    #
    # Container Apps runs init containers to completion before any app
    # container in the replica, and `oag migrate` exits non-zero on failure, so
    # a gateway serving in front of an unmigrated database is structurally
    # impossible.
    #
    # The cost, accepted deliberately: this inverts a property the gateway was
    # built for. `Db::connect` is lazy and `/health/live` ignores the database
    # precisely so a replica survives a Postgres failover by reporting
    # `ready: false` and being routed around. Gating replica start on migrate
    # means a scale-out replica during a failover crash-loops instead — and
    # `init_container` has no retry limit and no probe knobs, unlike the Helm
    # Job's backoffLimit. `run_migrations = false` is the lever if that bites.
    dynamic "init_container" {
      for_each = var.run_migrations ? [1] : []
      content {
        name   = "migrate"
        image  = var.image
        args   = ["migrate"]
        cpu    = var.migrate_cpu
        memory = var.migrate_memory

        # The SAME environment as the gateway container, literals included.
        # `Config::validate` runs before the subcommand match, so a migrate
        # container given only the database URL exits 1 on config validation
        # and every replica crash-loops with an error that looks nothing like
        # a migration failure.
        env {
          name  = "OAG_SERVER__PUBLIC_ADDR"
          value = "0.0.0.0:8080"
        }
        env {
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
      }
    }

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

  timeouts {
    create = "40m"
    update = "40m"
  }

  lifecycle {
    precondition {
      condition     = var.premium_ingress || var.max_stream_duration_seconds <= 240
      error_message = "Container Apps caps the ingress request timeout at 240 seconds without premium ingress. Either set premium_ingress = true (a billed dedicated workload profile) or lower max_stream_duration_seconds to 240 or less."
    }
  }
}
