# Principals: the identity a key belongs to and the budget it spends against.
#
# LEAVES RESIDUE. There is no delete route, so the principal this creates stays
# on the deployment. It is named `hurl-test@localhost` so it is obvious in a
# listing, and re-running updates it rather than making another.

# ── create, or update if it exists ──
# Upsert rather than create: a partner service binding an org to a principal
# should not have to know whether it has done so before.
POST {{admin_host}}/admin/api/principals
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "email": "hurl-test@localhost",
  "role": "member"
}
HTTP 200

# Idempotent: the same call again is the same answer, not a conflict.
POST {{admin_host}}/admin/api/principals
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "email": "hurl-test@localhost",
  "role": "member"
}
HTTP 200

# ── a monthly budget ──
# A string because it is money. Budget pressure degrades before it denies: the
# last fifth buys cheap models only, and only past the hard-stop multiple does
# the gateway refuse — a developer who can get no answer at all routes around
# the gateway, and then you have neither the savings nor the visibility.
PATCH {{admin_host}}/admin/api/principals/hurl-test@localhost/budget
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "monthly_budget_usd": "25.00" }
HTTP 200

# ── what they have spent ──
GET {{admin_host}}/admin/api/principals/hurl-test@localhost/usage
Authorization: Bearer {{admin_key}}
HTTP 200

# ── uncapped ──
# `null` clears the cap. Zero would mean "may spend nothing", which is a
# different and much harsher instruction.
PATCH {{admin_host}}/admin/api/principals/hurl-test@localhost/budget
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "monthly_budget_usd": null }
HTTP 200

# ── a principal that does not exist ──
GET {{admin_host}}/admin/api/principals/nobody@nowhere.invalid/usage
Authorization: Bearer {{admin_key}}
HTTP 404
