# Credential state: disable, enable, clear a cooldown.
#
# REQUIRES an explicit account id, and that is the safety catch:
#
#   hurl --variables-file .dev-hurl-vars \
#        --variable account_id=<uuid> --test deploy/test/api/mutating/accounts.hurl
#
# Without it hurl fails on the undefined variable rather than picking a
# credential for you. Disabling a seat takes it out of rotation for real, and on
# a deployment whose only serving credential is that seat, a coworker turn
# landing in the window between disable and enable fails for a reason nobody
# will connect to a test run. Pick the id deliberately.

# ── the starting state, so the file can put it back ──
GET {{admin_host}}/admin/api/accounts
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0

# ── an id that names nothing is 404, not a silent success ──
# Checked here because these three verbs return 200 with no body on success, so
# "it worked" and "it did nothing" would otherwise look identical — and a
# disable that quietly matched no row is the kind of thing an operator only
# discovers when the credential they meant to bench serves a request.
POST {{admin_host}}/admin/api/accounts/00000000-0000-0000-0000-000000000000/disable
Authorization: Bearer {{admin_key}}
HTTP 404

POST {{admin_host}}/admin/api/accounts/00000000-0000-0000-0000-000000000000/enable
Authorization: Bearer {{admin_key}}
HTTP 404

POST {{admin_host}}/admin/api/accounts/00000000-0000-0000-0000-000000000000/clear-cooldown
Authorization: Bearer {{admin_key}}
HTTP 404

# ── out of rotation ──
POST {{admin_host}}/admin/api/accounts/{{account_id}}/disable
Authorization: Bearer {{admin_key}}
HTTP 200

# `schedulable` is the scheduler's view and `state` is the human one. A disabled
# credential is an operator decision rather than a transient condition, which is
# why the listing distinguishes it from a cooldown.
GET {{admin_host}}/admin/api/accounts
Authorization: Bearer {{admin_key}}
HTTP 200

# ── back in ──
POST {{admin_host}}/admin/api/accounts/{{account_id}}/enable
Authorization: Bearer {{admin_key}}
HTTP 200

# ── clear a cooldown ──
# A cooldown is set by the breaker after a failure and expires on its own; this
# ends it early, for when the operator knows the upstream is healthy again and
# does not want to wait out a timer.
POST {{admin_host}}/admin/api/accounts/{{account_id}}/clear-cooldown
Authorization: Bearer {{admin_key}}
HTTP 200

# ── restored ──
GET {{admin_host}}/admin/api/accounts
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$" count > 0
