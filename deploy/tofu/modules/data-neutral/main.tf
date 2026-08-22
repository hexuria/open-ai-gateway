# Cloud-neutral data: Neon for Postgres, Upstash for Redis, or anything else
# that speaks the same wire protocols.
#
# This module deliberately provisions nothing. Both vendors are a few clicks or
# an API call, their Terraform providers are third-party and move faster than
# this repository will, and the gateway only ever needed a URL. Taking the URLs
# as input is what makes the data tier genuinely portable: the compute stack
# below can move between clouds without the database moving with it.
#
# It does check two things that are easy to get wrong and expensive to discover
# in production.

terraform {}

locals {
  # Neon gives you two hostnames. The direct one caps out at a low connection
  # count; the pooled one (`-pooler`) fronts PgBouncer. The gateway opens a pool
  # per replica, so with autoscaling the direct endpoint runs out of connections
  # under exactly the load you deployed it for.
  neon_direct = can(regex("neon\\.tech", var.database_url)) && !can(regex("-pooler", var.database_url))

  # Upstash and Neon both require TLS. A URL without it either fails to connect
  # or, worse, silently downgrades.
  db_insecure    = !can(regex("sslmode=(require|verify-full|verify-ca)", var.database_url))
  redis_insecure = can(regex("upstash\\.io", var.redis_url)) && !startswith(var.redis_url, "rediss://")
}

resource "terraform_data" "preflight" {
  lifecycle {
    precondition {
      condition     = !local.neon_direct
      error_message = "This looks like a Neon direct endpoint. Use the pooled one (the host containing '-pooler'): the gateway opens a connection pool per replica, and the direct endpoint runs out of connections under autoscaling."
    }
    precondition {
      condition     = !local.db_insecure
      error_message = "database_url has no sslmode. Add ?sslmode=require — this connection carries the credential store."
    }
    precondition {
      condition     = !local.redis_insecure
      error_message = "This looks like an Upstash URL without TLS. Use rediss:// rather than redis://."
    }
  }
}
