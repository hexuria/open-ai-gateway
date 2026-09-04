# The catalogue: naming a model, and reloading it into memory.
#
# Fully reversible. It renames one model and puts the name back.

GET {{admin_host}}/admin/api/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Captures]
some_model: jsonpath "$.models[0].id"

# ── name it ──
# An id is an ADDRESS — clients send it, rungs name it, the ledger records it —
# so renaming one breaks all three. A label is a name, and renaming a name is
# free. That is why they are two fields rather than one mutable string.
PATCH {{admin_host}}/admin/api/models/{{some_model}}
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "display_label": "Renamed by hurl" }
HTTP 200

GET {{admin_host}}/admin/api/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
body contains "Renamed by hurl"

# ── hand it back to the derived default ──
# `null` CLEARS the column, which is not the same as writing today's derived
# string into it: a cleared row keeps following the provider's spelling, while a
# stored copy goes stale the moment the catalogue is refreshed.
PATCH {{admin_host}}/admin/api/models/{{some_model}}
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "display_label": null }
HTTP 200

GET {{admin_host}}/admin/api/models
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
body not contains "Renamed by hurl"

# ── naming a model that does not exist ──
PATCH {{admin_host}}/admin/api/models/nope/not-a-model
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{ "display_label": "x" }
HTTP 404

# ── reload the in-memory catalogue from the database ──
# The catalogue is held in memory and swapped wholesale, so seeding or repricing
# a model otherwise needs every replica restarted before it takes effect — and
# the replicas give no sign that they are stale.
POST {{admin_host}}/admin/api/catalog/reload
Authorization: Bearer {{admin_key}}
HTTP 200
