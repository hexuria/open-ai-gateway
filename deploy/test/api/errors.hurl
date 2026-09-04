# What a wrong request gets. Half the contract, and the half nobody writes down.
#
# Every status here was observed against a running gateway rather than inferred
# from the code. If one of these starts failing, the contract moved.

# ── no credential at all ──
POST {{host}}/v1/chat/completions
Content-Type: application/json
{ "model": "oag/default", "max_tokens": 8, "messages": [{ "role": "user", "content": "hi" }] }
HTTP 401

# Refused on headers, before the body is read. `route_layer` is what makes that
# true: an unauthenticated POST does not get `max_body_bytes` pulled into the
# process first.
GET {{host}}/v1/models
HTTP 401

# ── a key that is not a key ──
GET {{host}}/v1/models
Authorization: Bearer oag_live_definitely_not_a_real_key
HTTP 401

# ── an admin key on the inference listener: ALLOWED ──
# Admin authority is a property of the key, added on top; it does not remove the
# ability to spend. Documented because it surprises people, and because it means
# an admin key leaked into an SDK config is an inference credential too.
GET {{host}}/v1/models
Authorization: Bearer {{admin_key}}
HTTP 200

# ── an inference key on the admin listener: REFUSED ──
# The inverse is not symmetric, and that is the point. An ordinary key is pasted
# into SDK configs and CI; it must not also be able to disable credentials or
# revoke keys.
GET {{admin_host}}/admin/api/summary
Authorization: Bearer {{api_key}}
HTTP 403

# ── a model that does not exist ──
# 400 with a message naming the route and the fixing command, not a bare 404:
# the caller can act on the first and cannot on the second.
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{ "model": "nope/not-a-model", "max_tokens": 8, "messages": [{ "role": "user", "content": "hi" }] }
HTTP 400
[Asserts]
jsonpath "$.error.type" exists

# ── a model the CREDENTIAL refuses ──
# Different failure, same status. The model is real, catalogued and priced; this
# seat is simply not entitled to it, and the upstream says so in its own words.
# This is the shape that made a whole afternoon confusing: advertised is not
# served, and only the credential knows the difference.
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{ "model": "openai/gpt-5.4", "max_tokens": 8, "messages": [{ "role": "user", "content": "hi" }] }
HTTP 400
[Asserts]
body contains "not supported"

# ── a credential kind nobody holds ──
# 503 rather than 400: the request is well formed and the model is real, there is
# just no credential of that kind on this route. Asking for `@api` where only a
# subscription exists is a deployment fact, not a client mistake.
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{ "model": "{{model}}@api", "max_tokens": 8, "messages": [{ "role": "user", "content": "hi" }] }
HTTP 503

# ── a body that is not JSON ──
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
```
{ not json at all
```
HTTP 400

# ── a path that does not exist ──
GET {{host}}/v1/nothing-here
Authorization: Bearer {{api_key}}
HTTP 404

# ── a spent budget ──
# 402, and it is simulated WITHOUT anything failing: a key minted with a
# minuscule quota is already past its hard stop, so the refusal happens before
# any upstream is contacted. Nothing is spent and no provider is involved.
#
# This is the pattern for every "make the gateway refuse" error — a floor, a
# reserve, a disabled credential — and it is cheaper and more reliable than
# arranging for a real failure. See ERRORS.md.
POST {{admin_host}}/admin/api/keys
Authorization: Bearer {{admin_key}}
Content-Type: application/json
{
  "principal_email": "dev@localhost",
  "name": "hurl-exhausted",
  "route": "default",
  "quota_usd": "0.0000001"
}
HTTP 200
[Captures]
broke_id: jsonpath "$.id"
broke_key: jsonpath "$.key"

POST {{host}}/v1/chat/completions
Authorization: Bearer {{broke_key}}
Content-Type: application/json
{ "model": "oag/default", "max_tokens": 8, "messages": [{ "role": "user", "content": "hi" }] }
HTTP 402
[Asserts]
jsonpath "$.error.type" == "budget_exhausted"
# The message names WHICH cap bound — key, route or principal. Three different
# people fix those three, so a client that only says "out of budget" sends its
# user to the wrong one.
jsonpath "$.error.message" contains "key"

# The listing agrees: an exhausted caller is advertised nothing, because
# `oag/auto` on a key that cannot spend is a 402 on the first turn.
GET {{host}}/v1/models
Authorization: Bearer {{broke_key}}
HTTP 200
[Asserts]
jsonpath "$.oag.budget.pressure" == "exhausted"
jsonpath "$.data" count == 0

# Clean up after itself, so the file stays repeatable.
POST {{admin_host}}/admin/api/keys/{{broke_id}}/revoke
Authorization: Bearer {{admin_key}}
HTTP 200

# ── a body too large: 413, verified but NOT in this file ──
# `count_tokens` carries its own 4 MiB limit, below the server's global one: it
# parses the whole body and stringifies every tool schema, and a real
# count_tokens payload has no reason to be megabytes.
#
# Not asserted here because the fixture would be a five-megabyte blob in the
# repository to test one status code. Reproduce it in one line instead:
#
#   python3 -c "import json,sys; sys.stdout.write(json.dumps(
#     {'model':'oag/default','messages':[{'role':'user','content':'x'*5_000_000}]}))" \
#     | curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
#         -H "Authorization: Bearer $(grep ^api_key= .dev-hurl-vars | cut -d= -f2)" \
#         -H 'content-type: application/json' --data-binary @- \
#         http://127.0.0.1:29080/v1/messages/count_tokens
#
# Observed: 413.
