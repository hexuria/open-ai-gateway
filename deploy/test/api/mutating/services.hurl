# Partner services: things this gateway knows about but does not proxy.
#
# LEAVES RESIDUE, one row per run. There is no delete route, and the name is
# UNIQUE — so the name here is generated per run rather than fixed, which is what
# makes the file repeatable at the cost of accumulating rows. Clear them out
# with:
#
#   psql "$OAG_DATABASE__URL" -c "delete from service where name like 'hurl-test-%'"
#
# `check` reports the service unhealthy, which is correct: the base URL points at
# a port nothing is listening on, deliberately.

# ── create ──
# 201, not 200. It made something.
POST {{admin_host}}/admin/api/services
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-test-{{newUuid}}",
  "kind": "sandbox",
  "base_url": "http://127.0.0.1:59999",
  "health_path": "/health"
}
HTTP 201
[Captures]
service_id: jsonpath "$.id"

# ── the kind is validated, not stored ──
# An unrecognised kind is refused with a sentence rather than written through and
# discovered later by whatever tries to use it.
POST {{admin_host}}/admin/api/services
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-test-{{newUuid}}",
  "kind": "firecracker",
  "base_url": "http://127.0.0.1:59999",
  "health_path": "/health"
}
HTTP 400

# ── and so is the URL ──
POST {{admin_host}}/admin/api/services
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-test-{{newUuid}}",
  "kind": "sandbox",
  "base_url": "not-a-url",
  "health_path": "/health"
}
HTTP 400

# ── it shows up ──
GET {{admin_host}}/admin/api/services
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0

# ── edit it ──
PATCH {{admin_host}}/admin/api/services/{{service_id}}
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "name": "hurl-test-{{newUuid}}",
  "kind": "sandbox",
  "base_url": "http://127.0.0.1:59998",
  "health_path": "/healthz"
}
HTTP 200

# ── health-check it ──
# Expected to report unhealthy. The point is that the gateway ASKED and recorded
# the answer, not that the answer was yes.
POST {{admin_host}}/admin/api/services/{{service_id}}/check
Authorization: Bearer {{admin_key}}
HTTP 200

# ── out of, and back into, rotation ──
POST {{admin_host}}/admin/api/services/{{service_id}}/disable
Authorization: Bearer {{admin_key}}
HTTP 200

POST {{admin_host}}/admin/api/services/{{service_id}}/enable
Authorization: Bearer {{admin_key}}
HTTP 200

# ── an id that names nothing ──
POST {{admin_host}}/admin/api/services/00000000-0000-0000-0000-000000000000/check
Authorization: Bearer {{admin_key}}
HTTP 404
