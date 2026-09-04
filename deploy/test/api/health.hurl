# Health and metrics. The only endpoints here that need no key.

# Liveness is on BOTH listeners, deliberately: a process can be up on one and
# wedged on the other, and a liveness probe that only ever asks the listener it
# is co-located with cannot tell you that.
GET {{host}}/health/live
HTTP 200

GET {{admin_host}}/health/live
HTTP 200

# Readiness is admin-only and means something different: the process is up AND
# Postgres and Redis answered. A load balancer should route on this, not on
# liveness — a replica that cannot reach its database is alive and useless.
GET {{admin_host}}/health/ready
HTTP 200

# Prometheus exposition. Unauthenticated by design on the admin listener, which
# is why that listener is meant to be on an internal network.
GET {{admin_host}}/metrics
HTTP 200
[Asserts]
header "content-type" contains "text"
# The counters this deployment's savings story is built on. Present even at
# zero, so a scrape can chart them before any traffic has happened.
body contains "oag_"

# The dashboard SPA. HTML, not JSON — asserted because a 200 carrying the wrong
# content type is how a proxy misconfiguration hides.
GET {{admin_host}}/
HTTP 200
[Asserts]
header "content-type" contains "html"
