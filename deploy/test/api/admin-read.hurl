# Every admin read, on the admin listener (:29081 in dev), with an admin key.
#
# Nothing here changes anything — see `mutating/` for the rest of the admin API.
# Ids are CAPTURED from earlier responses rather than hardcoded, so this file
# works against any deployment instead of only the one it was written on.

# ── the dashboard's headline ──
# `saved_usd` is the number this product exists to state: SUM(counterfactual −
# cost) over the window. On a flat-rate seat cost is zero, so the saving is the
# bill the subscription displaced.
GET {{admin_host}}/admin/api/summary
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.window.period" exists
jsonpath "$.window.label" exists
jsonpath "$.requests" >= 0
jsonpath "$.spent_usd" exists
jsonpath "$.counterfactual_usd" exists

# The window is selectable, and the savings figure is meaningless without saying
# over what. Note `days` is its own parameter rather than a `period` value —
# `period=days` is rejected, which is the kind of thing only a real request
# tells you.
GET {{admin_host}}/admin/api/summary?days=7
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.window.label" == "last 7 days"

# Calendar months, which is what a bill is denominated in.
GET {{admin_host}}/admin/api/summary?period=month
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.window.period" == "month"

GET {{admin_host}}/admin/api/summary?period=last-month
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.window.period" == "last-month"

# An unknown period is refused rather than quietly defaulted: a report over a
# window nobody asked for is worse than an error.
GET {{admin_host}}/admin/api/summary?period=fortnight
Authorization: Bearer {{admin_key}}
HTTP 400

# ── upstream credentials ──
# `state` is the one to read: `ready` is not the same as `schedulable`, because a
# seat can be enabled and still benched by a cooldown or a spent window, and
# `reason` says which.
GET {{admin_host}}/admin/api/accounts
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].id" exists
jsonpath "$[0].provider" exists
jsonpath "$[0].kind" exists
jsonpath "$[0].state" exists
[Captures]
account_id: jsonpath "$[0].id"

# ── routes and their ladders ──
GET {{admin_host}}/admin/api/routes
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].name" exists
jsonpath "$[0].mode" exists
jsonpath "$[0].tiers" exists

# ── what this gateway can talk to at all ──
# Includes WHY a provider offers no subscription: `prohibited_by_terms` with the
# quote, rather than a silent absence somebody later reads as an oversight.
GET {{admin_host}}/admin/api/providers
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].provider" exists
jsonpath "$[0].dialect" exists
jsonpath "$[0].credential_kinds" exists

# ── inbound keys ──
# Prefix only. The secret is shown once at creation and only its SHA-256 is
# stored, so there is nothing here to leak.
GET {{admin_host}}/admin/api/keys
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].key_prefix" exists
jsonpath "$[0].active" exists
[Captures]
key_id: jsonpath "$[0].id"
principal_email: jsonpath "$[0].principal"

# ── the ledger, newest first ──
GET {{admin_host}}/admin/api/usage
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" exists

# ── one key's spend ──
GET {{admin_host}}/admin/api/keys/{{key_id}}/usage
Authorization: Bearer {{admin_key}}
HTTP 200

# ── one key's spend, per model, in points ──
# What a partner service reads to enforce a member's limits in its own currency.
GET {{admin_host}}/admin/api/keys/{{key_id}}/usage/models
Authorization: Bearer {{admin_key}}
HTTP 200

# ── one principal's spend ──
GET {{admin_host}}/admin/api/principals/{{principal_email}}/usage
Authorization: Bearer {{admin_key}}
HTTP 200

# ── the catalogue ──
# Wrapped in an object, unlike most of the admin lists. `display_label` is the
# operator's name and `derived_label` is the fallback — kept separate so an
# unnamed model follows the provider's own spelling forever rather than being
# backfilled with today's derivation.
GET {{admin_host}}/admin/api/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.models" count > 0
jsonpath "$.models[0].id" exists
jsonpath "$.models[0].upstream_name" exists
jsonpath "$.models[0].derived_label" exists

# ── partner services ──
# Empty on a deployment that has none, which is a truthful answer rather than an
# error.
GET {{admin_host}}/admin/api/services
Authorization: Bearer {{admin_key}}
HTTP 200

# ── admin reads need an admin key ──
GET {{admin_host}}/admin/api/summary
Authorization: Bearer {{api_key}}
HTTP 403

GET {{admin_host}}/admin/api/summary
HTTP 401
