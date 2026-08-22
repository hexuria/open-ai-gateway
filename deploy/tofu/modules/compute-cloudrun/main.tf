# Cloud Run.
#
# The best fit of the managed container platforms for this workload, for one
# reason: its request timeout goes to 60 minutes, which is the only ceiling in
# the family that comfortably clears a 30-minute streamed completion. Lambda
# stops at 15 and API Gateway at 29 seconds, which rules both out entirely.
#
# Two settings below are load-bearing and neither is the default. Read the
# comments before changing them.

terraform {
  required_providers {
    google = { source = "hashicorp/google", version = ">= 5.0" }
  }
}

resource "google_service_account" "this" {
  account_id   = "${var.name}-sa"
  display_name = "open-ai-gateway"
}

resource "google_cloud_run_v2_service" "this" {
  name     = var.name
  location = var.region
  ingress  = var.ingress

  template {
    service_account = google_service_account.this.email

    # Up to 3600. Must exceed max_stream_duration, or Cloud Run cuts a stream
    # the gateway still considers live.
    timeout = "${var.request_timeout_seconds}s"

    scaling {
      # Not zero. A cold start pays for the model catalog load and an empty auth
      # cache on the first request after every idle period — and with scale to
      # zero that is most requests on a quiet gateway.
      min_instance_count = var.min_instances
      max_instance_count = var.max_instances
    }

    max_instance_request_concurrency = var.concurrency

    dynamic "vpc_access" {
      for_each = var.vpc_subnet == "" ? [] : [1]
      content {
        # Direct VPC egress rather than a Serverless VPC Connector: no extra
        # instances to size, and it is how Cloud Run reaches Cloud SQL and
        # Memorystore on private IPs.
        network_interfaces {
          subnetwork = var.vpc_subnet
        }
        egress = "PRIVATE_RANGES_ONLY"
      }
    }

    containers {
      image = var.image
      args  = ["serve"]

      ports {
        # Cloud Run routes to exactly ONE port. The gateway's two-listener shape
        # cannot survive that, so single-listener mode puts the admin API,
        # /metrics and /health/ready on the same port. They still require an
        # admin key; restrict this service with `ingress` and IAM.
        container_port = 8080
      }

      resources {
        limits = {
          cpu    = var.cpu
          memory = var.memory
        }

        # CPU ALWAYS ALLOCATED. This is not a performance tuning knob.
        #
        # The gateway records what a request cost in a task that runs at the
        # instant the response body completes. With the default (cpu_idle =
        # true) Cloud Run de-allocates CPU the moment a response finishes, and
        # that write may simply never happen — spend the provider has already
        # billed us for, missing from the ledger, with nothing logged because
        # the process was frozen mid-task.
        #
        # It also keeps the catalog refresh and credential-refresh timers
        # running between requests, which cpu_idle would freeze.
        cpu_idle = false

        startup_cpu_boost = true
      }

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
      env {
        name  = "OAG_TELEMETRY__LOG_JSON"
        value = "true"
      }

      dynamic "env" {
        for_each = var.env
        content {
          name  = env.key
          value = env.value
        }
      }

      # Secrets come from Secret Manager, never from plain env values, so they
      # are not visible in the service description or in Terraform state.
      dynamic "env" {
        for_each = var.secret_env
        content {
          name = env.key
          value_source {
            secret_key_ref {
              secret  = env.value
              version = "latest"
            }
          }
        }
      }

      startup_probe {
        http_get { path = "/health/live" }
        period_seconds    = 3
        failure_threshold = 30
      }

      liveness_probe {
        # Liveness, not readiness: a probe that fails during a database outage
        # would make Cloud Run recycle every instance and turn a recoverable
        # incident into a crash loop.
        http_get { path = "/health/live" }
        period_seconds = 30
      }
    }
  }

  traffic {
    type    = "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST"
    percent = 100
  }

  lifecycle {
    precondition {
      condition     = var.request_timeout_seconds > var.max_stream_duration_seconds
      error_message = "request_timeout_seconds must exceed max_stream_duration_seconds, or Cloud Run cuts streams the gateway still considers live."
    }
    precondition {
      condition     = var.request_timeout_seconds <= 3600
      error_message = "Cloud Run caps the request timeout at 3600 seconds."
    }
  }
}

# Migrations as a Cloud Run Job. `oag migrate` is advisory-locked, so running
# it twice or alongside a deploy is safe.
resource "google_cloud_run_v2_job" "migrate" {
  name     = "${var.name}-migrate"
  location = var.region

  template {
    template {
      service_account = google_service_account.this.email
      max_retries     = 3

      dynamic "vpc_access" {
        for_each = var.vpc_subnet == "" ? [] : [1]
        content {
          network_interfaces { subnetwork = var.vpc_subnet }
          egress = "PRIVATE_RANGES_ONLY"
        }
      }

      containers {
        image = var.image
        args  = ["migrate"]

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
            name = env.key
            value_source {
              secret_key_ref {
                secret  = env.value
                version = "latest"
              }
            }
          }
        }
      }
    }
  }
}
