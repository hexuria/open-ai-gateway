# The key lifecycle, as one self-cleaning chain: mint → quota → read → revoke.
#
# Safe to run repeatedly. It creates its own key and revokes it at the end, so
# it never touches a key anybody is using — which is the only way a mutating
# file can be run more than once without an operator holding their breath.

# ── mint ──
# The secret comes back exactly once. Only its SHA-256 is stored, so this reply
# is the sole copy in existence and there is no endpoint that can hand it over
# again. Captured into a variable rather than printed for that reason.
POST {{admin_host}}/admin/api/keys
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "principal_email": "dev@localhost",
  "name": "hurl-lifecycle",
  "route": "default"
}
HTTP 200
[Asserts]
jsonpath "$.key" exists
jsonpath "$.id" exists
[Captures]
minted_id: jsonpath "$.id"
minted_key: jsonpath "$.key"

# ── the minted key actually works ──
# Minting a credential that cannot authenticate is a failure worth catching here
# rather than in whatever config it was pasted into.
GET {{host}}/v1/models
Authorization: Bearer {{minted_key}}
HTTP 200

# ── and it is NOT an admin key ──
# Admin authority is opt-in at creation. A key minted without `--admin` reaching
# the admin API would make every SDK config an admin credential.
GET {{admin_host}}/admin/api/summary
Authorization: Bearer {{minted_key}}
HTTP 403

# ── set a quota ──
# A string, not a number: these are money and go through `Decimal`, and JSON
# numbers are binary floating point.
PATCH {{admin_host}}/admin/api/keys/{{minted_id}}/quota
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "quota_usd": "5.00" }
HTTP 200

# ── read its spend ──
GET {{admin_host}}/admin/api/keys/{{minted_id}}/usage
Authorization: Bearer {{admin_key}}
HTTP 200

# ── and its spend per model, in points ──
GET {{admin_host}}/admin/api/keys/{{minted_id}}/usage/models
Authorization: Bearer {{admin_key}}
HTTP 200

# ── clear the quota ──
# `null` clears rather than setting zero. Zero would be a cap of nothing, which
# is a very different instruction from "uncapped".
PATCH {{admin_host}}/admin/api/keys/{{minted_id}}/quota
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "quota_usd": null }
HTTP 200

# ── revoke ──
POST {{admin_host}}/admin/api/keys/{{minted_id}}/revoke
Authorization: Bearer {{admin_key}}
HTTP 200

# ── and it stops working ──
# The row update alone would not be a revocation: every replica caches auth by
# hash, so the shared cache is evicted too. This assertion is what proves the
# eviction happened rather than the row merely changing.
GET {{host}}/v1/models
Authorization: Bearer {{minted_key}}
HTTP 401
