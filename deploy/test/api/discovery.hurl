# Model discovery: what THIS key may actually ask for.
#
# Not a catalogue dump. The answer is the intersection of the route's ladder, the
# key's floor, and what each live credential says it serves — so two keys on one
# gateway can and should get different lists.

# ── the listing a client populates its picker from ──
GET {{host}}/v1/models
Authorization: Bearer {{api_key}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count > 0
# Every entry carries both dialects' required fields on one object, because the
# caller's SDK is not knowable from the request: `extract_key` accepts all three
# header spellings, so auth carries no dialect signal.
jsonpath "$.data[0].id" exists
jsonpath "$.data[0].object" == "model"
jsonpath "$.data[0].owned_by" exists
jsonpath "$.data[0].type" == "model"
jsonpath "$.data[0].display_name" exists
# No pricing, ever. That is the organisation's cost data and it stays on the
# admin listener where it already lives.
body not contains "per_mtok"

# ── the same surface without the version prefix ──
# Several SDKs drop it when given a custom base URL. Same handler, so a
# difference here would mean the router, not the handler.
GET {{host}}/models
Authorization: Bearer {{api_key}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count > 0

# ── why the picker looks the way it does ──
# The half of the reply that explains an EMPTY list, which a picker cannot show
# and a support ticket always needs: budget pressure, and per provider whether
# it is serving and why not.
GET {{host}}/v1/models
Authorization: Bearer {{api_key}}
HTTP 200
[Asserts]
jsonpath "$.oag.mode" exists
jsonpath "$.oag.budget.pressure" exists
jsonpath "$.oag.providers" count > 0
jsonpath "$.oag.providers[0].provider" exists
jsonpath "$.oag.providers[0].serving" exists
jsonpath "$.oag.providers[0].reason" exists

# ── Claude Code's discovery filter ──
# The CLI drops every id not matching /^(claude|anthropic)/i, so a gateway
# serving `xai/grok-4.6` populates an EMPTY picker with nothing saying why. This
# query parameter previews what the CLI would cache before an operator turns the
# config flag on for everyone.
GET {{host}}/v1/models?claude_code=1
Authorization: Bearer {{api_key}}
HTTP 200
[Asserts]
# The twins are additive: the canonical ids are all still there.
jsonpath "$.oag.claude_code_aliases" == true
body contains "anthropic/"

# Forced off, whatever the config says. An unparseable value falls back to the
# configured default rather than 400ing — this is the call a client makes on
# startup and failing it takes the whole session with it.
GET {{host}}/v1/models?claude_code=0
Authorization: Bearer {{api_key}}
HTTP 200
[Asserts]
jsonpath "$.oag.claude_code_aliases" == false

# ── the Gemini-dialect listing ──
# A different envelope on purpose: `models`, not `data`, and `name`/`displayName`
# rather than `id`. Deliberately does not share a renderer with the OpenAI one.
GET {{host}}/v1beta/models
Authorization: Bearer {{api_key}}
HTTP 200
[Asserts]
jsonpath "$.models" count > 0
jsonpath "$.models[0].name" exists

# ── a listing needs a key like everything else ──
# This is an entitlement surface, not a public catalogue: it says what a
# particular caller may spend on.
GET {{host}}/v1/models
HTTP 401
