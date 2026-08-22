# Cloudflare as the edge, in front of whichever cloud the gateway runs on.
#
# This is the piece that makes "mix and match" practical: DNS, TLS, WAF and
# DDoS are identical regardless of what is behind them, so moving the compute
# between clouds is a change to one DNS record.
#
# ONE THING WILL BITE YOU, and it is not obvious.
#
# Cloudflare's Proxy Read Timeout is 100 seconds (125 on some plans). It is an
# *inactivity* timeout, not a cap on total duration — a proxied response that
# keeps sending data can run far longer than that. The gateway sends a keepalive
# event every 10 seconds precisely so a model thinking quietly does not look
# like a dead origin and earn a 524.
#
# So: proxied SSE works here **because of** that keepalive. If you ever raise
# `stream_keepalive_interval` past ~90 seconds, streams start dying at the edge
# and the gateway logs will show nothing wrong, because from its side nothing
# is. Only Enterprise can raise the timeout itself.

terraform {
  required_providers {
    # Pinned to v4: v5 turned `rules` from a block into an attribute, so the
    # ruleset resources below do not parse against it.
    cloudflare = { source = "cloudflare/cloudflare", version = "~> 4.0" }
  }
}

resource "cloudflare_record" "this" {
  zone_id = var.zone_id
  name    = var.hostname
  type    = var.origin_is_hostname ? "CNAME" : "A"
  content = var.origin
  proxied = var.proxied
  ttl     = 1

  lifecycle {
    precondition {
      condition     = !var.proxied || var.keepalive_interval_seconds <= 90
      error_message = "With proxied = true, the gateway's stream_keepalive_interval must stay well under Cloudflare's ~100s Proxy Read Timeout. At this value a quiet stream looks like a dead origin and Cloudflare returns 524 — while the gateway's own logs show a perfectly healthy stream."
    }
  }
}

# Turn off anything that would buffer or transform a stream.
resource "cloudflare_ruleset" "streaming" {
  count = var.proxied ? 1 : 0

  zone_id = var.zone_id
  name    = "open-ai-gateway streaming"
  kind    = "zone"
  phase   = "http_config_settings"

  rules {
    action = "set_config"
    # The inference paths only. The dashboard and any static assets should keep
    # normal handling.
    expression  = "(http.host eq \"${var.hostname}\")"
    description = "Do not buffer or rewrite streamed responses"
    enabled     = true

    action_parameters {
      # Rocket Loader and Mirage rewrite HTML; neither belongs anywhere near an
      # event stream, and both have been known to buffer.
      rocket_loader = false
      mirage        = false
    }
  }
}

# Rate limiting at the edge, so obvious abuse never reaches the gateway or its
# database. Deliberately generous: real clients open long-lived streams, and a
# limit tuned for ordinary request/response traffic would cut them off.
resource "cloudflare_ruleset" "rate_limit" {
  count = var.proxied && var.rate_limit_requests_per_minute > 0 ? 1 : 0

  zone_id = var.zone_id
  name    = "open-ai-gateway rate limit"
  kind    = "zone"
  phase   = "http_ratelimit"

  rules {
    action      = "block"
    expression  = "(http.host eq \"${var.hostname}\")"
    description = "Per-IP ceiling ahead of the gateway's own per-key limits"
    enabled     = true

    ratelimit {
      characteristics     = ["ip.src", "cf.colo.id"]
      period              = 60
      requests_per_period = var.rate_limit_requests_per_minute
      mitigation_timeout  = 60
    }
  }
}
