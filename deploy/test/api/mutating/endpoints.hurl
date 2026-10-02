# Endpoints: upstreams an operator registers.
#
# SELF-CLEANING. It registers one endpoint, `hurl-test`, and removes it at the
# end; the first request clears one a failed run left behind. Its base URL is a
# port nothing listens on, so the check reports no answer, which is correct:
# the point is that the gateway asked and said what happened.
#
# The name is fixed rather than generated because an endpoint's name is at most
# 32 characters of a-z, 0-9, `_` and `-`, which a UUID does not fit.

# ── clear what a failed run left ──
DELETE {{admin_host}}/admin/api/endpoints/hurl-test
Authorization: Bearer {{admin_key}}
HTTP *

# ── register ──
# 201, with the plain platform's default auth and the base URL as the gateway
# will use it: no trailing slash.
POST {{admin_host}}/admin/api/endpoints
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-test",
  "dialect": "openai",
  "platform": "plain",
  "base_url": "http://127.0.0.1:59999/v1/",
  "extra_headers": {"X-Hurl-Run": "1"},
  "display_name": "Hurl test"
}
HTTP 201
[Asserts]
jsonpath "$.name" == "hurl-test"
jsonpath "$.auth" == "bearer"
jsonpath "$.base_url" == "http://127.0.0.1:59999/v1"
jsonpath "$.served" == true
jsonpath "$.accounts" == 0

# ── a name is registered once ──
POST {{admin_host}}/admin/api/endpoints
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-test",
  "dialect": "openai",
  "platform": "plain",
  "base_url": "http://127.0.0.1:59999/v1"
}
HTTP 409

# ── a cloud metadata address is refused ──
POST {{admin_host}}/admin/api/endpoints
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-metadata",
  "dialect": "openai",
  "platform": "plain",
  "base_url": "http://169.254.169.254/latest"
}
HTTP 400
[Asserts]
jsonpath "$.error" contains "link-local or cloud-metadata"

# ── and so is a provider's own host on a plain endpoint ──
# See docs/compliance.md: a "compatible" endpoint must not be a way around the
# rules on the built-in providers.
POST {{admin_host}}/admin/api/endpoints
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-compliance",
  "dialect": "anthropic",
  "platform": "plain",
  "base_url": "https://api.anthropic.com"
}
HTTP 400
[Asserts]
jsonpath "$.error" contains "anthropic.com"

# ── it shows up, with what names it ──
GET {{admin_host}}/admin/api/endpoints
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0

GET {{admin_host}}/admin/api/endpoints/hurl-test
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.extra_headers['X-Hurl-Run']" == "1"
jsonpath "$.models" == 0
jsonpath "$.on_ladder" == 0

# ── change a setting ──
# A field left out is kept; `null` clears one that may be empty.
PATCH {{admin_host}}/admin/api/endpoints/hurl-test
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "display_name": null,
  "discover_models": true
}
HTTP 200
[Asserts]
jsonpath "$.display_name" == null
jsonpath "$.discover_models" == true
jsonpath "$.extra_headers['X-Hurl-Run']" == "1"

# ── but never what it is ──
PATCH {{admin_host}}/admin/api/endpoints/hurl-test
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "dialect": "anthropic"
}
HTTP 400

# ── ask it for its models ──
# 200 whatever it answered. Nothing listens on the port, so: not ok, no status.
POST {{admin_host}}/admin/api/endpoints/hurl-test/check
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.ok" == false
jsonpath "$.url" == "http://127.0.0.1:59999/v1/models"
jsonpath "$.error" startsWith "no answer"

# ── remove it ──
DELETE {{admin_host}}/admin/api/endpoints/hurl-test
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.deleted" == true

GET {{admin_host}}/admin/api/endpoints/hurl-test
Authorization: Bearer {{admin_key}}
HTTP 404
