# The points reference price. READ THIS BEFORE RUNNING IT.
#
# Every multiplier on `/admin/api/points/models` is a model's price divided by
# this one number. Changing it therefore RE-VALUES EVERY POOL AND CAP the
# deployment holds, in one move, retroactively as far as anything reading
# multipliers is concerned. It is not a setting; it is a redenomination.
#
# So this file deliberately does NOT experiment. It reads the current value,
# writes THE SAME VALUE back, and checks the multipliers did not move. That
# exercises the whole write path — parse, validate, store, recompute — without
# changing what anybody's credits are worth.
#
# To actually change it, do it knowingly and tell whoever is enforcing limits,
# because their numbers mean something different afterwards.

# ── what it is now ──
GET {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.usd_per_mtok" exists
[Captures]
current_reference: jsonpath "$.usd_per_mtok"

# ── what the multipliers are now ──
GET {{admin_host}}/admin/api/points/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Captures]
first_model_id: jsonpath "$[0].id"
first_model_input_x: jsonpath "$[0].input_x"

# ── write the same value back ──
# A string because it is money and goes through `Decimal`. A JSON number here
# would be binary floating point, which is exactly what a currency must not be.
PUT {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "usd_per_mtok": "{{current_reference}}" }
HTTP 200

# ── nothing moved ──
# The assertion that makes the no-op a test rather than a formality.
GET {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.usd_per_mtok" == "{{current_reference}}"

GET {{admin_host}}/admin/api/points/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$[0].id" == "{{first_model_id}}"
jsonpath "$[0].input_x" == "{{first_model_input_x}}"

# ── a price that is not a price ──
PUT {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "usd_per_mtok": "free" }
HTTP 400

# ── zero would make every multiplier a division by zero ──
PUT {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "usd_per_mtok": "0" }
HTTP 400

# ── negative money ──
PUT {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "usd_per_mtok": "-1" }
HTTP 400

# ── and it is admin-only ──
PUT {{admin_host}}/admin/api/points/reference
Authorization: Bearer {{api_key}}
Content-Type: application/json
{ "usd_per_mtok": "{{current_reference}}" }
HTTP 403
