# Points: the currency a partner service enforces limits in.
#
# Nothing here enforces anything. The gateway states the reference price and each
# model's multipliers over it, and whoever is billing reads those and does its
# own arithmetic — which is why this is an admin read surface and not a guard.

# ── the reference price ──
# One row for the whole deployment. Every multiplier is a model's price divided
# by this, so changing it re-values every pool and cap in one move — see
# `mutating/points-reference.hurl`, which is separate for that reason.
GET {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.usd_per_mtok" exists
[Captures]
reference_price: jsonpath "$.usd_per_mtok"

# ── every model's multipliers ──
# Keyed by catalogue id, which is the contract: a client holding `<id>@sub`
# strips the qualifier and looks up the base, so an alias inherits its base
# model's multipliers rather than needing a row of its own.
GET {{admin_host}}/admin/api/points/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].id" exists
jsonpath "$[0].input_x" exists
jsonpath "$[0].output_x" exists
jsonpath "$[0].cache_read_x" exists
jsonpath "$[0].cache_write_x" exists
# What a picker shows next to a model, so a person choosing one can see what it
# will cost them before they spend it.
jsonpath "$[0].shown_x" exists

# An `@sub` id is deliberately NOT a separate row. If one ever appears here, the
# partner's strip-the-suffix lookup has started returning two answers for one
# model and the multipliers have silently diverged.
GET {{admin_host}}/admin/api/points/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
body not contains "@sub"
body not contains "@api"

# ── a member's pool, in one query ──
# The batch read a partner service makes per turn: sum the points spent across
# every key belonging to one person. Capped at 500 keys per call.
GET {{admin_host}}/admin/api/keys
Authorization: Bearer {{admin_key}}
HTTP 200
[Captures]
key_one: jsonpath "$[0].id"

POST {{admin_host}}/admin/api/usage/points
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "keys": ["{{key_one}}"],
  "window": "month"
}
HTTP 200

# A key with no rows is zero, not an error — a member who has not spent yet has
# a pool, and 404ing them would make "new" indistinguishable from "broken".
POST {{admin_host}}/admin/api/usage/points
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "keys": ["00000000-0000-0000-0000-000000000000"],
  "window": "month"
}
HTTP 200

# An empty list is a legitimate question with a legitimate answer.
POST {{admin_host}}/admin/api/usage/points
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "keys": [],
  "window": "month"
}
HTTP 200

# ── points are admin-only ──
GET {{admin_host}}/admin/api/points/models
Authorization: Bearer {{api_key}}
HTTP 403
